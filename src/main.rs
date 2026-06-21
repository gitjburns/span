//! `span` — relocate or duplicate existing lines *by reference*.
//!
//! The model addresses lines by the 1-based numbers a file read already prints,
//! and supplies a substring *guard* per coordinate. `span` re-reads the file(s)
//! from disk on every call (disk is canonical — no session state), verifies that
//! each coordinate still resolves to a unique, undrifted line, then moves or
//! copies the bytes itself. The relocated bytes never re-enter the model's
//! context. Every failure is loud: nothing is written unless the call exits 0.
//!
//! See SPEC.md. This is a single self-contained binary on purpose
//! (no config, no JSON, no session) so it costs nothing until invoked.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use bstr::ByteSlice;
use clap::{Args, Parser, Subcommand};
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Command-line surface (§7)
// ---------------------------------------------------------------------------

/// Top-level parser: `span move …` / `span copy …`.
#[derive(Parser)]
#[command(
    name = "span",
    version,
    about = "Relocate or duplicate existing lines by reference (no content re-emission)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The two — and only two — verbs; both share the same flag shape (`OpArgs`).
/// `move` also has a degenerate no-dest form that deletes a span by reference
/// (§6, requires `--force`). Authoring new text and replacing content remain
/// explicit non-goals (§11).
#[derive(Subcommand)]
enum Command {
    /// Remove lines [from, to] and reinsert them relative to dest — or, with no
    /// --dest and --force, remove them entirely (delete-by-reference, §6).
    Move(OpArgs),
    /// Duplicate lines [from, to] relative to dest; the source is unchanged.
    Copy(OpArgs),
}

/// Flags shared by `move` and `copy`. Coordinates are line numbers plus
/// substring guards; `--*-context` entries borrow entropy from neighbors (§5).
#[derive(Args)]
struct OpArgs {
    /// Source file (positional). The destination defaults to this file.
    file: String,

    /// `from`/`to`/`dest` line numbers are **optional** (§5). A coordinate is
    /// addressed by its guard substring; the number, when supplied, is a
    /// positional cross-check (the guard must resolve to exactly that line, else
    /// loud `drift`). Omitting the number lets the guard resolve wherever it
    /// uniquely matches — no drift, no range check, uniqueness unchanged. This is
    /// what lets an agent sequence a batch of edits without re-reading a file to
    /// recover line numbers shifted by an earlier edit (the read `span` exists to
    /// avoid). The guard itself is never optional: a number alone cannot address a
    /// line, having no integrity check.
    #[arg(long)]
    from: Option<usize>,
    #[arg(long = "from-guard")]
    from_guard: Option<String>,

    #[arg(long)]
    to: Option<usize>,
    #[arg(long = "to-guard")]
    to_guard: Option<String>,

    /// Destination line for a relocation, optional like `from`/`to` (§5): the
    /// `--dest-guard` addresses the line and the number, if given, cross-checks it.
    /// `Some(0)` with `--after` means the start of the destination file and takes
    /// no guard (§6). The forced delete-by-reference form is keyed on `--force`,
    /// **not** on an absent `--dest` (a guard-only destination is also "no number"
    /// yet relocates); `copy` always requires a destination.
    #[arg(long)]
    dest: Option<usize>,
    #[arg(long = "dest-guard")]
    dest_guard: Option<String>,

    /// Position the relocated block before or after `dest` (exactly one).
    /// Meaningless — and rejected — on the no-dest delete form.
    #[arg(long)]
    before: bool,
    #[arg(long)]
    after: bool,

    /// Acknowledge a destination-less `move` as a deliberate delete-by-reference
    /// (§6). Required for that form, rejected whenever a destination is present,
    /// and it never weakens the `from`/`to` guard checks (a forced delete still
    /// resolves both endpoints to unique, undrifted lines).
    #[arg(long)]
    force: bool,

    /// Destination file for a cross-file relocation; omit for intra-file.
    #[arg(long = "dest-file")]
    dest_file: Option<String>,

    /// Repeatable neighbor anchors, each `offset:guard` (split on first colon).
    /// `allow_hyphen_values` is required so negative offsets like `-3:fn parse`
    /// (a first-class feature, §5/§7) are not mistaken for flags.
    #[arg(long = "from-context", allow_hyphen_values = true)]
    from_context: Vec<String>,
    #[arg(long = "to-context", allow_hyphen_values = true)]
    to_context: Vec<String>,
    #[arg(long = "dest-context", allow_hyphen_values = true)]
    dest_context: Vec<String>,
}

// ---------------------------------------------------------------------------
// Errors and exit codes (§7, §8)
// ---------------------------------------------------------------------------

/// Which coordinate a failure pertains to, used verbatim in flag names/hints.
#[derive(Clone, Copy, Debug)]
enum Role {
    From,
    To,
    Dest,
}

impl Role {
    /// Lowercase flag stem (`from`/`to`/`dest`) for error messages and hints.
    fn as_str(self) -> &'static str {
        match self {
            Role::From => "from",
            Role::To => "to",
            Role::Dest => "dest",
        }
    }
}

/// Every loud failure mode. The one-line message and the process exit code are
/// derived from the variant, keeping the §7 exit-code contract in one place
/// rather than scattered at call sites.
#[derive(Debug)]
enum SpanError {
    GuardNotFound {
        role: Role,
        guard: String,
        path: String,
    },
    Ambiguous {
        role: Role,
        guard: String,
        /// Each matching line as `(1-based number, logical content)`. The content
        /// is carried so the error can show candidates verbatim (§8, Change 4),
        /// letting the agent pick a `--*-context` neighbor without re-reading.
        matches: Vec<(usize, String)>,
        path: String,
    },
    Drift {
        role: Role,
        /// The guard text that drifted, echoed back so a consolidated report can
        /// name the anchor (`anchor "…" resolved at line N`) without re-reading.
        guard: String,
        addressed: usize,
        actual: usize,
    },
    OutOfRange {
        role: Role,
        line: usize,
        count: usize,
        path: String,
    },
    DestInSpan {
        dest: usize,
        from: usize,
        to: usize,
    },
    FromAfterTo {
        from: usize,
        to: usize,
    },
    EmptyGuard {
        role: Role,
    },
    /// A `--dest-file` (or source) path that does not exist. Authoring files is a
    /// non-goal (§11), so a missing destination is an I/O failure (exit 3, Change
    /// 3) rather than an implicit create.
    FileMissing {
        path: String,
        /// True when the path was the `--dest-file`, which gets the more pointed
        /// "span relocates into existing files only" hint.
        is_dest: bool,
    },
    /// One or more coordinates failed up-front validation (Change 1). Carries the
    /// per-role outcome for all three coordinates so the agent sees every failure
    /// — and the resolved lines of the ones that passed — in a single response,
    /// avoiding the serial fix-rerun-fix round-trips a first-failure-wins report
    /// forces under a uniform line shift.
    Multi(Vec<RoleOutcome>),
    /// Bad/missing flag combinations not caught by clap's own parsing.
    Usage(String),
    /// Unreadable/unwritable file or non-UTF-8 input.
    Io(String),
}

/// The validation result for one coordinate role, used only to assemble the
/// consolidated report (Change 1). A passing coordinate keeps its resolved line
/// so the report can show `--dest  ok: line 321` beside the failures.
#[derive(Debug)]
struct RoleOutcome {
    role: Role,
    /// `Ok(line)` if the coordinate resolved; `Err` carries the loud failure for
    /// that role (drift/ambiguous/guard-not-found/out-of-range/empty-guard).
    result: Result<usize, SpanError>,
}

impl SpanError {
    /// Process exit code: 1 = loud coordinate/guard failure, 2 = usage,
    /// 3 = I/O (§7).
    fn code(&self) -> i32 {
        match self {
            SpanError::Usage(_) => 2,
            // A missing file is an I/O-class failure (exit 3, §7/Change 3), kept
            // distinct from a generic Io only for its more actionable message.
            SpanError::Io(_) | SpanError::FileMissing { .. } => 3,
            _ => 1,
        }
    }

    /// The single actionable stderr line (§8): reason word, offending
    /// coordinate, and a hint toward the fix.
    fn message(&self) -> String {
        match self {
            SpanError::GuardNotFound { role, guard, path } => format!(
                "error: guard-not-found: --{}-guard {:?} not found in {}",
                role.as_str(),
                guard,
                path
            ),
            // Change 4: list each candidate as `<number><TAB><content>` so the
            // agent can choose a disambiguating --*-context neighbor without
            // re-reading the file.
            SpanError::Ambiguous {
                role,
                guard,
                matches,
                path,
            } => {
                let mut s = format!(
                    "error: ambiguous-anchor: --{}-guard {:?} matches {} lines in {}:\n",
                    role.as_str(),
                    guard,
                    matches.len(),
                    path
                );
                for (num, text) in matches {
                    s.push_str(&format!("   {}\t{}\n", num, truncate_content(text)));
                }
                s.push_str(&format!(
                    "  hint: add --{}-context (e.g. a distinctive neighbor line) to make it unique",
                    role.as_str()
                ));
                s
            }
            SpanError::Drift {
                role,
                guard,
                addressed,
                actual,
            } => format!(
                "error: drift: --{0} anchor {1:?} resolved at line {2}, not {3}; retry with --{0} {2}",
                role.as_str(),
                guard,
                actual,
                addressed
            ),
            SpanError::OutOfRange {
                role,
                line,
                count,
                path,
            } => format!(
                "error: out-of-range: --{} {} is outside [1, {}] in {}",
                role.as_str(),
                line,
                count,
                path
            ),
            SpanError::DestInSpan { dest, from, to } => format!(
                "error: dest-in-span: --dest {} lies within [{}, {}]; choose a destination outside the span",
                dest, from, to
            ),
            SpanError::FromAfterTo { from, to } => format!(
                "error: from-after-to: --from {} is greater than --to {}",
                from, to
            ),
            SpanError::EmptyGuard { role } => format!(
                "error: empty-guard: --{}-guard must be a non-empty substring",
                role.as_str()
            ),
            SpanError::FileMissing { path, is_dest } => {
                if *is_dest {
                    format!(
                        "error: io: destination file {:?} does not exist; span relocates into existing files only",
                        path
                    )
                } else {
                    format!("error: io: file {:?} does not exist", path)
                }
            }
            // Change 1: one report covering all three coordinates. Passing
            // coordinates are shown as `ok: line N` beside the failures so the
            // agent can rebuild the whole command in a single edit.
            SpanError::Multi(outcomes) => {
                let failures = outcomes.iter().filter(|o| o.result.is_err()).count();
                let mut s = format!(
                    "error: {} {} failed:\n",
                    failures,
                    if failures == 1 {
                        "coordinate"
                    } else {
                        "coordinates"
                    }
                );
                for outcome in outcomes {
                    s.push_str(&outcome.consolidated_line());
                }
                // Trim the trailing newline so the whole report is one logical
                // stderr block with no blank tail.
                s.pop();
                s
            }
            SpanError::Usage(msg) => format!("error: usage: {}", msg),
            SpanError::Io(msg) => format!("error: io: {}", msg),
        }
    }
}

/// Truncate a line's logical content to a sensible display width for error
/// listings (Change 4 / §8), appending an ellipsis when cut. Width is in chars,
/// not bytes, so multibyte content is not split mid-character.
fn truncate_content(text: &str) -> String {
    const MAX: usize = 60;
    if text.chars().count() <= MAX {
        text.to_string()
    } else {
        let cut: String = text.chars().take(MAX).collect();
        format!("{}…", cut)
    }
}

impl RoleOutcome {
    /// One indented line (occasionally several, for an ambiguous candidate list)
    /// for the consolidated report (Change 1): `  --<role>  <status>`. A passing
    /// coordinate reads `ok: line N`; a failing one carries its reason word and
    /// actionable hint, mirroring the standalone messages so the agent sees the
    /// same wording either way.
    fn consolidated_line(&self) -> String {
        let role = self.role.as_str();
        match &self.result {
            Ok(line) => format!("  --{}\tok: line {}\n", role, line),
            Err(SpanError::Drift {
                guard,
                addressed,
                actual,
                ..
            }) => format!(
                "  --{0}\tdrift: anchor {1:?} resolved at line {2}, not {3} (retry with --{0} {2})\n",
                role, guard, actual, addressed
            ),
            Err(SpanError::GuardNotFound { guard, path, .. }) => format!(
                "  --{}\tguard-not-found: {:?} not found in {}\n",
                role, guard, path
            ),
            Err(SpanError::Ambiguous { guard, matches, .. }) => {
                let mut s = format!(
                    "  --{}\tambiguous-anchor: {:?} matches {} lines (add --{}-context):\n",
                    role,
                    guard,
                    matches.len(),
                    role
                );
                for (num, text) in matches {
                    s.push_str(&format!("       {}\t{}\n", num, truncate_content(text)));
                }
                s
            }
            Err(SpanError::OutOfRange {
                line, count, path, ..
            }) => format!(
                "  --{}\tout-of-range: line {} is outside [1, {}] in {}\n",
                role, line, count, path
            ),
            Err(SpanError::EmptyGuard { .. }) => {
                format!(
                    "  --{0}\tempty-guard: --{0}-guard must be a non-empty substring\n",
                    role
                )
            }
            // Other variants never arise from per-coordinate resolution; render
            // their standalone message rather than silently dropping detail.
            Err(other) => format!("  --{}\t{}\n", role, other.message()),
        }
    }
}

// ---------------------------------------------------------------------------
// Text model (§4)
// ---------------------------------------------------------------------------

/// A neighbor anchor: a signed offset and the substring expected on the line
/// at `addressed + offset`.
struct ContextSpec {
    offset: i64,
    guard: String,
}

/// A file as an ordered list of physical lines, each retaining its own line
/// terminator (`\n` or `\r\n`) exactly as it appeared. The final element lacks
/// a terminator iff the file did not end with a newline. This representation is
/// what makes relocation byte-exact and preserves each file's CRLF and
/// trailing-newline state (§4).
type Lines = Vec<Vec<u8>>;

/// Read a file as UTF-8 text split into terminator-preserving physical lines.
/// Non-UTF-8 or unreadable input is an I/O-class failure (exit 3); the bytes
/// themselves are never normalized.
fn read_lines(path: &str) -> Result<Lines, SpanError> {
    let bytes =
        fs::read(path).map_err(|e| SpanError::Io(format!("cannot read {}: {}", path, e)))?;
    // Treated as UTF-8 text (§4); reject otherwise rather than corrupt silently.
    if std::str::from_utf8(&bytes).is_err() {
        return Err(SpanError::Io(format!("{} is not valid UTF-8", path)));
    }
    Ok(bytes.lines_with_terminator().map(|l| l.to_vec()).collect())
}

/// Logical content of a line for guard matching: the bytes with a single
/// trailing terminator removed (`\n`, and a preceding `\r` for CRLF, §4). The
/// terminator is never part of guard content.
fn content(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && line[end - 1] == b'\r' {
            end -= 1;
        }
    }
    &line[..end]
}

/// Whether a physical line carries a `\n` terminator.
fn has_terminator(line: &[u8]) -> bool {
    line.last() == Some(&b'\n')
}

/// Guarantee a line ends with a separator, synthesizing a `\n` if missing (§4,
/// "never join lines"). The synthesized separator is always a bare `\n`.
fn ensure_terminator(line: &mut Vec<u8>) {
    if !has_terminator(line) {
        line.push(b'\n');
    }
}

/// Remove a line's terminator (`\n`, plus a preceding `\r`) so it can become a
/// file's final unterminated line without altering its logical content.
fn strip_terminator(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
}

// ---------------------------------------------------------------------------
// Coordinate guard resolution (§5)
// ---------------------------------------------------------------------------

/// Does the 1-based line `idx` satisfy `guard` and every context anchor? A
/// context anchor whose target line falls outside the file fails to satisfy
/// (it cannot match a line that does not exist). Matching is a case-sensitive
/// literal substring test against logical content (§5).
fn satisfies(lines: &Lines, idx: usize, guard: &str, context: &[ContextSpec]) -> bool {
    if content(&lines[idx - 1]).find(guard.as_bytes()).is_none() {
        return false;
    }
    for cs in context {
        let target = idx as i64 + cs.offset;
        if target < 1 || target as usize > lines.len() {
            return false;
        }
        if content(&lines[target as usize - 1])
            .find(cs.guard.as_bytes())
            .is_none()
        {
            return false;
        }
    }
    true
}

/// Resolve a guarded coordinate against `lines`, enforcing the integrity
/// guarantee (§5). The guard is the address; `addressed` is an **optional**
/// positional cross-check:
/// - `Some(n)`: valid iff exactly one line satisfies the anchor *and* it is line
///   `n` — otherwise loud `drift` (unique match elsewhere) / `out-of-range`.
/// - `None` (guard-only): valid iff exactly one line satisfies the anchor; that
///   line is used. There is no addressed line, so drift cannot arise and the
///   range check is skipped.
///
/// In **both** modes the uniqueness scan is identical, so zero matches
/// (`guard-not-found`) and multiple matches (`ambiguous-anchor`) fail loud either
/// way — a wrong-line edit is impossible whether or not a number was supplied.
fn resolve(
    lines: &Lines,
    path: &str,
    role: Role,
    addressed: Option<usize>,
    guard: &str,
    context: &[ContextSpec],
) -> Result<usize, SpanError> {
    if guard.is_empty() {
        return Err(SpanError::EmptyGuard { role });
    }
    let count = lines.len();
    // Range-validate only a *supplied* number; guard-only addressing has no number
    // to bound (the scan below resolves wherever the content uniquely lives).
    if let Some(n) = addressed
        && (n < 1 || n > count)
    {
        return Err(SpanError::OutOfRange {
            role,
            line: n,
            count,
            path: path.to_string(),
        });
    }
    // Uniqueness scan: this is the integrity guarantee in both addressing modes,
    // including low-entropy lines (a bare `}` is ambiguous until context
    // disambiguates), so a coincidental same-content line is never edited silently.
    let matches: Vec<usize> = (1..=count)
        .filter(|&i| satisfies(lines, i, guard, context))
        .collect();
    match matches.as_slice() {
        [] => Err(SpanError::GuardNotFound {
            role,
            guard: guard.to_string(),
            path: path.to_string(),
        }),
        // A number was supplied and the unique match sits elsewhere → loud drift.
        // When the number agrees, or none was given, the unique match is the line.
        [only] => match addressed {
            Some(n) if n != *only => Err(SpanError::Drift {
                role,
                guard: guard.to_string(),
                addressed: n,
                actual: *only,
            }),
            _ => Ok(*only),
        },
        many => Err(SpanError::Ambiguous {
            role,
            guard: guard.to_string(),
            // Carry each candidate's logical content (§4) so the error lists
            // `<number>\t<content>` and the agent can disambiguate without a
            // re-read (Change 4).
            matches: many
                .iter()
                .map(|&i| (i, display_text(&lines[i - 1])))
                .collect(),
            path: path.to_string(),
        }),
    }
}

/// Parse repeatable `offset:guard` context flags into `ContextSpec`s, splitting
/// on the first colon so the guard may itself contain colons (§7). Zero offsets
/// and malformed entries are usage errors; an empty guard is the loud
/// `empty-guard` failure (§6).
fn parse_context(specs: &[String], role: Role) -> Result<Vec<ContextSpec>, SpanError> {
    let mut out = Vec::with_capacity(specs.len());
    for raw in specs {
        let (off_str, guard) = raw.split_once(':').ok_or_else(|| {
            SpanError::Usage(format!(
                "--{}-context {:?} must be of the form offset:guard",
                role.as_str(),
                raw
            ))
        })?;
        let offset: i64 = off_str.parse().map_err(|_| {
            SpanError::Usage(format!(
                "--{}-context offset {:?} is not an integer",
                role.as_str(),
                off_str
            ))
        })?;
        if offset == 0 {
            return Err(SpanError::Usage(format!(
                "--{}-context offset must be non-zero",
                role.as_str()
            )));
        }
        if guard.is_empty() {
            return Err(SpanError::EmptyGuard { role });
        }
        out.push(ContextSpec {
            offset,
            guard: guard.to_string(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Splice mechanics (§4 seam rules)
// ---------------------------------------------------------------------------

/// Whether a line is blank: its logical content (§4, terminator excluded) holds
/// no non-whitespace byte. An empty line and a whitespace-only line both count
/// (Change 2). This is the local fact boundary-blank mirroring keys on.
fn is_blank(line: &[u8]) -> bool {
    content(line).iter().all(|b| b.is_ascii_whitespace())
}

/// Count the consecutive blank lines immediately *above* the 0-based position
/// `gap` (i.e. at indices `gap-1, gap-2, …`), stopping at the first non-blank
/// line or the top of file. This is the source block's leading blank-run width
/// when `gap` is the block's first-line index, and the destination's existing
/// leading run when `gap` is an insertion point (Decision 1).
fn blank_run_above(lines: &Lines, gap: usize) -> usize {
    let mut n = 0;
    while gap > n && is_blank(&lines[gap - 1 - n]) {
        n += 1;
    }
    n
}

/// Count the consecutive blank lines starting *at* the 0-based position `gap`
/// (indices `gap, gap+1, …`), stopping at the first non-blank line or EOF. This
/// is the source block's trailing blank-run width when `gap` is the index just
/// past the block, and the destination's existing trailing run when `gap` is an
/// insertion point (Decision 1).
fn blank_run_below(lines: &Lines, gap: usize) -> usize {
    let mut n = 0;
    while gap + n < lines.len() && is_blank(&lines[gap + n]) {
        n += 1;
    }
    n
}

/// The separator convention to use for a *synthesized* blank line in this file
/// (Change 2 / §4): `\r\n` if any existing line is CRLF-terminated, else `\n`.
/// A synthesized blank is the file's own newline bytes and nothing else.
fn newline(lines: &Lines) -> Vec<u8> {
    let crlf = lines
        .iter()
        .any(|l| l.len() >= 2 && l[l.len() - 2] == b'\r' && l[l.len() - 1] == b'\n');
    if crlf {
        b"\r\n".to_vec()
    } else {
        b"\n".to_vec()
    }
}

/// Insert `block` into `lines` at the 0-based `gap` (block lands before the
/// element currently at `gap`; `gap == lines.len()` appends) and return the
/// block's resulting 1-based inclusive range — *excluding* any synthesized
/// blanks, so the caller marks only the moved lines as affected (§8).
///
/// Enforces the §4 seam invariants (never join lines; an append adopts the
/// destination's trailing-newline convention) and applies count-based
/// boundary-blank mirroring (Decision 1): `src_leading_blanks`/
/// `src_trailing_blanks` are the source block's blank-run widths on each side,
/// and the destination seam is *topped up* to that width — synthesizing only the
/// shortfall over the blank run already present at the seam, never exceeding the
/// source count. Mirroring never fabricates whitespace the source lacked
/// (`count == 0` synthesizes nothing) and never doubles an existing blank.
fn insert_block(
    lines: &mut Lines,
    gap: usize,
    mut block: Lines,
    src_leading_blanks: usize,
    src_trailing_blanks: usize,
) -> (usize, usize) {
    let blen = block.len();
    if blen == 0 {
        return (gap, gap);
    }
    let nl = newline(lines);
    // Capture the destination's trailing-newline convention before mutating:
    // None = empty file (no convention to honor).
    let dest_trailing_nl = lines.last().map(|l| has_terminator(l));

    // Mirroring decisions (positional — "preceding"/"following" are relative to
    // where the block lands). Synthesize `max(0, source_count - existing_run)`
    // blanks on each side: top the seam up to the source's width, never beyond.
    // A leading run is suppressed at top-of-file (gap == 0, no preceding line); a
    // trailing run at EOF (gap == len, no following line) — no neighbor to part.
    let add_leading = if gap > 0 {
        src_leading_blanks.saturating_sub(blank_run_above(lines, gap))
    } else {
        0
    };
    let add_trailing = if gap < lines.len() {
        src_trailing_blanks.saturating_sub(blank_run_below(lines, gap))
    } else {
        0
    };

    // The line preceding the insertion point must terminate so the block (or a
    // synthesized leading blank) does not glue onto it.
    if gap > 0 {
        ensure_terminator(&mut lines[gap - 1]);
    }

    // The block's last line must terminate when anything follows it — a
    // synthesized trailing blank, or an existing line at the gap. Only a true
    // EOF append with no trailing blank lets it adopt the destination convention.
    if add_trailing > 0 || gap < lines.len() {
        ensure_terminator(block.last_mut().unwrap());
    } else {
        match dest_trailing_nl {
            Some(true) => ensure_terminator(block.last_mut().unwrap()),
            Some(false) => strip_terminator(block.last_mut().unwrap()),
            // Empty destination has no convention; keep the block byte-exact.
            None => {}
        }
    }

    // Assemble [leading blanks…] + block + [trailing blanks…] and splice it in.
    let mut insertion: Lines = Vec::with_capacity(blen + add_leading + add_trailing);
    for _ in 0..add_leading {
        insertion.push(nl.clone());
    }
    let block_start = gap + insertion.len(); // 0-based index of the block's first line
    insertion.extend(block);
    for _ in 0..add_trailing {
        insertion.push(nl.clone());
    }

    let tail = lines.split_off(gap);
    lines.extend(insertion);
    lines.extend(tail);

    (block_start + 1, block_start + blen)
}

/// Remove the 1-based inclusive span [from, to] from `lines`, preserving the
/// file's trailing-newline state (§4): removing the tail must not silently add
/// or drop a terminating newline on the new last line.
fn remove_block(lines: &mut Lines, from: usize, to: usize) {
    let had_trailing_nl = lines.last().map(|l| has_terminator(l)).unwrap_or(false);
    let removed_tail = to == lines.len();
    lines.drain(from - 1..to);
    if removed_tail && let Some(last) = lines.last_mut() {
        // Reassert the original trailing-newline convention on the line that
        // is now last.
        if had_trailing_nl {
            ensure_terminator(last);
        } else {
            strip_terminator(last);
        }
    }
}

/// Move-only source-gap cleanup (Decision 1): removing a block that was
/// blank-delimited on *both* sides leaves its `lead` leading blanks adjacent to
/// its `trail` trailing blanks — a merged run of `lead + trail` where the
/// neighbors were previously separated only by the block. Reduce that
/// removal-created run to `max(lead, trail)` by dropping `min(lead, trail)`
/// blanks, so a relocation never restyles the bystander lines it leaves behind
/// (the symmetric PEP 8 `2/2` case stays `2`; the legacy `1/1` case stays `1`).
/// It only de-doubles: when either side had no blank run (`min == 0`) nothing is
/// removed, so a single legitimately-placed separator — e.g. one left dangling at
/// EOF — is never stripped. `seam` is the 1-based index now occupied by the line
/// that followed the removed block. Preserves the file's trailing-newline state
/// and returns the number of lines removed, so an in-flight insertion index can
/// be adjusted.
fn collapse_seam_blank(lines: &mut Lines, seam: usize, lead: usize, trail: usize) -> usize {
    // The doubling exists only when the block was blank-delimited on both sides;
    // we keep the wider run (max) and drop the overlap (min). lead/trail were
    // measured before removal, so this is purely removal-created.
    let remove_n = lead.min(trail);
    // Defensive bounds: the dropped blanks occupy 1-based [seam, seam+remove_n-1]
    // (the trailing-side run); keep the leading-side run intact above the seam.
    if remove_n == 0 || seam < 1 || seam - 1 + remove_n > lines.len() {
        return 0;
    }
    let had_trailing_nl = lines.last().map(|l| has_terminator(l)).unwrap_or(false);
    let removed_tail = seam - 1 + remove_n == lines.len();
    lines.drain(seam - 1..seam - 1 + remove_n);
    if removed_tail && let Some(last) = lines.last_mut() {
        if had_trailing_nl {
            ensure_terminator(last);
        } else {
            strip_terminator(last);
        }
    }
    remove_n
}

/// Translate a destination line + position into a 0-based insertion gap. Line
/// `0` (only valid with `after`) denotes the very start of the file (§6).
fn gap_for(dest_line: usize, before: bool) -> usize {
    if dest_line == 0 {
        0
    } else if before {
        dest_line - 1
    } else {
        dest_line
    }
}

// ---------------------------------------------------------------------------
// Atomic write (§9)
// ---------------------------------------------------------------------------

/// Write `lines` to `path` atomically and durably: a fresh temp file in the
/// destination directory, fsync the temp file, rename over the target, then
/// fsync the directory. The file fully updates or is left untouched (§9).
fn write_file(path: &str, lines: &Lines) -> Result<(), SpanError> {
    let target = Path::new(path);
    // Empty parent (a bare filename) means the current directory.
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => Path::new(".").to_path_buf(),
    };

    let io = |e: std::io::Error| SpanError::Io(format!("cannot write {}: {}", path, e));

    let mut tmp = NamedTempFile::new_in(&dir).map_err(io)?;
    for line in lines {
        tmp.write_all(line).map_err(io)?;
    }
    tmp.flush().map_err(io)?;
    tmp.as_file().sync_all().map_err(io)?; // durability of contents
    tmp.persist(target)
        .map_err(|e| SpanError::Io(format!("cannot persist {}: {}", path, e)))?; // atomic rename

    // fsync the directory so the rename itself is durable.
    let dir_handle =
        File::open(&dir).map_err(|e| SpanError::Io(format!("cannot open dir for fsync: {}", e)))?;
    dir_handle
        .sync_all()
        .map_err(|e| SpanError::Io(format!("cannot fsync dir: {}", e)))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Output rendering (§8)
// ---------------------------------------------------------------------------

/// Decoded logical text of a line for display. Input was validated as UTF-8 on
/// read (`read_lines`), so this conversion cannot lose data.
fn display_text(line: &[u8]) -> String {
    String::from_utf8_lossy(content(line)).into_owned()
}

/// `"line"`/`"lines"` agreement helper for summary lines.
fn plural(n: usize) -> &'static str {
    if n == 1 { "line" } else { "lines" }
}

/// Append one numbered row in `<gutter> <number>\t<text>` form (§8). `affected`
/// rows carry a leading `>`, context rows a space.
fn push_row(out: &mut String, lines: &Lines, num: usize, affected: bool) {
    let gutter = if affected { '>' } else { ' ' };
    out.push_str(&format!(
        "{} {}\t{}\n",
        gutter,
        num,
        display_text(&lines[num - 1])
    ));
}

/// Render the affected range plus 3 lines of context above and below, clamped
/// to file bounds (§8). Affected ranges longer than 6 lines elide the middle,
/// showing the first and last 3 with a `…` marker.
fn render_affected(out: &mut String, lines: &Lines, start: usize, end: usize) {
    let n = lines.len();
    let ctx_start = if start > 3 { start - 3 } else { 1 };
    let ctx_end = (end + 3).min(n);

    for i in ctx_start..start {
        push_row(out, lines, i, false);
    }
    if end - start < 6 {
        for i in start..=end {
            push_row(out, lines, i, true);
        }
    } else {
        for i in start..start + 3 {
            push_row(out, lines, i, true);
        }
        out.push_str("  …\n");
        for i in end - 2..=end {
            push_row(out, lines, i, true);
        }
    }
    for i in end + 1..=ctx_end {
        push_row(out, lines, i, false);
    }
}

/// Render the seam left by a cross-file move's source removal: surrounding
/// lines around the removal point, renumbered, with no `>` markers (§8). When
/// the removed block was the file's tail, the window clamps to the new end.
fn render_seam(out: &mut String, lines: &Lines, seam_line: usize) {
    let n = lines.len();
    if n == 0 {
        return; // source is now empty: the header note alone conveys removal.
    }
    let center = seam_line.min(n + 1);
    let start = if center > 3 { center - 3 } else { 1 };
    let end = (center + 2).min(n);
    for i in start..=end {
        push_row(out, lines, i, false);
    }
}

// ---------------------------------------------------------------------------
// Operation assembly and dispatch (§6)
// ---------------------------------------------------------------------------

/// Whether the relocation duplicates or moves; controls source removal and the
/// summary verb.
#[derive(Clone, Copy, PartialEq)]
enum Verb {
    Move,
    Copy,
}

impl Verb {
    /// Past-tense word used in the success summary line (§8).
    fn summary_word(self) -> &'static str {
        match self {
            Verb::Move => "moved",
            Verb::Copy => "copied",
        }
    }
}

/// Do two path strings refer to the same file on disk? Used to fold a
/// `--dest-file` that names the source back into the intra-file path (§6), so
/// the overlap guard still applies. Non-canonicalizable paths are treated as
/// distinct.
fn same_file(a: &str, b: &str) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Resolve the destination coordinate, handling the top-of-file anchor (line
/// `0`, which requires `--after`, takes no guard, and no context, §6). `dest` is
/// the relocate path's destination line — `args.dest` already unwrapped from its
/// `Option` (the `None` delete form never reaches here, §6).
fn resolve_dest(
    lines: &Lines,
    path: &str,
    args: &OpArgs,
    dest_ctx: &[ContextSpec],
    before: bool,
    dest: Option<usize>,
) -> Result<usize, SpanError> {
    // Top-of-file anchor: `--dest 0 --after`, no guard, no context (§6).
    if dest == Some(0) {
        if before {
            return Err(SpanError::Usage(
                "--dest 0 (top of file) requires --after".into(),
            ));
        }
        if args.dest_guard.is_some() {
            return Err(SpanError::Usage("--dest 0 takes no --dest-guard".into()));
        }
        if !dest_ctx.is_empty() {
            return Err(SpanError::Usage("--dest 0 takes no --dest-context".into()));
        }
        return Ok(0);
    }
    // Number present (and non-zero) or omitted entirely: either way the
    // `--dest-guard` is the address and must be present. Passing `dest` (an
    // `Option`) straight through means a guard-only destination skips the
    // drift/range check inside `resolve`, exactly like `from`/`to`.
    let guard = args
        .dest_guard
        .as_deref()
        .ok_or(SpanError::EmptyGuard { role: Role::Dest })?;
    resolve(lines, path, Role::Dest, dest, guard, dest_ctx)
}

/// Fail loud (exit 3, Change 3) if `path` does not exist rather than creating
/// it — authoring files is a non-goal (§11). `is_dest` selects the more pointed
/// destination hint. Runs in the up-front validation phase (§9 step 1), so a
/// missing file never results in a partial write.
fn ensure_exists(path: &str, is_dest: bool) -> Result<(), SpanError> {
    if Path::new(path).exists() {
        Ok(())
    } else {
        Err(SpanError::FileMissing {
            path: path.to_string(),
            is_dest,
        })
    }
}

/// Validate flags, resolve all coordinates, apply the relocation, and return
/// the success output. All guard resolution happens before any write, which is
/// what makes the cross-file write order a clean rollback (§9): the irreversible
/// source removal is the final step.
fn run(verb: Verb, args: &OpArgs) -> Result<String, SpanError> {
    let from_ctx = parse_context(&args.from_context, Role::From)?;
    let to_ctx = parse_context(&args.to_context, Role::To)?;
    let dest_ctx = parse_context(&args.dest_context, Role::Dest)?;

    // §9 step 1: existence is checked before reading content or writing anything
    // (Change 3 — a missing file is never implicitly created).
    ensure_exists(&args.file, false)?;
    let src_lines = read_lines(&args.file)?;

    // Forced delete-by-reference is keyed on `--force` (Decision 2), *not* on an
    // absent `--dest`: now that the destination number is optional, a guard-only
    // destination (`--dest-guard` with no `--dest` number) is also "no number" yet
    // is a relocation. `--force` is therefore the sole delete trigger. `run_delete`
    // rejects any destination addressing alongside it, and rejects `copy` (which
    // has no delete form). Removing the span by coordinate keeps its bytes out of
    // the model's context — the same thesis as a relocation, applied to a deletion.
    if args.force {
        return run_delete(verb, args, src_lines, &from_ctx, &to_ctx);
    }

    // A relocation needs a destination. With no `--force`, no `--dest` number, and
    // no `--dest-guard`, there is nothing to relocate to — a loud usage error
    // rather than a silent delete (only the explicit `--force` form deletes, §6).
    if args.dest.is_none() && args.dest_guard.is_none() {
        return Err(SpanError::Usage(
            "relocation requires a destination (--dest and/or --dest-guard); deleting the span instead requires --force".into(),
        ));
    }

    // Exactly one of --before/--after (§7) — required on the relocate path; the
    // delete form rejects both inside run_delete.
    let before = match (args.before, args.after) {
        (true, false) => true,
        (false, true) => false,
        _ => {
            return Err(SpanError::Usage(
                "exactly one of --before / --after is required".into(),
            ));
        }
    };

    // Decide intra- vs cross-file. A --dest-file that names the source is
    // intra-file so the overlap constraint still applies (§6).
    let cross = match &args.dest_file {
        Some(p) => !same_file(p, &args.file),
        None => false,
    };

    // For cross-file, read the (existing) destination up front so its guard
    // validates before any write. Intra-file destination is the source itself.
    let dest_lines_owned: Option<Lines> = if cross {
        let dp = args.dest_file.as_deref().unwrap();
        ensure_exists(dp, true)?;
        Some(read_lines(dp)?)
    } else {
        None
    };
    let (dest_path, dest_lines): (&str, &Lines) = if cross {
        (
            args.dest_file.as_deref().unwrap(),
            dest_lines_owned.as_ref().unwrap(),
        )
    } else {
        (args.file.as_str(), &src_lines)
    };

    // Change 1: resolve all three coordinates before reacting to any failure, so
    // a uniform line shift (which drifts from/to/dest at once) is reported in one
    // response instead of one serial round-trip per coordinate. A missing guard
    // flag is folded into `resolve` as the empty string, surfacing as the loud
    // `empty-guard` outcome rather than a silent panic.
    let from_guard = args.from_guard.as_deref().unwrap_or("");
    let to_guard = args.to_guard.as_deref().unwrap_or("");
    let from_res = resolve(
        &src_lines,
        &args.file,
        Role::From,
        args.from,
        from_guard,
        &from_ctx,
    );
    let to_res = resolve(&src_lines, &args.file, Role::To, args.to, to_guard, &to_ctx);
    // A dest *flag-shape* error (e.g. `--dest 0 --before`) is a usage error
    // (exit 2), a different class from a coordinate-guard failure, so it
    // short-circuits rather than joining the consolidated coordinate report.
    let dest_res = match resolve_dest(dest_lines, dest_path, args, &dest_ctx, before, args.dest) {
        Err(e) if e.code() == 2 => return Err(e),
        other => other,
    };

    // One failed coordinate → its standalone (rich) message; two or more → a
    // consolidated report naming every role, including the ones that passed
    // (Change 1). Order is fixed From, To, Dest.
    let outcomes = vec![
        RoleOutcome {
            role: Role::From,
            result: from_res,
        },
        RoleOutcome {
            role: Role::To,
            result: to_res,
        },
        RoleOutcome {
            role: Role::Dest,
            result: dest_res,
        },
    ];
    let failures = outcomes.iter().filter(|o| o.result.is_err()).count();
    if failures == 1 {
        let only = outcomes.into_iter().find_map(|o| o.result.err()).unwrap();
        return Err(only);
    } else if failures >= 2 {
        return Err(SpanError::Multi(outcomes));
    }
    let mut resolved = outcomes.into_iter();
    let from = resolved.next().unwrap().result.unwrap();
    let to = resolved.next().unwrap().result.unwrap();
    let dest_line = resolved.next().unwrap().result.unwrap();

    if from > to {
        return Err(SpanError::FromAfterTo { from, to });
    }

    // Boundary-blank *counts* of the source block, captured before any mutation
    // so the destination seam can mirror the source's exact blank-run width and
    // the source seam can collapse the removal-created doubling to
    // max(lead, trail) (Decision 1). from == 1 / to == line_count naturally yield
    // 0 — there is no run past a file edge.
    let src_leading_blanks = blank_run_above(&src_lines, from - 1);
    let src_trailing_blanks = blank_run_below(&src_lines, to);

    let block: Lines = src_lines[from - 1..=to - 1].to_vec();
    let blen = block.len();

    if cross {
        let mut dest_lines = dest_lines_owned.unwrap();
        let gap = gap_for(dest_line, before);

        // Build the destination result in memory before any write; mirroring may
        // synthesize a seam blank, so insert_block reports the moved block's
        // post-edit range directly.
        let dest_affected = insert_block(
            &mut dest_lines,
            gap,
            block,
            src_leading_blanks,
            src_trailing_blanks,
        );

        // §9 ordering: commit the destination durably first; only then remove
        // from the source. Any earlier failure leaves the source untouched.
        write_file(dest_path, &dest_lines)?;

        let mut out = String::new();
        if verb == Verb::Move {
            let mut src_after = src_lines;
            remove_block(&mut src_after, from, to);
            // Move-only: collapse the blank doubling the removal may have created
            // at the source gap, reducing it to max(lead, trail) (Decision 1).
            collapse_seam_blank(
                &mut src_after,
                from,
                src_leading_blanks,
                src_trailing_blanks,
            );
            write_file(&args.file, &src_after)?;

            out.push_str(&format!(
                "{} {} {} → {}:{}-{} (from {}:{}-{})\n\n",
                verb.summary_word(),
                blen,
                plural(blen),
                dest_path,
                dest_affected.0,
                dest_affected.1,
                args.file,
                from,
                to
            ));
            out.push_str(&format!("{}:\n", dest_path));
            render_affected(&mut out, &dest_lines, dest_affected.0, dest_affected.1);
            out.push_str(&format!(
                "\n{}:  ({} {} removed at {})\n",
                args.file,
                blen,
                plural(blen),
                from
            ));
            // After removal the line that followed the block now sits at `from`.
            render_seam(&mut out, &src_after, from);
        } else {
            out.push_str(&format!(
                "{} {} {} → {}:{}-{} (from {}:{}-{})\n",
                verb.summary_word(),
                blen,
                plural(blen),
                dest_path,
                dest_affected.0,
                dest_affected.1,
                args.file,
                from,
                to
            ));
            out.push_str(&format!("{}:\n", dest_path));
            render_affected(&mut out, &dest_lines, dest_affected.0, dest_affected.1);
        }
        return Ok(out);
    }

    // Intra-file: destination is the source file (the common case).
    // The destination must lie outside the span being moved/copied (§6).
    if dest_line != 0 && dest_line >= from && dest_line <= to {
        return Err(SpanError::DestInSpan {
            dest: dest_line,
            from,
            to,
        });
    }
    let gap = gap_for(dest_line, before);

    let mut lines = src_lines;
    let affected = if verb == Verb::Move {
        remove_block(&mut lines, from, to);
        // Map the original-frame gap onto the post-removal vector. The gap is
        // always outside the removed region, so it is either before the block
        // start (unchanged) or after the block end (shift left by block len).
        let insert_at = if gap < from { gap } else { gap - blen };
        // Order the two edits (destination insert, source-gap collapse) by
        // descending index so the earlier one never shifts the later (Change 2).
        if insert_at >= from {
            // Collapse the source seam *first*, while the block is removed but not
            // yet reinserted. The collapse only ever drains blank separator lines
            // at the merged source seam, and since no block bytes are present in
            // the vector during the drain, the moved block can never be corrupted.
            // (Inserting first and draining after silently deleted the block when
            // the dest addressed a whitespace-only line inside the source's own
            // trailing blank run — there the drain range overlapped the fresh
            // insert.) Then re-target the gap onto the post-collapse frame: the
            // collapse dropped `removed` lines starting at `from-1`, at or below
            // `insert_at >= from`, so the gap shifts left by `removed`. No
            // underflow — removed = min(lead,trail) <= lead <= from-1 < insert_at.
            let removed =
                collapse_seam_blank(&mut lines, from, src_leading_blanks, src_trailing_blanks);
            insert_block(
                &mut lines,
                insert_at - removed,
                block,
                src_leading_blanks,
                src_trailing_blanks,
            )
        } else {
            collapse_seam_blank(&mut lines, from, src_leading_blanks, src_trailing_blanks);
            insert_block(
                &mut lines,
                insert_at,
                block,
                src_leading_blanks,
                src_trailing_blanks,
            )
        }
    } else {
        insert_block(
            &mut lines,
            gap,
            block,
            src_leading_blanks,
            src_trailing_blanks,
        )
    };
    write_file(&args.file, &lines)?;

    let mut out = String::new();
    out.push_str(&format!(
        "{} {} {} → {}:{}-{}\n",
        verb.summary_word(),
        blen,
        plural(blen),
        args.file,
        affected.0,
        affected.1
    ));
    render_affected(&mut out, &lines, affected.0, affected.1);
    Ok(out)
}

/// The forced delete-by-reference form of `move` (Decision 2): triggered by
/// `--force` (now the sole trigger, since a guard-only `--dest` is also "no
/// number"), it removes `[from, to]` from the file and reinserts it nowhere — and
/// rejects any destination addressing supplied alongside `--force`. It exists because
/// deleting a large block through the string editor forces the model to reproduce
/// every line verbatim in `old_string` — the exact re-emit cost span removes; a
/// coordinate-addressed delete carries coordinates, not content.
///
/// `--force` is mandatory here (the explicit acknowledgment that content is being
/// removed), and `copy` has no such form. Crucially, `--force` does **not** weaken
/// the guard checks: `from`/`to` are resolved with the full §5 integrity
/// guarantee, or span would reintroduce the one thing it must never do — a silent
/// wrong-line delete.
fn run_delete(
    verb: Verb,
    args: &OpArgs,
    src_lines: Lines,
    from_ctx: &[ContextSpec],
    to_ctx: &[ContextSpec],
) -> Result<String, SpanError> {
    // Usage gates (exit 2). copy has no delete form; --force is the required
    // acknowledgment; destination-side flags are meaningless with no destination.
    if verb != Verb::Move {
        return Err(SpanError::Usage(
            "copy requires a destination; only `move` removes a span (and only with --force)"
                .into(),
        ));
    }
    // Routing reaches here only with --force (run keys delete on it, Decision 2),
    // so the old `!--force` gate is gone. Any destination addressing alongside
    // --force is contradictory — a delete has no destination — so reject a dest
    // number, a --dest-guard, or a --dest-context, mirroring the message the
    // relocate path used to emit. This keeps the contract intact: --force is valid
    // ONLY on the no-destination delete form.
    if args.dest.is_some() || args.dest_guard.is_some() || !args.dest_context.is_empty() {
        return Err(SpanError::Usage(
            "--force applies only when removing a span (a `move` with no destination)".into(),
        ));
    }
    if args.dest_file.is_some() {
        return Err(SpanError::Usage(
            "--dest-file is meaningless when removing a span (there is no destination)".into(),
        ));
    }
    if args.before || args.after {
        return Err(SpanError::Usage(
            "--before / --after are meaningless when removing a span (there is no destination)"
                .into(),
        ));
    }

    // Resolve from/to with the full integrity guarantee (§5) — unchanged by
    // --force. Consolidate failures across the two coordinates, reusing the
    // relocate path's RoleOutcome/Multi machinery (here without Dest), so a
    // uniform shift that drifts both endpoints is reported in one response.
    let from_guard = args.from_guard.as_deref().unwrap_or("");
    let to_guard = args.to_guard.as_deref().unwrap_or("");
    let from_res = resolve(
        &src_lines,
        &args.file,
        Role::From,
        args.from,
        from_guard,
        from_ctx,
    );
    let to_res = resolve(&src_lines, &args.file, Role::To, args.to, to_guard, to_ctx);
    let outcomes = vec![
        RoleOutcome {
            role: Role::From,
            result: from_res,
        },
        RoleOutcome {
            role: Role::To,
            result: to_res,
        },
    ];
    let failures = outcomes.iter().filter(|o| o.result.is_err()).count();
    if failures == 1 {
        return Err(outcomes.into_iter().find_map(|o| o.result.err()).unwrap());
    } else if failures >= 2 {
        return Err(SpanError::Multi(outcomes));
    }
    let mut it = outcomes.into_iter();
    let from = it.next().unwrap().result.unwrap();
    let to = it.next().unwrap().result.unwrap();
    if from > to {
        return Err(SpanError::FromAfterTo { from, to });
    }

    // Source-seam blank counts, captured before mutation, so the collapse reduces
    // the removal-created doubling to max(lead, trail) (Decision 1) exactly as on
    // the relocate path.
    let lead = blank_run_above(&src_lines, from - 1);
    let trail = blank_run_below(&src_lines, to);
    let blen = to - from + 1;

    let mut lines = src_lines;
    remove_block(&mut lines, from, to);
    collapse_seam_blank(&mut lines, from, lead, trail);
    write_file(&args.file, &lines)?;

    let mut out = format!(
        "deleted {} {} from {}:{}-{}\n",
        blen,
        plural(blen),
        args.file,
        from,
        to
    );
    // The line that followed the removed block now sits at `from`; show the seam.
    render_seam(&mut out, &lines, from);
    Ok(out)
}

/// Parse arguments, dispatch the verb, and map results onto stdout/stderr and
/// the §7 exit codes. Nothing is printed to stdout unless the call succeeds.
fn main() {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Move(args) => run(Verb::Move, args),
        Command::Copy(args) => run(Verb::Copy, args),
    };
    match result {
        Ok(output) => {
            print!("{}", output);
            std::process::exit(0);
        }
        Err(err) => {
            eprintln!("{}", err.message());
            std::process::exit(err.code());
        }
    }
}
