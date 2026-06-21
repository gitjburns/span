//! Integration tests for the `span-guard` PreToolUse hook (§10, Decisions 2 & 4).
//!
//! These drive the compiled `span-guard` binary exactly as Claude Code does: a
//! `PreToolUse` JSON payload on stdin, the hook's decision JSON on stdout (or
//! empty for a silent allow), always exit 0. Fixtures live in a throwaway
//! tempdir whose path is passed as `cwd`, so the workspace twin-scan is isolated
//! to the test. We assert on the hook's *decision* and the assembled `span`
//! command, which is the hook's externally observable contract.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

/// Run span-guard with `payload` on stdin. Returns the parsed `hookSpecificOutput`
/// object, or `None` for a silent allow (empty stdout). Asserts exit 0 — the hook
/// never signals via status, only via the JSON body (§10, fail-open).
fn decide(payload: &Value) -> Option<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_span-guard"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn span-guard");
    child
        .stdin
        .take()
        .expect("stdin handle")
        .write_all(payload.to_string().as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait span-guard");
    assert_eq!(out.status.code(), Some(0), "span-guard must always exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(trimmed).expect("hook output must be JSON");
    Some(v["hookSpecificOutput"].clone())
}

/// The deny reason string, or panic if the decision was not a deny.
fn deny_reason(out: &Value) -> String {
    assert_eq!(
        out["permissionDecision"].as_str(),
        Some("deny"),
        "expected a deny decision, got: {out}"
    );
    out["permissionDecisionReason"]
        .as_str()
        .expect("deny reason string")
        .to_string()
}

/// Create a scratch workspace dir and write `name` with `contents`.
fn workspace(name: &str, contents: &str) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(name), contents).expect("write fixture");
    dir
}

/// An `Edit` deletion payload (`old_string` → "") against `file` in `dir`.
fn delete_payload(dir: &Path, file: &str, old: &str) -> Value {
    json!({
        "cwd": dir.to_str().unwrap(),
        "tool_name": "Edit",
        "tool_input": {
            "file_path": dir.join(file).to_str().unwrap(),
            "old_string": old,
            "new_string": "",
        }
    })
}

/// A `Write` insertion payload putting `content` into `file` in `dir`.
fn write_payload(dir: &Path, file: &str, content: &str) -> Value {
    json!({
        "cwd": dir.to_str().unwrap(),
        "tool_name": "Write",
        "tool_input": {
            "file_path": dir.join(file).to_str().unwrap(),
            "content": content,
        }
    })
}

// A 14-line module: a 12-line `helper` fn ending in a bare `}` (line 12) plus a
// second `}` on line 14 — so the block's closing-brace endpoint is ambiguous and
// must borrow context (Decision 4).
const HELPER_MOD: &str = "fn helper() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    let e = 5;\n    let f = 6;\n    let g = 7;\n    let h = 8;\n    let i = 9;\n    let j = 10;\n}\nfn other() {\n}\n";

/// The first 12 lines of `HELPER_MOD` (the helper fn, ending in `}`), terminated
/// — the `old_string` a hand-rolled deletion of that fn would carry.
fn helper_fn() -> String {
    let lines: Vec<&str> = HELPER_MOD.lines().collect();
    format!("{}\n", lines[..12].join("\n"))
}

// ---------------------------------------------------------------------------
// Decision 2 (hook side): large deletions are hard-denied with a move/force fork
// ---------------------------------------------------------------------------

/// A large editor deletion (>= DELETE_DENY_MIN_LINES = 10) is hard-denied with a
/// fork: a relocate command (`--dest …`) and a delete command (`--force`), both
/// addressed by reference. No twin is required (delete-first order hides it).
#[test]
fn large_deletion_denied_with_fork() {
    let dir = workspace("mod.rs", HELPER_MOD);
    let out = decide(&delete_payload(dir.path(), "mod.rs", &helper_fn()))
        .expect("a 12-line deletion must not be a silent allow");
    let reason = deny_reason(&out);

    // The fork offers both halves by reference.
    assert!(reason.contains("span move mod.rs"), "reason: {reason}");
    assert!(
        reason.contains("--dest <LINE>"),
        "relocate arm missing: {reason}"
    );
    assert!(reason.contains("--force"), "delete arm missing: {reason}");
    // Coordinates span the whole 12-line block.
    assert!(reason.contains("--from 1"), "reason: {reason}");
    assert!(reason.contains("--to 12"), "reason: {reason}");
    // The unambiguous opening line anchors --from directly.
    assert!(
        reason.contains("--from-guard \"fn helper() {\""),
        "reason: {reason}"
    );
}

/// The assembled command auto-attaches a `--to-context` neighbor because the
/// block's closing `}` is ambiguous (it also matches line 14) — so the model's
/// first `span` run resolves uniquely instead of failing `ambiguous-anchor`
/// (Decision 4).
#[test]
fn delete_fork_auto_attaches_context_for_ambiguous_endpoint() {
    let dir = workspace("mod.rs", HELPER_MOD);
    let reason = deny_reason(&decide(&delete_payload(dir.path(), "mod.rs", &helper_fn())).unwrap());

    assert!(reason.contains("--to-guard \"}\""), "reason: {reason}");
    // The neighbor just inside the block disambiguates the brace.
    assert!(
        reason.contains("--to-context \"-1:let j = 10;\""),
        "expected auto context for ambiguous brace: {reason}"
    );
}

/// A sub-threshold deletion (3 <= n < 10) with no verbatim twin is a silent
/// allow: a plain dead-code removal must not be interrupted.
#[test]
fn small_deletion_without_twin_is_silent() {
    let dir = workspace(
        "code.rs",
        "fn f() {\n    uniq_one();\n    uniq_two();\n    uniq_three();\n}\n",
    );
    let old = "    uniq_one();\n    uniq_two();\n    uniq_three();\n";
    assert!(
        decide(&delete_payload(dir.path(), "code.rs", old)).is_none(),
        "a 3-line no-twin deletion should pass silently"
    );
}

// ---------------------------------------------------------------------------
// Decision 4 / §10: insertion gate — twin-gated deny with unique guards
// ---------------------------------------------------------------------------

/// A genuinely new large block (no verbatim twin anywhere) passes silently: the
/// insertion gate only fires on the relocation tell, never on fresh authoring.
#[test]
fn new_large_block_without_twin_is_silent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let content: String = (0..20)
        .map(|i| format!("brand_new_unique_line_{i}\n"))
        .collect();
    assert!(
        decide(&write_payload(dir.path(), "new.rs", &content)).is_none(),
        "a new block with no twin should pass silently"
    );
}

/// Writing a block that already exists verbatim in another file is the
/// destination half of a move → hard deny, with the source coordinates assembled
/// and the ambiguous `}` endpoint auto-disambiguated (Decision 4).
#[test]
fn insertion_with_twin_denied_and_uniquely_anchored() {
    // The relocation source lives in mod.rs; the model tries to Write it verbatim
    // into a fresh dest.rs.
    let dir = workspace("mod.rs", HELPER_MOD);
    let reason = deny_reason(
        &decide(&write_payload(dir.path(), "dest.rs", &helper_fn()))
            .expect("inserting a twinned block must deny"),
    );

    assert!(
        reason.contains("already exists verbatim at mod.rs:1-12"),
        "reason: {reason}"
    );
    assert!(reason.contains("span move mod.rs"), "reason: {reason}");
    // The closing-brace endpoint is auto-anchored the same way as on the delete
    // fork, since the twin file holds a second `}`.
    assert!(
        reason.contains("--to-context \"-1:let j = 10;\""),
        "expected unique anchor on insertion deny: {reason}"
    );
}

/// Malformed stdin must fail open: no JSON, no output, exit 0 (the hook can never
/// break editing, §10).
#[test]
fn malformed_input_fails_open() {
    let payload = json!({ "not": "a tool call" });
    assert!(
        decide(&payload).is_none(),
        "unknown payload should be silent"
    );
}

/// Regression (review F2): when the deleted block appears MORE THAN ONCE in the
/// edited file (the write-first move state — destination copy already written,
/// source still present), the hook must NOT hand back a confident `--force`
/// command that could address the wrong copy and silently invert the move. It
/// degrades to a coordinate-free reminder (or silent allow) instead.
#[test]
fn duplicated_block_deletion_never_emits_wrong_target_command() {
    // The 12-line helper fn body appears verbatim twice: a destination copy above
    // and the source copy below.
    let body = HELPER_MOD.lines().take(12).collect::<Vec<_>>().join("\n");
    let file = format!("mod dest_region {{\n{body}\n}}\nmod source_region {{\n{body}\n}}\n");
    let dir = workspace("mod.rs", &file);

    // Delete the (second) source copy of the body.
    match decide(&delete_payload(dir.path(), "mod.rs", &body)) {
        // Silent allow is acceptable — it is never a wrong command.
        None => {}
        Some(out) => {
            // Whatever is emitted must NOT be a deny carrying a copy-pasteable
            // command for an arbitrary occurrence — a reminder (which may mention
            // `span move` as general advice) is fine, but it must never embed an
            // assembled command with coordinates or --force.
            assert_ne!(
                out["permissionDecision"].as_str(),
                Some("deny"),
                "must not hard-deny a duplicated-block deletion with a wrong-target command: {out}"
            );
            let ctx = out["additionalContext"].as_str().unwrap_or("");
            assert!(
                !ctx.contains("--force") && !ctx.contains("--from"),
                "a reminder must not embed assembled command coordinates: {ctx}"
            );
        }
    }
}

/// Regression (review F3): a fully-duplicated block whose only unique
/// disambiguator sits beyond the old ±3 window must still receive a working
/// `--*-context` (the outward scan reaches it), so the emitted command would
/// resolve on the model's first run instead of failing `ambiguous-anchor`.
#[test]
fn fully_duplicated_block_gets_far_context_anchor() {
    // dup1..dup5 appears twice; the sole unique line (MIDDLE_UNIQUE) sits 5 lines
    // from the top block's `from` endpoint — beyond ±3.
    let dir = workspace(
        "src.txt",
        "dup1\ndup2\ndup3\ndup4\ndup5\nMIDDLE_UNIQUE\ndup1\ndup2\ndup3\ndup4\ndup5\n",
    );
    // Inserting the 5-line block into a fresh file is the destination tell → deny
    // against the twin in src.txt.
    let reason = deny_reason(
        &decide(&write_payload(
            dir.path(),
            "dest.txt",
            "dup1\ndup2\ndup3\ndup4\ndup5\n",
        ))
        .expect("a twinned insertion must deny"),
    );
    // The ambiguous `dup1` from-endpoint is anchored by a neighbor past ±3.
    assert!(
        reason.contains("--from-context"),
        "no context emitted: {reason}"
    );
    assert!(
        reason.contains("MIDDLE_UNIQUE"),
        "far disambiguator not used: {reason}"
    );
}
