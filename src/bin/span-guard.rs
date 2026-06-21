//! `span-guard` — a Claude Code `PreToolUse` hook that redirects large verbatim
//! relocations away from the string-replace editor and toward `span` (§10 of
//! SPEC.md).
//!
//! It reads the `PreToolUse` JSON payload on stdin, inspects an `Edit`/`Write`/
//! `MultiEdit`, and detects the move/duplicate *tell*. The hook has exactly two
//! outcomes, keyed on block size and direction (§10):
//!
//! - large inserted block (≥ `DENY_MIN_LINES`) with a verbatim twin → **deny**,
//!   with an assembled `span` command carrying the discovered source coordinates;
//! - large deleted block (≥ `DELETE_DENY_MIN_LINES`) → **deny** with a move/force
//!   fork — no twin required, since delete-first order hides it (Decision 2);
//! - anything else → **silent allow**.
//!
//! There is no soft "reminder" outcome: a per-edit nudge spends the very context
//! `span` exists to conserve, and the companion CLAUDE.md guidance already carries
//! the proactive "prefer `span`" habit. The hook is the hard backstop, nothing more.
//!
//! **Fail open.** A hook must never break editing. Any malformed input, I/O
//! error, or unrecognized tool results in exit `0` with no output, leaving the
//! normal permission flow untouched.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

// Insertion deny threshold, in logical lines: an inserted block at or above this
// size with a verbatim twin is hard-blocked (§10). Smaller insertions are left
// strictly alone — at that size re-emission is cheap and the false-positive risk
// of acting outweighs the tiny context saved. ~5 is the spec's "large block"
// boundary. With no reminder outcome, this is also the floor below which an
// insertion is never even collected as a candidate (see `push_edit`).
const DENY_MIN_LINES: usize = 5;

// Deletion deny threshold (§10, Decision 2): an editor deletion at or above this
// many lines is hard-denied with the move/force fork. Deliberately *higher* than
// the insertion DENY_MIN_LINES and a separate constant — never reuse 5. Three
// independent pressures push the deletion bar up: (1) base rate — `Edit`→"" is
// the *standard* way to delete a block, so most large deletions are genuine
// deletes, not a move's first half; (2) no twin filter — in delete-first order
// the destination twin isn't written yet, so the deny fires on block size alone,
// without the insertion deny's second precision filter; (3) asymmetric cost — a
// missed delete-first move just falls back to the editor status quo (≈ free),
// while a false deny interrupts a legitimate delete. So bias toward false
// negatives: 10 clears ordinary 5–9 line dead-code removals while still catching
// substantial moves. Tunable later if usage data warrants.
const DELETE_DENY_MIN_LINES: usize = 10;

// Skip files larger than this when scanning for a twin: a relocated *source*
// block lives in ordinary text, and reading large/binary blobs wastes the
// hook's time budget.
const MAX_SCAN_BYTES: u64 = 2 * 1024 * 1024;

// Directory names never worth scanning for a relocation source.
const SKIP_DIRS: [&str; 3] = [".git", "target", "node_modules"];

// Longest guard substring we emit. A guard is a *substring* match (§5), so a
// prefix of a long line is still a valid (if weaker) anchor the model can
// extend; capping keeps the suggested command readable.
const MAX_GUARD_CHARS: usize = 60;

// Placeholder emitted in place of a guard for a blank endpoint line, which
// cannot anchor anything. Named so `unique_anchor` can recognize and skip it
// (a blank neighbor carries no entropy either).
const BLANK_GUARD_HINT: &str = "<pick a distinctive line>";

/// The `PreToolUse` envelope. `tool_input`'s shape varies per tool, so it is
/// kept as a raw `Value` and the few fields we need are pulled out per tool —
/// the boundary-fidelity rule from PRINCIPLES.md (don't narrow an external
/// payload into a lossy struct).
#[derive(Deserialize)]
struct HookInput {
    /// Project root the hook runs against; used to scope the twin scan and to
    /// render paths relative in the suggested command.
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_input: serde_json::Value,
}

/// Whether a candidate block is being added to or removed from a file. The two
/// tells differ in whether a verbatim twin is required to act (`decide`): an
/// insertion needs one (its block is not yet on disk, so any occurrence is the
/// relocation source); a deletion does not (delete-first order hides the twin).
#[derive(Clone, Copy)]
enum Tell {
    /// Text being written (`Write` content, or an `Edit`/`MultiEdit`
    /// `new_string`): the destination half of a move/copy.
    Insertion,
    /// Text being removed (an `Edit`/`MultiEdit` `old_string` → empty): the
    /// source-removal half of a move.
    Deletion,
}

/// One block the edit would add or remove, plus the file it targets. The block
/// is pre-split into blank-trimmed logical lines — the unit twin matching and
/// guard selection both work in.
struct Candidate {
    block: Vec<String>,
    tell: Tell,
    /// The file being edited (the relocation *destination* for an insertion).
    file: Option<String>,
}

/// Split text into logical lines for guard/twin comparison: split on `\n`, drop
/// a single trailing `\r` (CRLF), and discard the empty element a terminating
/// newline produces. Block and file content are split the same way so equality
/// is meaningful.
fn split_logical(s: &str) -> Vec<&str> {
    let mut v: Vec<&str> = s
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    if v.last() == Some(&"") {
        v.pop();
    }
    v
}

/// A logical-line list with leading/trailing blank lines removed, owned.
/// Trimming blanks lets the candidate match the relocated content itself rather
/// than incidental surrounding whitespace (e.g. the blank line an insertion adds
/// around an anchor), and keeps the endpoint guards on real content lines.
fn trim_blank_lines(lines: &[&str]) -> Vec<String> {
    let start = lines.iter().position(|l| !l.trim().is_empty());
    let end = lines.iter().rposition(|l| !l.trim().is_empty());
    match (start, end) {
        (Some(a), Some(b)) => lines[a..=b].iter().map(|s| s.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// A block's logical lines, blank-trimmed and owned. Thin wrapper over
/// `trim_blank_lines` for whole-string blocks (`Write` content, a pure deletion's
/// `old_string`); the diff-based insertion path trims the net-new slice directly.
fn block_lines(s: &str) -> Vec<String> {
    trim_blank_lines(&split_logical(s))
}

/// The lines an `Edit` genuinely *adds*: `new`'s lines after removing the runs it
/// shares with `old` at the front and the back, then blank-trimmed.
///
/// An insertion relative to an anchor keeps that anchor in **both** `old_string`
/// and `new_string` — often on *both* sides of the inserted text (a leading and a
/// trailing anchor). Those shared lines are unchanged context merely passing
/// through the edit, not relocated content, so they must not reach the twin test:
/// a block sitting verbatim in some sibling file (an `INSTALL.md.backup`, a
/// vendored copy) would otherwise trip the insertion deny even though the edit
/// added nothing resembling it.
///
/// Diffing `new − old` at the **line** level is what guarantees this. The previous
/// whole-string prefix/suffix strip only canceled an anchor that was the *entire*
/// `old` sitting wholly at one end of `new`; a bilateral anchor, or any anchor
/// with differing content beside it, left `old` as neither a clean prefix nor
/// suffix of `new`, so the strip silently failed and the anchor leaked into the
/// candidate. Canceling the common leading and trailing line runs instead makes a
/// block present in both `old` and `new` net to **zero** added lines regardless of
/// what surrounds it — the discriminator is the `new − old` delta, not file
/// identity (§10).
fn added_lines(old: &str, new: &str) -> Vec<String> {
    let old_l = split_logical(old);
    let new_l = split_logical(new);
    // Cancel the common leading run.
    let mut lead = 0;
    while lead < old_l.len() && lead < new_l.len() && old_l[lead] == new_l[lead] {
        lead += 1;
    }
    // Cancel the common trailing run, without overlapping the leading run already
    // consumed on either side.
    let mut trail = 0;
    while trail < old_l.len() - lead
        && trail < new_l.len() - lead
        && old_l[old_l.len() - 1 - trail] == new_l[new_l.len() - 1 - trail]
    {
        trail += 1;
    }
    // What remains on the `new` side is the genuinely-added content.
    trim_blank_lines(&new_l[lead..new_l.len() - trail])
}

/// Append the move/copy-relevant candidate from a single `old → new` edit. A
/// pure deletion (`new` empty) of a sizeable block is the source-removal tell;
/// otherwise the genuinely-added text is the insertion tell. Each tell is
/// collected only at its own deny-eligible size — with no reminder outcome, a
/// smaller block can never produce output, so scanning for it would be wasted work.
fn push_edit(out: &mut Vec<Candidate>, old: &str, new: &str, file: &Option<String>) {
    if new.is_empty() {
        let block = block_lines(old);
        if block.len() >= DELETE_DENY_MIN_LINES {
            out.push(Candidate {
                block,
                tell: Tell::Deletion,
                file: file.clone(),
            });
        }
        return;
    }
    let block = added_lines(old, new);
    if block.len() >= DENY_MIN_LINES {
        out.push(Candidate {
            block,
            tell: Tell::Insertion,
            file: file.clone(),
        });
    }
}

/// Extract candidate blocks from a tool invocation. Only the three editing tools
/// are inspected; anything else yields no candidates (and so a silent allow).
fn candidates(tool: &str, input: &serde_json::Value) -> Vec<Candidate> {
    let file = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .map(String::from);
    let mut out = Vec::new();
    match tool {
        "Write" => {
            if let Some(content) = input.get("content").and_then(|v| v.as_str()) {
                let block = block_lines(content);
                // A `Write` is always an insertion; collect only deny-eligible sizes.
                if block.len() >= DENY_MIN_LINES {
                    out.push(Candidate {
                        block,
                        tell: Tell::Insertion,
                        file,
                    });
                }
            }
        }
        "Edit" => {
            let old = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = input
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            push_edit(&mut out, old, new, &file);
        }
        "MultiEdit" => {
            if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
                for e in edits {
                    let old = e.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
                    let new = e.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
                    push_edit(&mut out, old, new, &file);
                }
            }
        }
        _ => {}
    }
    out
}

/// One verbatim occurrence of the block: the file it was found in and the
/// 1-based line where the match starts.
struct Hit {
    path: PathBuf,
    start: usize,
}

/// Find every verbatim occurrence of `block` (a contiguous run of identical
/// logical lines) in `text`, recording each match's start line in `path`.
///
/// Twin matching is **byte-exact** by contract (§10, Decision 3): every line of
/// the block must equal the corresponding file line in full. This is the
/// false-positive filter that lets the deny be a *hard* gate — a near-miss is not
/// a relocation tell. Do not "optimize" this to a first+last+length heuristic;
/// that would resurrect false denies on coincidentally-bracketed blocks.
fn find_in_text(text: &str, block: &[String], path: &Path, hits: &mut Vec<Hit>) {
    let lines = split_logical(text);
    let len = block.len();
    if len == 0 || lines.len() < len {
        return;
    }
    for i in 0..=lines.len() - len {
        if (0..len).all(|k| lines[i + k] == block[k]) {
            hits.push(Hit {
                path: path.to_path_buf(),
                start: i + 1,
            });
        }
    }
}

/// Walk the workspace under `root` and collect every verbatim occurrence of the
/// block across readable UTF-8 text files. Skips VCS/build directories, hidden
/// directories, oversized files, and non-UTF-8 content. Errors are swallowed so
/// the hook fails open.
fn scan_workspace(root: &str, block: &[String]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut stack = vec![PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                let skip = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| SKIP_DIRS.contains(&n) || n.starts_with('.'))
                    .unwrap_or(false);
                if !skip {
                    stack.push(path);
                }
            } else if file_type.is_file() {
                if entry
                    .metadata()
                    .map(|m| m.len() > MAX_SCAN_BYTES)
                    .unwrap_or(true)
                {
                    continue;
                }
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // Treat only valid UTF-8 as text; relocation sources are text.
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    find_in_text(text, block, &path, &mut hits);
                }
            }
        }
    }
    hits
}

/// Render `path` relative to the workspace root for a tidy suggested command,
/// falling back to the original path when it is not under root.
fn display_path(path: &Path, root: &str) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// A guard substring for an endpoint line: trimmed and length-capped. A blank
/// endpoint cannot anchor anything, so `BLANK_GUARD_HINT` is emitted in its place
/// for the model to replace.
fn guard_for(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return BLANK_GUARD_HINT.to_string();
    }
    if trimmed.chars().count() > MAX_GUARD_CHARS {
        return trimmed.chars().take(MAX_GUARD_CHARS).collect();
    }
    trimmed.to_string()
}

/// A resolved endpoint anchor for an assembled span command (Decision 4): the
/// guard substring, plus an optional neighbor `(offset, guard)` that makes it
/// resolve to exactly one line. `context_flag` renders the optional half.
struct Anchor {
    guard: String,
    context: Option<(i64, String)>,
}

/// Build a uniqueness-checked guard for the endpoint at 0-based `idx` in `lines`,
/// mirroring span's own substring resolution (§5): if the trimmed line text
/// occurs on exactly one line, the bare guard suffices; otherwise borrow entropy
/// from a neighbor by attaching a `--*-context` `offset:guard` that makes the
/// combined anchor resolve to exactly one line — so the model's first `span` run
/// does not trip `ambiguous-anchor` (Decision 4). `inside_dir` (+1 for the `from`
/// endpoint, -1 for `to`) biases the search toward offsets *inside* the block,
/// whose neighbors move with the block and so stay valid when the model applies
/// the command. Best effort: returns a context-free anchor if nothing disambiguates.
fn unique_anchor(lines: &[&str], idx: usize, inside_dir: i64) -> Anchor {
    let base = guard_for(lines[idx]);
    // A blank endpoint already yields a placeholder; no neighbor can anchor it.
    if base == BLANK_GUARD_HINT {
        return Anchor {
            guard: base,
            context: None,
        };
    }
    let base_hits = lines.iter().filter(|l| l.contains(base.as_str())).count();
    if base_hits <= 1 {
        return Anchor {
            guard: base,
            context: None,
        };
    }
    // Scan outward by growing magnitude rather than a fixed window. A verbatim-
    // duplicated block — the only case the deny fires on — has its endpoint AND
    // every in-block neighbor duplicated by construction, so ONLY a line outside
    // the block can disambiguate, and that unique line may sit well beyond ±3.
    // Prefer the inside direction at each magnitude (its neighbors move with the
    // block, staying valid when the model applies the command); file length bounds
    // the scan, and out-of-range offsets are skipped below. The first neighbor
    // that makes the combined anchor resolve to exactly one line wins.
    let max_mag = lines.len() as i64;
    let offsets = (1..=max_mag).flat_map(|m| [m * inside_dir, -m * inside_dir]);
    for off in offsets {
        let t = idx as i64 + off;
        if t < 0 || t as usize >= lines.len() {
            continue;
        }
        let ctx = guard_for(lines[t as usize]);
        if ctx == BLANK_GUARD_HINT {
            continue; // a blank neighbor carries no entropy
        }
        // Count file lines satisfying (base here AND ctx at +off), mirroring
        // span's `satisfies`: case-sensitive substring over logical content.
        let pair_hits = (0..lines.len())
            .filter(|&l| {
                if !lines[l].contains(base.as_str()) {
                    return false;
                }
                let n = l as i64 + off;
                n >= 0 && (n as usize) < lines.len() && lines[n as usize].contains(ctx.as_str())
            })
            .count();
        if pair_hits == 1 {
            return Anchor {
                guard: base,
                context: Some((off, ctx)),
            };
        }
    }
    Anchor {
        guard: base,
        context: None,
    }
}

/// Render an optional neighbor anchor as a ` --<role>-context "off:guard"` flag,
/// or empty when there is none. The value is quoted so a guard containing spaces
/// or colons survives the shell (span splits on the first colon, §7).
fn context_flag(role: &str, ctx: &Option<(i64, String)>) -> String {
    match ctx {
        Some((off, guard)) => format!(" --{}-context {:?}", role, format!("{}:{}", off, guard)),
        None => String::new(),
    }
}

/// Compute uniqueness-checked `(from, to)` endpoint anchors for a block found at
/// `hit`, re-reading the twin's file so guard uniqueness is judged exactly as
/// span will (Decision 4). Falls back to plain trimmed endpoint guards (no
/// context) when the file cannot be re-read or the block no longer sits where the
/// scan found it — best effort, since the hook must never break editing.
fn resolve_endpoint_guards(block: &[String], hit: &Hit) -> (Anchor, Anchor) {
    let n = block.len();
    let fallback = || {
        (
            Anchor {
                guard: guard_for(&block[0]),
                context: None,
            },
            Anchor {
                guard: guard_for(&block[n - 1]),
                context: None,
            },
        )
    };
    let text = match std::fs::read_to_string(&hit.path) {
        Ok(t) => t,
        Err(_) => return fallback(),
    };
    let lines = split_logical(&text);
    let from_idx = hit.start - 1;
    let to_idx = hit.start + n - 2;
    if to_idx >= lines.len() {
        return fallback();
    }
    (
        unique_anchor(&lines, from_idx, 1),
        unique_anchor(&lines, to_idx, -1),
    )
}

/// Are two paths the same file on disk? Used to decide whether the relocation is
/// cross-file. A destination that cannot be canonicalized (e.g. a `Write` to a
/// not-yet-existing file) is treated as a different file.
fn same_file(a: &Path, b: &str) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Build the deny reason for a large inserted block: explain the hazard and
/// hand the model an assembled `span` command pre-filled with the discovered
/// source coordinates and endpoint guards. The destination anchor is left as a
/// placeholder because the hook cannot know the intended insertion point.
fn deny_reason(block: &[String], hit: &Hit, dest: Option<&str>, root: &str) -> String {
    let n = block.len();
    let from = hit.start;
    let to = hit.start + n - 1;
    let src = display_path(&hit.path, root);

    // Endpoint guards, uniqueness-checked against the twin's own file so the
    // model's first `span` run does not trip `ambiguous-anchor` (Decision 4).
    let (from_anchor, to_anchor) = resolve_endpoint_guards(block, hit);
    let from_guard = &from_anchor.guard;
    let to_guard = &to_anchor.guard;
    let from_ctx_flag = context_flag("from", &from_anchor.context);
    let to_ctx_flag = context_flag("to", &to_anchor.context);

    let cross = dest.map(|d| !same_file(&hit.path, d)).unwrap_or(false);
    let dest_file = if cross {
        // Render the destination relative to root as well.
        let shown = dest
            .map(|d| display_path(Path::new(d), root))
            .unwrap_or_default();
        format!(" --dest-file {shown}")
    } else {
        String::new()
    };

    // {:?} quotes and escapes each guard for a copy-pasteable shell argument.
    let cmd = format!(
        "span move {src} --from {from} --from-guard {from_guard:?}{from_ctx_flag} --to {to} --to-guard {to_guard:?}{to_ctx_flag} --dest <LINE> --dest-guard \"<TEXT ON DEST LINE>\" --after{dest_file}"
    );

    format!(
        "This {n}-line block already exists verbatim at {src}:{from}-{to}. Re-emitting it through the string editor risks silent corruption and carries the bytes as input on every later turn. Relocate it by reference with span instead:\n\n  {cmd}\n\nFill in --dest/--dest-guard for the insertion point (choose --before or --after), and use `span copy` (same flags) if you are duplicating rather than moving."
    )
}

/// Build the deny reason for a large editor deletion (§10, Decision 2): explain
/// the re-emit hazard and hand back a *fork* — the relocate form (`--dest …`,
/// which does *both* halves of a move by reference) and the delete form
/// (`--force`, no dest). Coordinates come from the block's **sole** occurrence in
/// the edited file (still on disk pre-edit) with uniqueness-checked guards.
/// Returns `None` when the file/coordinates cannot be determined, OR when the
/// block appears more than once — so the caller falls back to a silent allow
/// rather than emitting a confident command that might address the wrong copy.
fn delete_fork_reason(candidate: &Candidate, twin: Option<&str>, root: &str) -> Option<String> {
    let block = &candidate.block;
    let n = block.len();
    // Need the edited file to give --from/--to; without it there is no command.
    let file = candidate.file.as_deref()?;
    let text = std::fs::read_to_string(file).ok()?;
    let mut hits = Vec::new();
    find_in_text(&text, block, Path::new(file), &mut hits);
    // The block can legitimately appear more than once at the delete step of a
    // write-first move (the destination copy is already written, the source still
    // present). `hits.first()` would address an arbitrary occurrence, so a
    // confident `--from/--to/--force` command could target the WRONG copy and
    // silently invert the move — defeating span's "never act on the wrong line"
    // guarantee precisely because `unique_anchor` would manufacture a unique
    // anchor for that wrong site. Refuse to assemble a command unless the block is
    // unique in the file; the caller then degrades to a silent allow.
    if hits.len() != 1 {
        return None;
    }
    let hit = &hits[0];
    let from = hit.start;
    let to = hit.start + n - 1;

    let (from_anchor, to_anchor) = resolve_endpoint_guards(block, hit);
    let from_guard = &from_anchor.guard;
    let to_guard = &to_anchor.guard;
    let from_ctx_flag = context_flag("from", &from_anchor.context);
    let to_ctx_flag = context_flag("to", &to_anchor.context);
    let shown = display_path(Path::new(file), root);
    let coords = format!(
        "--from {from} --from-guard {from_guard:?}{from_ctx_flag} --to {to} --to-guard {to_guard:?}{to_ctx_flag}"
    );

    let relocate_cmd = format!(
        "span move {shown} {coords} --dest <LINE> --dest-guard \"<TEXT ON DEST LINE>\" --after"
    );
    let delete_cmd = format!("span move {shown} {coords} --force");

    // A twin in a different file strengthens the case that this is a move's first
    // half rather than a genuine deletion.
    let twin_note = match twin {
        Some(p) => format!(
            " It also appears verbatim at {p}, so this looks like the source half of a move."
        ),
        None => String::new(),
    };

    Some(format!(
        "Deleting this {n}-line block through the string editor forces you to reproduce all {n} lines verbatim in old_string — the same re-emit cost and corruption risk span removes.{twin_note} Do it by reference instead:\n\n  Relocating it? →\n    {relocate_cmd}\n\n  Genuinely deleting it? →\n    {delete_cmd}\n\nThe delete form requires --force as a deliberate acknowledgment; deletes run via `span … --force` go through your shell and are not interrupted by this hook."
    ))
}

/// Hook JSON for a hard deny carrying `reason` back to the model. A deny is
/// expressed in the body, never via exit status (§10) — the process still
/// exits 0.
fn deny_payload(reason: String) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
    .to_string()
}

/// Decide on the largest candidate block. Returns the hook's deny JSON, or `None`
/// for a silent allow — the hook's only two outcomes. Every collected candidate is
/// already deny-eligible by size (`push_edit` / `candidates` gate on the deny
/// thresholds), so there is no sub-threshold case left to filter here.
fn decide(cands: &[Candidate], root: &str) -> Option<String> {
    // Act on the biggest block; it is the one whose mishandling is costliest.
    let candidate = cands.iter().max_by_key(|c| c.block.len())?;
    let block = &candidate.block;

    let hits = scan_workspace(root, block);

    match candidate.tell {
        // Insertion = the destination half of a move/copy. The block is not yet on
        // disk, so any verbatim occurrence is the relocation source; with none, the
        // block is genuinely new and passes silently. A twinned block is the
        // hard-gate case (§10): deny with an assembled command.
        Tell::Insertion => {
            if hits.is_empty() {
                return None;
            }
            // The destination is the edited file; the twin is the source. Prefer a
            // twin that is not the destination file itself.
            let dest = candidate.file.as_deref();
            let hit = hits
                .iter()
                .find(|h| dest.map(|d| !same_file(&h.path, d)).unwrap_or(true))
                .unwrap_or(&hits[0]);
            Some(deny_payload(deny_reason(block, hit, dest, root)))
        }
        // Deletion = the source-removal half of a move. In delete-first order the
        // destination twin is not yet written, so — unlike an insertion — a twin is
        // NOT required to act (Decision 2): the re-emit cost is paid in old_string
        // regardless. Deny with the move/force fork; the forced-delete escape is the
        // clean exit. If a confident command cannot be assembled (no file_path,
        // unreadable, or the block is not unique in the edited file), fail open
        // silently rather than emit an unactionable or wrong-copy deny.
        Tell::Deletion => {
            // A twin in a *different* file strengthens the message (likely a move)
            // but is not required for the deny.
            let twin = hits
                .iter()
                .find(|h| {
                    candidate
                        .file
                        .as_deref()
                        .map(|f| !same_file(&h.path, f))
                        .unwrap_or(false)
                })
                .map(|h| display_path(&h.path, root));
            delete_fork_reason(candidate, twin.as_deref(), root).map(deny_payload)
        }
    }
}

/// Read the payload, decide, and print the hook JSON (if any). Always exits `0`:
/// a `deny` is expressed in the JSON body, never via exit status, and every
/// failure path falls through to a silent allow so editing is never broken.
fn main() {
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        return;
    }
    let input: HookInput = match serde_json::from_str(&buf) {
        Ok(v) => v,
        Err(_) => return,
    };
    let root = input.cwd.clone().unwrap_or_else(|| ".".to_string());
    let cands = candidates(&input.tool_name, &input.tool_input);
    if let Some(output) = decide(&cands, &root) {
        println!("{output}");
    }
}
