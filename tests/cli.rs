//! Integration tests for the `span` binary (§ enhancements 1–4).
//!
//! These drive the *compiled* executable via `env!("CARGO_BIN_EXE_span")` rather
//! than calling library functions, because `span`'s contract is its CLI: argv
//! parsing, exit codes, stderr wording, and — above all — the exact bytes it
//! leaves on disk. Each test writes a fixture into a throwaway `tempfile::tempdir`
//! and asserts on the resulting bytes (exact, hand-verified against the binary)
//! and/or the loud-failure stderr (substring). The tool is stateless and treats
//! disk as canonical, so a fresh dir per test is full isolation.

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Run the compiled binary inside `dir`, returning `(exit_code, stdout, stderr)`.
/// Running with `current_dir(dir)` lets fixtures use bare filenames, which also
/// exercises `write_file`'s "bare filename → current directory" branch (§9).
fn run(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_span"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn span binary");
    (
        out.status.code().expect("span terminated by signal"),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Create a scratch dir and write `name` with exact `bytes`.
fn fixture(name: &str, bytes: &[u8]) -> TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    fs::write(dir.path().join(name), bytes).expect("write fixture");
    dir
}

/// Read a fixture file's raw bytes back.
fn read(dir: &Path, name: &str) -> Vec<u8> {
    fs::read(dir.join(name)).expect("read result file")
}

/// Render bytes with visible escapes so a byte-mismatch panic is legible.
fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace('\r', "\\r")
        .replace('\n', "\\n\n")
        .replace('\t', "\\t")
}

/// Assert exact bytes, printing both sides with visible escapes on mismatch.
fn assert_bytes(actual: &[u8], expected: &[u8]) {
    assert!(
        actual == expected,
        "byte mismatch\n--- expected ---\n{}\n--- actual ---\n{}",
        show(expected),
        show(actual)
    );
}

// ---------------------------------------------------------------------------
// 1. Consolidated coordinate validation (Change 1)
// ---------------------------------------------------------------------------

/// A uniform downward shift drifts from/to/dest at once. They must all be
/// reported together (exit 1, "3 coordinates failed") rather than one per
/// round-trip, and the file must be untouched.
#[test]
fn consolidated_drift_reports_all_three() {
    let dir = fixture(
        "f.rs",
        b"pad\npad\nfn parse() {\n    return 1;\n}\n\nfn other() {\n}\n",
    );
    let before = read(dir.path(), "f.rs");

    let (code, _out, err) = run(
        dir.path(),
        &[
            "move",
            "f.rs",
            "--from",
            "1",
            "--from-guard",
            "fn parse",
            "--to",
            "3",
            "--to-guard",
            "}",
            "--to-context",
            "-1:return 1",
            "--dest",
            "5",
            "--dest-guard",
            "fn other",
            "--after",
        ],
    );

    assert_eq!(code, 1, "stderr was: {err}");
    assert!(err.contains("3 coordinates failed"), "stderr: {err}");
    assert!(err.contains("not 1"), "stderr: {err}");
    assert!(err.contains("not 3"), "stderr: {err}");
    assert!(err.contains("not 5"), "stderr: {err}");
    // Nothing is written unless the call exits 0.
    assert_bytes(&read(dir.path(), "f.rs"), &before);
}

// ---------------------------------------------------------------------------
// 2. Boundary-blank mirroring, default-on (Change 2)
// ---------------------------------------------------------------------------

/// Moving a blank-delimited section reproduces its leading blank at the seam
/// without doubling the blank already present at the destination — net result is
/// a clean blank-delimited block, no triple newline.
#[test]
fn mirror_leading_blank_without_doubling() {
    let dir = fixture("d.md", b"# T\n\n## A\n\nbody A\n\n## B\n\nbody B\n");

    let (code, _out, err) = run(
        dir.path(),
        &[
            "move",
            "d.md",
            "--from",
            "7",
            "--from-guard",
            "## B",
            "--to",
            "9",
            "--to-guard",
            "body B",
            "--dest",
            "1",
            "--dest-guard",
            "# T",
            "--after",
        ],
    );

    assert_eq!(code, 0, "stderr: {err}");
    let result = read(dir.path(), "d.md");
    // One synthesized leading blank before `## B`; the blank between `bodyB` and
    // `## A` is the destination's own pre-existing blank (not doubled). The single
    // trailing blank is the source-seam leftover: the moved block was the file's
    // tail and blank-delimited above, so removing it leaves that separator-blank
    // dangling at EOF — `collapse_seam_blank` only de-doubles, so it survives.
    assert_bytes(&result, b"# T\n\n## B\n\nbody B\n\n## A\n\nbody A\n\n");
    // The key no-doubling property still holds: nowhere are there three in a row.
    assert!(
        !result.windows(3).any(|w| w == b"\n\n\n"),
        "unexpected triple newline"
    );
}

/// Moving a line within a tight, blank-free code block must NOT synthesize any
/// blank: mirroring is self-gating on the source being blank-delimited.
#[test]
fn contiguous_code_injects_no_blank() {
    let dir = fixture(
        "f.rs",
        b"fn f() {\n    a();\n    b();\n    c();\n    d();\n}\n",
    );

    let (code, _out, err) = run(
        dir.path(),
        &[
            "move",
            "f.rs",
            "--from",
            "2",
            "--from-guard",
            "a();",
            "--to",
            "2",
            "--to-guard",
            "a();",
            "--dest",
            "4",
            "--dest-guard",
            "c();",
            "--after",
        ],
    );

    assert_eq!(code, 0, "stderr: {err}");
    assert_bytes(
        &read(dir.path(), "f.rs"),
        b"fn f() {\n    b();\n    c();\n    a();\n    d();\n}\n",
    );
}

/// Move-only: removing a block that was blank on both sides leaves two adjacent
/// blanks at the source seam; the collapse caps that doubling at one blank.
#[test]
fn source_gap_collapses_to_single_blank() {
    let dir = fixture("d.md", b"alpha\n\nMID\n\nomega\n");

    let (code, _out, err) = run(
        dir.path(),
        &[
            "move",
            "d.md",
            "--from",
            "3",
            "--from-guard",
            "MID",
            "--to",
            "3",
            "--to-guard",
            "MID",
            "--dest",
            "0",
            "--after",
        ],
    );

    assert_eq!(code, 0, "stderr: {err}");
    // MID lands at top (no leading blank, but a trailing blank mirrored below it);
    // the alpha/omega gap collapses from two blanks back to one.
    assert_bytes(&read(dir.path(), "d.md"), b"MID\n\nalpha\n\nomega\n");
}

/// Two seam edges in one test: a `--dest 0` top insert adds no blank *above* the
/// block (no preceding line), and an EOF append preserves a missing trailing
/// newline while still mirroring the source's leading blank before the appendee.
#[test]
fn dest_zero_and_eof_edges() {
    // dest-0: no leading blank synthesized at the very top of file.
    let top = fixture("a.md", b"one\ntwo\n\nthree\n");
    let (code, _o, err) = run(
        top.path(),
        &[
            "move",
            "a.md",
            "--from",
            "4",
            "--from-guard",
            "three",
            "--to",
            "4",
            "--to-guard",
            "three",
            "--dest",
            "0",
            "--after",
        ],
    );
    assert_eq!(code, 0, "stderr: {err}");
    assert_bytes(&read(top.path(), "a.md"), b"three\none\ntwo\n\n");

    // EOF append: source has no trailing newline; appended copy mirrors the
    // source's leading blank and the file still ends without a newline.
    let eof = fixture("b.md", b"x\n\ny\nLAST");
    let (code, _o, err) = run(
        eof.path(),
        &[
            "copy",
            "b.md",
            "--from",
            "3",
            "--from-guard",
            "y",
            "--to",
            "3",
            "--to-guard",
            "y",
            "--dest",
            "4",
            "--dest-guard",
            "LAST",
            "--after",
        ],
    );
    assert_eq!(code, 0, "stderr: {err}");
    assert_bytes(&read(eof.path(), "b.md"), b"x\n\ny\nLAST\n\ny");
}

/// A CRLF file keeps CRLF everywhere, and the synthesized seam blank is `\r\n`,
/// not a lone `\n` (§4 separator-convention rule).
#[test]
fn crlf_synthesized_blank_uses_crlf() {
    let dir = fixture("d.md", b"A\r\nB\r\n\r\nC\r\n");

    let (code, _out, err) = run(
        dir.path(),
        &[
            "move",
            "d.md",
            "--from",
            "4",
            "--from-guard",
            "C",
            "--to",
            "4",
            "--to-guard",
            "C",
            "--dest",
            "1",
            "--dest-guard",
            "A",
            "--after",
        ],
    );

    assert_eq!(code, 0, "stderr: {err}");
    let result = read(dir.path(), "d.md");
    assert_bytes(&result, b"A\r\n\r\nC\r\nB\r\n\r\n");
    // No lone LF: every `\n` must be immediately preceded by `\r`.
    for (i, &b) in result.iter().enumerate() {
        if b == b'\n' {
            assert!(i > 0 && result[i - 1] == b'\r', "lone \\n at byte {i}");
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Destination-file existence (Change 3)
// ---------------------------------------------------------------------------

/// A `--dest-file` that does not exist is a loud I/O failure (exit 3), never an
/// implicit create. The source is untouched and the missing path stays absent.
#[test]
fn missing_dest_file_exits_3_and_writes_nothing() {
    let dir = fixture("src.md", b"a\nb\nc\n");
    let before = read(dir.path(), "src.md");

    let (code, _out, err) = run(
        dir.path(),
        &[
            "copy",
            "src.md",
            "--from",
            "1",
            "--from-guard",
            "a",
            "--to",
            "1",
            "--to-guard",
            "a",
            "--dest",
            "1",
            "--dest-guard",
            "NOPE",
            "--after",
            "--dest-file",
            "nonexistent.md",
        ],
    );

    assert_eq!(code, 3, "stderr: {err}");
    assert!(err.contains("does not exist"), "stderr: {err}");
    assert_bytes(&read(dir.path(), "src.md"), &before);
    assert!(
        !dir.path().join("nonexistent.md").exists(),
        "span must not create the missing destination"
    );
}

// ---------------------------------------------------------------------------
// 4. Self-correcting ambiguous anchor (Change 4)
// ---------------------------------------------------------------------------

/// A guard matching several lines fails loud (exit 1) and lists each candidate's
/// number and content so the agent can pick a `--*-context` without re-reading.
/// Only one coordinate is ambiguous here, so the standalone (rich) message form
/// is used, not the consolidated report.
#[test]
fn ambiguous_anchor_lists_candidates() {
    let dir = fixture("f.rs", b"a {\n}\nb {\n}\nc {\n}\n");
    let before = read(dir.path(), "f.rs");

    let (code, _out, err) = run(
        dir.path(),
        &[
            "move",
            "f.rs",
            "--from",
            "1",
            "--from-guard",
            "a {",
            "--to",
            "2",
            "--to-guard",
            "}",
            "--dest",
            "5",
            "--dest-guard",
            "c {",
            "--after",
        ],
    );

    assert_eq!(code, 1, "stderr: {err}");
    assert!(err.contains("ambiguous-anchor"), "stderr: {err}");
    // Candidate rows: the three `}` lines at 2/4/6.
    assert!(err.contains("2\t}"), "missing candidate row 2: {err}");
    assert!(err.contains("4\t}"), "missing candidate row 4: {err}");
    assert!(err.contains("6\t}"), "missing candidate row 6: {err}");
    assert_bytes(&read(dir.path(), "f.rs"), &before);
}

// ---------------------------------------------------------------------------
// 9. Cross-file copy: source byte-identical, destination mirrored
// ---------------------------------------------------------------------------

/// `copy` across files must leave the source byte-for-byte identical (it only
/// transfers bytes by reference) while the destination receives the block with
/// boundary blanks mirrored around it.
#[test]
fn cross_file_copy_leaves_source_identical() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = b"# T\n\n## A\n\nbody A\n\n## B\n\nbody B\n";
    fs::write(dir.path().join("c.md"), src).expect("write source");
    fs::write(dir.path().join("dest.md"), b"HEADER\n").expect("write dest");

    let (code, _out, err) = run(
        dir.path(),
        &[
            "copy",
            "c.md",
            "--from",
            "7",
            "--from-guard",
            "## B",
            "--to",
            "9",
            "--to-guard",
            "body B",
            "--dest",
            "1",
            "--dest-guard",
            "HEADER",
            "--after",
            "--dest-file",
            "dest.md",
        ],
    );

    assert_eq!(code, 0, "stderr: {err}");
    assert_bytes(&read(dir.path(), "c.md"), src);
    // dest receives the `## B` block with a mirrored blank above the heading.
    assert_bytes(&read(dir.path(), "dest.md"), b"HEADER\n\n## B\n\nbody B\n");
}

// ---------------------------------------------------------------------------
// Decision 1: count-based blank-run preservation (PEP 8 double blanks)
// ---------------------------------------------------------------------------

/// Moving a block delimited by *two* blank lines (the PEP 8 top-level `def`
/// case) must reproduce both blanks at the destination seam and collapse the
/// removal-created doubling at the source seam to `max(lead, trail) == 2` — never
/// to one (the legacy single-blank behavior would corrupt PEP 8 spacing). The
/// destination here is *below* the source seam, exercising the
/// `(s - removed, e - removed)` index adjustment with `removed == 2`.
#[test]
fn move_preserves_double_blank_seams() {
    let dir = fixture(
        "e.py",
        b"def a():\n    pass\n\n\ndef b():\n    pass\n\n\ndef c():\n    pass\n",
    );

    let (code, out, err) = run(
        dir.path(),
        &[
            "move",
            "e.py",
            "--from",
            "5",
            "--from-guard",
            "def b",
            // `pass` repeats on lines 2/6/10; borrow entropy from the def above.
            "--to",
            "6",
            "--to-guard",
            "pass",
            "--to-context",
            "-1:def b",
            "--dest",
            "10",
            "--dest-guard",
            "pass",
            "--dest-context",
            "-1:def c",
            "--after",
        ],
    );

    assert_eq!(code, 0, "stderr: {err}");
    // The moved block is renumbered to 9-10 (after the 2-line source collapse).
    assert!(out.contains("e.py:9-10"), "summary: {out}");
    // def b lands after def c with 2 blanks above it (source count preserved);
    // the def a / def c seam keeps exactly 2 blanks (collapsed from the merged 4).
    assert_bytes(
        &read(dir.path(), "e.py"),
        b"def a():\n    pass\n\n\ndef c():\n    pass\n\n\ndef b():\n    pass\n",
    );
    // Two blank lines is the max anywhere: never a third (no quadruple newline).
    assert!(
        !read(dir.path(), "e.py")
            .windows(4)
            .any(|w| w == b"\n\n\n\n"),
        "a blank run grew past 2"
    );
}

/// The double-blank rule honors CRLF: synthesized seam blanks use the
/// destination's `\r\n`, never a lone `\n`, and the source seam collapses to two
/// `\r\n` blanks. Moving to the top exercises the collapse-then-insert branch.
#[test]
fn crlf_double_blank_seams_use_crlf() {
    let dir = fixture(
        "e.py",
        b"def a():\r\n    pass\r\n\r\n\r\ndef b():\r\n    pass\r\n\r\n\r\ndef c():\r\n    pass\r\n",
    );

    let (code, _out, err) = run(
        dir.path(),
        &[
            "move",
            "e.py",
            "--from",
            "5",
            "--from-guard",
            "def b",
            "--to",
            "6",
            "--to-guard",
            "pass",
            "--to-context",
            "-1:def b",
            "--dest",
            "0",
            "--after",
        ],
    );

    assert_eq!(code, 0, "stderr: {err}");
    let result = read(dir.path(), "e.py");
    assert_bytes(
        &result,
        b"def b():\r\n    pass\r\n\r\n\r\ndef a():\r\n    pass\r\n\r\n\r\ndef c():\r\n    pass\r\n",
    );
    // No lone LF: every `\n` must be immediately preceded by `\r`.
    for (i, &b) in result.iter().enumerate() {
        if b == b'\n' {
            assert!(i > 0 && result[i - 1] == b'\r', "lone \\n at byte {i}");
        }
    }
}

// ---------------------------------------------------------------------------
// Decision 2: forced delete-by-reference (degenerate no-dest `move`)
// ---------------------------------------------------------------------------

/// `span move <file> --from .. --to .. --force` with no `--dest` removes the span
/// by reference: byte-exact removal, source-seam collapse, and a `deleted …`
/// summary. The block here is blank-delimited on both sides (lead=trail=1), so
/// the collapse caps the merged run at one blank.
#[test]
fn forced_delete_removes_span_by_reference() {
    let dir = fixture("a.txt", b"alpha\n\nMID1\nMID2\n\nomega\n");

    let (code, out, err) = run(
        dir.path(),
        &[
            "move",
            "a.txt",
            "--from",
            "3",
            "--from-guard",
            "MID1",
            "--to",
            "4",
            "--to-guard",
            "MID2",
            "--force",
        ],
    );

    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("deleted 2 lines from a.txt:3-4"),
        "summary: {out}"
    );
    // The two former separator-blanks collapse to a single blank at the seam.
    assert_bytes(&read(dir.path(), "a.txt"), b"alpha\n\nomega\n");
}

/// A forced delete of the file's tail preserves its (missing) trailing-newline
/// state: the new last line must not silently gain a terminator.
#[test]
fn forced_delete_preserves_trailing_newline_state() {
    // No final newline; delete the last line.
    let dir = fixture("a.txt", b"keep1\nkeep2\nGONE");

    let (code, _out, err) = run(
        dir.path(),
        &[
            "move",
            "a.txt",
            "--from",
            "3",
            "--from-guard",
            "GONE",
            "--to",
            "3",
            "--to-guard",
            "GONE",
            "--force",
        ],
    );

    assert_eq!(code, 0, "stderr: {err}");
    // keep2 was terminated; after removing the unterminated tail it must inherit
    // the file's no-trailing-newline state.
    assert_bytes(&read(dir.path(), "a.txt"), b"keep1\nkeep2");
}

/// The forced-delete form's usage gates, each a clean exit 2 that writes nothing:
/// no `--dest` without `--force`; `--force` with a `--dest`; `copy` with no dest;
/// `--before`/`--after` with no dest.
#[test]
fn forced_delete_usage_errors() {
    let dir = fixture("f.txt", b"a\nb\nc\n");
    let before = read(dir.path(), "f.txt");
    let base = [
        "f.txt",
        "--from",
        "1",
        "--from-guard",
        "a",
        "--to",
        "1",
        "--to-guard",
        "a",
    ];

    // (1) no dest, no --force → must demand --force.
    let (code, _o, err) = run(dir.path(), &[&["move"], &base[..]].concat());
    assert_eq!(code, 2, "stderr: {err}");
    assert!(err.contains("requires --force"), "stderr: {err}");

    // (2) --force together with a destination → rejected.
    let (code, _o, err) = run(
        dir.path(),
        &[
            &["move"],
            &base[..],
            &["--dest", "3", "--dest-guard", "c", "--after", "--force"],
        ]
        .concat(),
    );
    assert_eq!(code, 2, "stderr: {err}");
    assert!(err.contains("--force applies only"), "stderr: {err}");

    // (3) copy with no destination → rejected (copy has no delete form).
    let (code, _o, err) = run(dir.path(), &[&["copy"], &base[..], &["--force"]].concat());
    assert_eq!(code, 2, "stderr: {err}");
    assert!(err.contains("copy requires a destination"), "stderr: {err}");

    // (4) --before/--after are meaningless with no destination.
    let (code, _o, err) = run(
        dir.path(),
        &[&["move"], &base[..], &["--force", "--after"]].concat(),
    );
    assert_eq!(code, 2, "stderr: {err}");
    assert!(err.contains("meaningless when removing"), "stderr: {err}");

    // Every rejection left the file untouched.
    assert_bytes(&read(dir.path(), "f.txt"), &before);
}

/// `--force` must NOT weaken the guard checks: a drifted endpoint still fails loud
/// (exit 1) and writes nothing, so a forced delete can never hit the wrong line.
#[test]
fn forced_delete_still_guards_loud() {
    let dir = fixture("f.rs", b"pad\nfn parse() {\n    body();\n}\n");
    let before = read(dir.path(), "f.rs");

    // `fn parse` actually lives on line 2, but we address line 1 → drift.
    let (code, _o, err) = run(
        dir.path(),
        &[
            "move",
            "f.rs",
            "--from",
            "1",
            "--from-guard",
            "fn parse",
            "--to",
            "4",
            "--to-guard",
            "}",
            "--force",
        ],
    );

    assert_eq!(code, 1, "stderr: {err}");
    assert!(err.contains("drift"), "stderr: {err}");
    assert_bytes(&read(dir.path(), "f.rs"), &before);
}

/// Regression (review F1): moving a block to *after a whitespace-only line inside
/// its own trailing blank run* must preserve the moved block byte-exact. A tab
/// line is `is_blank` (so it counts toward the seam) yet guardable (so it can be
/// the dest), landing the insertion inside the source's trailing run — the seam
/// collapse must run before the insert so it can never drain the relocated block.
#[test]
fn move_into_own_trailing_blank_run_preserves_block() {
    // Lines: A, '', '', B, C, '\t', '', D, E. Block [B,C] has lead=2, trail=2.
    let dir = fixture("t.txt", b"A\n\n\nB\nC\n\t\n\nD\nE\n");

    let (code, out, err) = run(
        dir.path(),
        &[
            "move",
            "t.txt",
            "--from",
            "4",
            "--from-guard",
            "B",
            "--to",
            "5",
            "--to-guard",
            "C",
            "--dest",
            "6",
            "--dest-guard",
            "\t",
            "--after",
        ],
    );

    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("moved 2 lines"), "summary: {out}");
    // B and C survive intact; the tab blank normalizes to a plain separator blank.
    // (Before the fix this silently deleted B.)
    assert_bytes(&read(dir.path(), "t.txt"), b"A\n\n\nB\nC\n\n\nD\nE\n");
}

/// Regression (review F5): a cross-file MOVE of a double-blank-delimited block
/// tops the destination seam up to the source's 2-blank width and collapses the
/// source seam to max(lead,trail)=2, leaving the source otherwise byte-exact.
/// (The only prior cross-file test was a single-blank copy.)
#[test]
fn cross_file_move_preserves_double_blank_seams() {
    let dir = fixture(
        "e.py",
        b"def a():\n    pass\n\n\ndef b():\n    pass\n\n\ndef c():\n    pass\n",
    );
    fs::write(dir.path().join("dest.py"), b"HEADER\n").expect("write dest");

    let (code, _out, err) = run(
        dir.path(),
        &[
            "move",
            "e.py",
            "--from",
            "5",
            "--from-guard",
            "def b",
            // `pass` repeats on lines 2/6/10; disambiguate with the def above.
            "--to",
            "6",
            "--to-guard",
            "pass",
            "--to-context",
            "-1:def b",
            "--dest",
            "1",
            "--dest-guard",
            "HEADER",
            "--after",
            "--dest-file",
            "dest.py",
        ],
    );

    assert_eq!(code, 0, "stderr: {err}");
    // Destination seam topped up to 2 leading blanks (the source's run width).
    assert_bytes(
        &read(dir.path(), "dest.py"),
        b"HEADER\n\n\ndef b():\n    pass\n",
    );
    // Source seam collapsed to max(2,2)=2; def a / def c otherwise byte-exact.
    assert_bytes(
        &read(dir.path(), "e.py"),
        b"def a():\n    pass\n\n\ndef c():\n    pass\n",
    );
}

// ---------------------------------------------------------------------------
// 5. Optional line numbers — guard-only addressing (§5)
// ---------------------------------------------------------------------------
//
// The line number on each coordinate is optional: a guard alone resolves to the
// unique line bearing its content, skipping the drift/range check but keeping the
// uniqueness guarantee. These tests exercise the new "no number" branch on all
// three coordinates and confirm it neither regresses the numbered path nor
// weakens loudness.

/// A `move` with NO line numbers — `from`/`to`/`dest` each addressed by guard
/// alone — resolves and relocates byte-exact. This is the headline use case: the
/// agent supplies only distinctive substrings, never a number.
#[test]
fn guard_only_move_all_three_coordinates() {
    let dir = fixture("g.txt", b"alpha\nbeta\ngamma\ndelta\n");
    let (code, _o, err) = run(
        dir.path(),
        &[
            "move",
            "g.txt",
            "--from-guard",
            "beta",
            "--to-guard",
            "beta",
            "--dest-guard",
            "delta",
            "--after",
        ],
    );
    assert_eq!(code, 0, "stderr: {err}");
    // beta (line 2) lifted and reinserted after delta — no numbers given.
    assert_bytes(&read(dir.path(), "g.txt"), b"alpha\ngamma\ndelta\nbeta\n");
}

/// The core motivation: two guard-only moves in sequence against the SAME file.
/// After the first move every line number below the seam has shifted, yet the
/// second command needs no fresh numbers — it re-addresses by guard alone. This
/// is the re-read the tool exists to avoid, made unnecessary.
#[test]
fn guard_only_sequential_batch_needs_no_renumber() {
    let dir = fixture("b.txt", b"one\ntwo\nthree\nfour\nfive\n");

    // Move 1: "two" → after "five".  ⇒ one,three,four,five,two
    let (code, _o, err) = run(
        dir.path(),
        &[
            "move",
            "b.txt",
            "--from-guard",
            "two",
            "--to-guard",
            "two",
            "--dest-guard",
            "five",
            "--after",
        ],
    );
    assert_eq!(code, 0, "first move stderr: {err}");

    // Move 2: "four" → after "two" (now the last line). The numbers from the
    // original read are all stale here; guard-only addressing is immune.
    let (code, _o, err) = run(
        dir.path(),
        &[
            "move",
            "b.txt",
            "--from-guard",
            "four",
            "--to-guard",
            "four",
            "--dest-guard",
            "two",
            "--after",
        ],
    );
    assert_eq!(code, 0, "second move stderr: {err}");
    assert_bytes(&read(dir.path(), "b.txt"), b"one\nthree\nfive\ntwo\nfour\n");
}

/// Coordinates are independent: a numbered `from` (with its drift cross-check)
/// composes with guard-only `to` and `dest` in the same call.
#[test]
fn mixed_numbered_and_guard_only_coordinates() {
    let dir = fixture("m.txt", b"one\ntwo\nthree\nfour\n");
    let (code, _o, err) = run(
        dir.path(),
        &[
            "move",
            "m.txt",
            "--from",
            "2",
            "--from-guard",
            "two",
            "--to-guard",
            "two",
            "--dest-guard",
            "four",
            "--after",
        ],
    );
    assert_eq!(code, 0, "stderr: {err}");
    assert_bytes(&read(dir.path(), "m.txt"), b"one\nthree\nfour\ntwo\n");
}

/// Guard-only mode keeps the uniqueness guarantee loud: a guard matching two
/// lines is `ambiguous-anchor` (exit 1), never a silent pick, and writes nothing.
#[test]
fn guard_only_ambiguous_fails_loud() {
    let dir = fixture("a.txt", b"x\ny\nx\n");
    let before = read(dir.path(), "a.txt");
    let (code, _o, err) = run(
        dir.path(),
        &[
            "move",
            "a.txt",
            "--from-guard",
            "x",
            "--to-guard",
            "y",
            "--dest-guard",
            "y",
            "--after",
        ],
    );
    assert_eq!(code, 1, "stderr: {err}");
    assert!(err.contains("ambiguous-anchor"), "stderr: {err}");
    assert_bytes(&read(dir.path(), "a.txt"), &before);
}

/// A guard-only coordinate whose guard matches nothing is `guard-not-found`
/// (exit 1), writing nothing — the zero-match arm is loud without a number too.
#[test]
fn guard_only_not_found_fails_loud() {
    let dir = fixture("n.txt", b"a\nb\nc\n");
    let before = read(dir.path(), "n.txt");
    let (code, _o, err) = run(
        dir.path(),
        &[
            "move",
            "n.txt",
            "--from-guard",
            "nope",
            "--to-guard",
            "nope",
            "--dest-guard",
            "c",
            "--after",
        ],
    );
    assert_eq!(code, 1, "stderr: {err}");
    assert!(err.contains("guard-not-found"), "stderr: {err}");
    assert_bytes(&read(dir.path(), "n.txt"), &before);
}

/// The guard is never optional: a coordinate addressed by neither a number nor a
/// guard fails loud as `empty-guard` (a number alone cannot address a line,
/// having no integrity check), writing nothing.
#[test]
fn coordinate_without_number_or_guard_is_empty_guard() {
    let dir = fixture("e.txt", b"a\nb\nc\n");
    let before = read(dir.path(), "e.txt");
    // No --from and no --from-guard at all.
    let (code, _o, err) = run(
        dir.path(),
        &[
            "move",
            "e.txt",
            "--to-guard",
            "a",
            "--dest-guard",
            "c",
            "--after",
        ],
    );
    assert_eq!(code, 1, "stderr: {err}");
    assert!(err.contains("empty-guard"), "stderr: {err}");
    assert_bytes(&read(dir.path(), "e.txt"), &before);
}

/// A relocation with no destination at all (no `--dest` number, no `--dest-guard`)
/// and no `--force` is a usage error (exit 2), NOT a silent delete — only the
/// explicit `--force` form removes a span.
#[test]
fn relocate_without_destination_or_force_is_usage_error() {
    let dir = fixture("r.txt", b"a\nb\nc\n");
    let before = read(dir.path(), "r.txt");
    let (code, _o, err) = run(
        dir.path(),
        &["move", "r.txt", "--from-guard", "a", "--to-guard", "a"],
    );
    assert_eq!(code, 2, "stderr: {err}");
    assert!(err.contains("requires a destination"), "stderr: {err}");
    assert_bytes(&read(dir.path(), "r.txt"), &before);
}
