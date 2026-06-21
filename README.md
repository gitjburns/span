# span

`span` is a small, stateless CLI tool for AI agents. It contains exactly two
verbs — `span move` and `span copy` that relocate or duplicate a block of existing
lines, intra-file or across files, addressed by line number. It does this without
the text being read and written by the non-deterministic LLM, saving your context
and ensuring a faithful byte-level copy of large blocks of text within a file or
between multiple files.

## The problem it solves

When an AI coding agent moves or duplicates a block of code with a string-replace
editor, it has to **re-emit every line verbatim**. That fails two ways at once:

- **It consumes context window.** In a long session the re-emitted bytes are
  generated as output and then carried back as *input* on every later turn. This
  needlessly consumes your context window.

- **It risks silent corruption.** An agent reproducing hundreds of lines by hand
  is likely to eventually drop, reorder, or alter the text is copies. A plausible
  looking result may even survives verification step (which further consumes
  context). This is a fidelity problem in *generation* that no amount of context
  budget can fix.

`span` removes the bytes from the agent's hands entirely. Relocation of the text
 is done by an external tool that carries **coordinates instead of content**: the
agent passes line numbers and a short anchor substring, and `span` transfers the
bytes itself, byte-exact, regardless of the size of the block.

The purpose of this tool is not to replace the a general-purpose edit tools. It
is meant for specific circumstances where the general-purpose edit tools are the
wrong tool for the job.

---

## How it works

- **Address by reference.** A coordinate is a **guard** — a literal substring the
  agent expects on the line — plus an **optional** 1-based **line number** (the
  same numbers a file read already prints). The guard does the addressing; a
  supplied number is a positional cross-check. Endpoints `from`/`to` and the
  `dest` are each guarded, and each number is independently optional — so a batch
  of edits can be sequenced without re-reading to recover line numbers that
  shifted under an earlier edit.

- **Disk is canonical.** Every call re-reads the file(s) from disk, validates,
  applies atomically, and prints a fresh numbered view. There is no state to open
  or hold.

- **Drift is loud; the number is optional.** Guards resolve to **exactly one**
  line. An ambiguous or missing anchor fails loudly and writes nothing, so a
  coincidental same-content line is never edited silently. When you also pass a
  line number, a unique match at a *different* line is loud `drift`; omit the
  number and the guard simply resolves wherever its content uniquely lives — there
  is no number to drift from. Either way the wrong line is never edited: dropping
  the number drops only the positional cross-check, never the uniqueness guarantee.

- **Bytes move untouched.** The moved block's own bytes (including each line's
  `\r\n`/`\n` and the file's trailing-newline state) are preserved exactly.

- **Seam tidiness.** If the source block was blank-line-delimited, `span`
  reproduces the source's **blank-run width** at the destination seam — a
  two-blank-delimited block (a PEP 8 top-level `def`) lands with two blanks, not
  one. It is self-gating: it never fabricates whitespace the source lacked, and it
  tops the seam up to the source's count without ever exceeding it (an existing
  blank is never doubled). Moving a line out of tight code synthesizes nothing.
  A `move` also tidies the seam it *leaves behind*, collapsing the blank doubling
  its own removal created so it never restyles the lines it left (see SPEC §4).

- **Atomic writes.** Each file is written via temp file → `fsync` → `rename` →
  directory `fsync`. A call fully applies or leaves the file untouched. A
  cross-file `move` writes the destination durably *first*, then removes the
  source, so any failure leaves the source intact.

- **Delete by reference (forced).** A `move` with **`--force`** removes the span
  and reinserts it nowhere — deleting a large block by coordinate instead of
  re-emitting it into the editor's `old_string`. `--force` is the sole trigger and
  an explicit acknowledgment; it rejects any destination supplied alongside it and
  never weakens the guard checks. `copy` has no such form. This is the one deletion `span` does, and it is on-thesis: it carries
  coordinates, not content.

See `SPEC.md` for the complete specification.

---

## Installation

`span` and `span-guard` compile to ordinary CLI binaries. They must be in the
`PATH` that your agent's shell sees: `span` so the agent can run it, and
`span-guard` because the hook (below) invokes it by bare name. Where you put
 them is your choice.

Build the source (requires a Rust toolchain, edition 2024):

```sh
cargo build --release
# → target/release/span         (the tool)
# → target/release/span-guard   (the optional Claude Code hook, see below)
```

Then copy both into a directory in your `PATH`:

```sh
install -m 755 target/release/span target/release/span-guard /usr/local/bin/
# …or ~/.local/bin, ~/bin — wherever you keep CLI tools
```

## Usage

`span` is designed to be run by your AI agent (e.g., Claude Code). However,
it can also be run manually if needed.

```
span move <file> [--from <n>] --from-guard <s> [--to <n>] --to-guard <s> \
                 [--dest <n>] --dest-guard <s> (--before | --after) \
                 [--dest-file <path>] \
                 [--from-context <off:s> …] [--to-context <off:s> …] \
                 [--dest-context <off:s> …]

span copy <file>  …same flags…

span move <file> [--from <n>] --from-guard <s> [--to <n>] --to-guard <s> --force \
                 [--from-context <off:s> …] [--to-context <off:s> …]
                 # forced delete-by-reference: --force, no destination
```

- `<file>` — the **source** file (positional). The destination defaults to it.
- `--from/--to/--dest <n>` — **optional** line numbers. When present each is a
  positional cross-check against its guard (mismatch → loud `drift`); when omitted
  the guard alone addresses the line, so a batch can be sequenced without
  re-reading shifted numbers.
- `--from-guard/--to-guard/--dest-guard <s>` — the per-coordinate guards, each a
  literal substring expected on the line. The guard is **required** (the number is
  not); `--dest 0`, top of file, is the lone exception and takes no guard.
- `--before | --after` — **required** on a relocation; where the block lands
  relative to `dest`.
- `--dest-file <path>` — destination file for a **cross-file** relocation; omit
  for intra-file. A `--dest-file` that resolves to the source (paths are compared
  by canonicalization) folds into the intra-file case, so the `dest-in-span`
  overlap guard still applies.
- `--force` — the sole trigger for the delete-by-reference form of `move`;
  rejected whenever any destination addressing is present, and it never weakens
  the `from`/`to` guard checks.
- `--<role>-context <off:s>` — repeatable neighbor anchor: a signed offset and a
  substring expected on the line at *that coordinate's own resolved line + off*
  (so `--from-context` is relative to `from`, `--to-context` to `to`,
  `--dest-context` to `dest`), split on the **first** colon, so the substring may
  contain colons. The offset must be **non-zero** (offset `0` would re-address the
  line the guard already covers). Use it to disambiguate a low-entropy line.
- **Top of file:** `--dest 0 --after` (no `--dest-guard`) inserts at the very
  start — and is the only way to target an empty destination file. `--dest 0`
  requires `--after`; since line `0` means "before the first real line,"
  `--dest 0 --before` has no meaning and is a usage error. (Note: it is `--force`
  that deletes the span; `--dest 0`, like any destination, relocates.)

**Delete** a block by reference (source is removed, nothing reinserted):

```sh
span move src.rs \
  --from 40 --from-guard 'fn dead_code' \
  --to   71 --to-guard   '}' --to-context '-1:return acc' \
  --force
```

The forced-delete misuses are usage errors (exit 2): a relocation with no
destination (no `--dest` number and no `--dest-guard`) and no `--force`, `--force`
alongside any destination addressing, a destination-less `copy`, and
`--before`/`--after` with no destination.

**Move** a block (source is removed), disambiguating a bare `}` with context:

```sh
span move src.rs \
  --from 10 --from-guard 'fn parse' \
  --to   24 --to-guard   '}' --to-context '-1:return result' \
  --dest 80 --dest-guard 'mod tests' --after
```

**Copy** a block into another file (source is kept):

```sh
span copy lib.rs \
  --from 3 --from-guard 'fn parse_ok' \
  --to   8 --to-guard   '}' \
  --dest 0 --after \
  --dest-file tests.rs
```

## Batching span ops — address by guard, skip the renumber

`span` transfers bytes by reference to save context; re-reading a file after each
move just to recover shifted line numbers throws that savings away. The simplest
way to avoid the re-read is to **omit the line numbers** and address each endpoint
by a distinctive `--*-guard` substring alone. A guard-only coordinate resolves
against the file as it is at the moment of the call, so a move that shifts lines
never invalidates the *next* command — there are no numbers to go stale, and
nothing to re-read between steps.

- Give each endpoint a **distinctive** substring; add `--*-context` if a line is
  not unique on its own. That is the whole instruction — no number bookkeeping,
  and no bottom-up ordering needed to keep a line-number frame valid.
- Each `span` call still echoes the affected region with fresh numbers, so you can
  confirm the result without a new read.
- If you *do* pass numbers — straight from a fresh read, or from the
  editor-intercept hook — the drift check stays on, so a stale number fails loud
  rather than corrupting. Numbers remain a safe, optional cross-check.

On success, `span` prints a one-line summary and a refreshed numbered view of the
affected region (post-edit line numbers, affected lines marked `>`), so a
follow-up edit in the same area needs no re-read.

### Exit codes

| code | meaning |
|------|---------|
| `0`  | success — the only case in which anything is written |
| `1`  | a loud coordinate/guard failure (drift, ambiguous, not-found, out-of-range, dest-in-span, from-after-to, …) |
| `2`  | a usage error (bad or missing flags) |
| `3`  | an I/O error — unreadable/unwritable file, non-UTF-8 input, or a path that **does not exist** (never created) |

### Failure output

A single drifted or missing coordinate prints a standalone, actionable line:

```
error: drift: --from anchor "fn parse" resolved at line 27, not 24; retry with --from 27
error: io: destination file "out.md" does not exist; span relocates into existing files only
```

An ambiguous anchor lists every candidate so you can pick a `--*-context` neighbor
without re-reading the file:

```
error: ambiguous-anchor: --to-guard "}" matches 3 lines in src.rs:
   2	}
   4	}
   6	}
  hint: add --to-context (e.g. a distinctive neighbor line) to make it unique
```

When several coordinates fail at once (e.g. a uniform line shift drifted all
three), one consolidated report lists every role — including the ones that
resolved — so the whole command can be rebuilt in a single edit:

```
error: 3 coordinates failed:
  --from	drift: anchor "fn parse" resolved at line 3, not 1 (retry with --from 3)
  --to	drift: anchor "}" resolved at line 5, not 3 (retry with --to 5)
  --dest	drift: anchor "fn other" resolved at line 7, not 5 (retry with --dest 7)
```

---

## Claude Code integration

`span` is driven entirely by an agent through its shell, so two pieces make
adoption reliable: a **hook** (deterministic backstop) and an **instructions
block** (so the agent reaches for `span` proactively). Use either or both
(both is recommended).

### 1. The `span-guard` hook

`span-guard` is a `PreToolUse` hook that watches the editing tools and detects the
move/duplicate tell. It has exactly **two outcomes** — **deny** or **silent
allow** — keyed on block size and direction:

- a **large inserted** block (≥ 5 lines) with a verbatim twin → **deny**, handing
  back a pre-assembled `span move` command with the discovered source coordinates
  (plus an auto `--*-context` neighbor when an endpoint guard would be ambiguous,
  so the model's first run resolves cleanly);
- a **large deleted** block (≥ 10 lines) → **deny** with a *fork* — `span move …
  --dest …` to relocate, or `span move … --force` to delete by reference — with
  **no twin required** (delete-first order hides the twin). The deletion bar is
  higher than the insertion bar because plain deletions are common, and a false
  deny costs only one needless interruption with a documented escape (`--force`);
- anything else → **silent allow**.

There is **no soft "reminder" outcome.** A per-edit nudge would spend the very
context `span` exists to conserve; the proactive "prefer `span`" habit lives in
`CLAUDE.md` (below), and the hook stays a pure hard backstop.

Twin matching is **byte-exact** — a near-miss is not a relocation tell — which is
what lets the insertion deny be a hard gate without blocking legitimate rewrites.

It **fails open**: any malformed input or error exits `0` with no output, so
editing is never broken.

Add the hook to your `settings.json` and provide permission to run it:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Edit|Write|MultiEdit",
        "hooks": [
          {
            "type": "command",
            "command": "span-guard"
          }
        ]
      }
    ]
  },
  "permissions": {
    "allow": ["Bash(span:*)"]
  }
}
```

The `span-guard` command must be on the agent's `PATH`. The `Bash(span:*)` permission
entry removes the per-call approval prompt, so the redirect to `span` never stalls.

### 2. Suggested `AGENTS.md` (or `CLAUDE.md`) instructions

Paste the block below into your project's `AGENTS.md` / `CLAUDE.md`. It stands on
its own and works **whether or not** the hook is installed — the hook just
backstops it:

````markdown
## Relocating or duplicating existing lines → use `span`, don't re-emit

When you are **moving or duplicating a block of existing lines** — especially a
large block, or one crossing files — do **not** re-emit the lines through the
string-replace editor (`Edit`/`Write`/`MultiEdit`); use `span` instead.

Address **each** endpoint — `from`, `to`, and the destination — by a
**distinctive `--*-guard` substring** of its line (a long, unique substring, not a
short fragment); you don't supply line numbers. A guard must match exactly one line
or `span` fails loud, so for a **low-entropy line** (a bare `}`, a repeated flag)
add a `--*-context <offset:substr>` neighbor (split on the first `:`; `offset` is
signed, negative = a line above). The moved span `[from, to]` is **inclusive**.
Land it `--before` or `--after` the destination line — to land at the **end** of a
file, guard its last line and use `--after`; for the **top** of a file (or an
empty file, which has no line to guard) use `--dest 0 --after`.

**Move** existing lines to a new location (source is removed):

```sh
span move src.rs \
  --from-guard 'fn parse' \
  --to-guard   '}' --to-context '-1:return result' \
  --dest-guard 'mod tests' --after
```

**Copy** existing lines to a new location (source is kept); cross-file shown
here with `--dest-file`:

```sh
span copy lib.rs \
  --from-guard 'fn parse_ok' \
  --to-guard   '}' --to-context '-1:assert!(run' \
  --dest-guard 'mod tests' --after \
  --dest-file tests.rs
```

For authoring new text or replacing content, keep using the normal string editor.
````

> **Deliberate omission.** The forced delete-by-reference form (`span move …
> --force`, see SPEC §6) is intentionally **left out** of the block above.
> Advertising it on every turn would reintroduce the standing per-turn context
> cost `span` exists to avoid; the `span-guard` hook surfaces it exactly when it is
> needed — on a large editor deletion — and nowhere else.

---

## Non-goals

`span` deliberately does not author new text or replace content, rename,
search-and-replace, batch multiple ops, address sub-line/columns, or expose any
JSON interface. (Deleting a span **by reference** — `span move … --force` with no
destination — *is* supported, since it carries coordinates not content; authoring
and replacement remain out of scope.) Its purpose is to relocate, duplicate, and
delete existing lines faithfully with minimal impact on session context.
