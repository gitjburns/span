# span — Specification

> **Provisional name.** `span` is chosen because every verb addresses a line
> span `[from, to]` against a guarded point; rename freely.
> **Status:** Draft v3.1 — relocation + forced delete-by-reference, **plain-CLI
> interface (no JSON)**.
> **Language:** Rust (single binary).

## 1. Motivation

String-replace editing forces a model to re-emit text verbatim to move or
duplicate it — costing context, and risking byte-level drift on content the
model did not author. In a session that never compacts, every re-emitted line is
doubly costly: paid once to generate, then carried as input on every later turn.

`span` relocates and duplicates text **by reference** — by the line numbers the
read tool already prints — so the moved bytes never re-enter the model's
context. Relocation carries coordinates, not content. The tool is **stateless**:
each call reads the file(s) as they are on disk, validates guards, applies
atomically, and prints a fresh numbered view of what it changed.

This document specifies a single capability: **`move` and `copy`, intra-file and
cross-file**, exposed as a **plain command-line tool** — flags in, numbered text
out, conventional exit codes. Authoring new text, deletion, replacement, rename,
batching, and any JSON interface are explicit non-goals (§11).

**Purpose, precisely.** `span` is a precision instrument for one high-severity
operation: relocating or duplicating a **large block of existing lines** —
especially across files — without routing the bytes through the model. The
string-replace editor fails this case two ways at once. It **blows out the context
window**: the re-emitted bytes are generated as output, then carried as input on
every later turn of a non-compacting session. And it **silently corrupts**: a
model reproducing hundreds of lines verbatim will eventually drop, reorder, or
alter some, yielding a plausible-looking but wrong result that survives a skim.
The second failure is a **fidelity guarantee the model cannot give itself at any
context budget**, because the fault is in generation, not memory — `span` removes
the bytes from the model's hands entirely. Judge the tool by the **severity** of
the operation it guards, not the **frequency** with which it fires. It is **not** a
general-purpose editor and **not** a replacement for string-replace editing, which
remains correct for authored changes.

**Settled architecture — do not relitigate.** The transport is a stateless plain
CLI driven through the agent's existing shell tool. This is deliberate, not an
oversight awaiting an "upgrade" to a more integrated surface:

- **Not MCP.** An MCP server injects its tool schema into context on **every
  turn** — a permanent standing cost — which is self-defeating for a tool whose
  entire purpose is to conserve context. MCP also addresses a different problem
  (exposing external or remote capabilities across a standardized boundary) and is
  the wrong protocol for invoking a local file edit. The CLI rides on the
  already-present shell tool, costs **nothing** until called, and then carries only
  coordinates.
- **Not the Agent SDK.** A custom-harness SDK tool would integrate natively but is
  out of scope by constraint, and shares MCP's standing-schema cost. The spec
  assumes a **closed, subscription harness** (e.g. Claude Code) where the binary is
  driven through the shell and adoption is enforced by a `PreToolUse` hook (§10) —
  not by registering a first-class tool, which the harness does not permit anyway.
- **No JSON (§11).** Flags in, numbered text out. Structured-payload interfaces
  tempt re-emission of content; `span` carries coordinates only.

This document assumes a reader fluent in the distinctions among a CLI, an MCP
server, and the Agent SDK, and in the context-cost tradeoffs among them. The
choices above are recorded to mark them **made**, not to teach the distinctions.

## 2. Design Principles

- **Address by reference.** A coordinate is a bounded **integrity guard** plus an
  **optional** line number (§5). `move` and `copy` carry only coordinates — never
  content.
- **Bytes move untouched.** Relocated text is transferred by the tool,
  byte-exact, and never enters the agent's context regardless of size.
- **Address by guard; the number is an optional cross-check.** A guard anchor is
  the primary address: it resolves to the unique line bearing that content. A line
  number is **optional** — when supplied it must agree with where the guard
  resolves (disagreement is loud `drift`); when omitted, the guard alone
  addresses the line. Splitting addressing from integrity still holds: the guard
  carries the integrity check in both modes; the number only *adds* a positional
  cross-check when present.
- **Make ambiguity and drift loud.** Every coordinate must resolve to a
  **unique** anchor (§5); zero matches (`guard-not-found`) or multiple
  (`ambiguous-anchor`) always fail **loud**, in both addressing modes. When a
  line number is supplied, a unique match at a *different* line is loud `drift`;
  when the number is omitted there is no addressed line to drift from, but the
  uniqueness requirement is unchanged — a coincidental same-content line is never
  edited silently.
- **Boring verbs, intuitive flags.** `span move` / `span copy` with named flags
  a model can assemble correctly on the first try; output is the same numbered
  format a file read already prints.

## 3. Architecture

- **Disk is canonical.** Each call reads the current content of the file(s) it
  touches, validates guards, applies the op, and writes the result. No state
  outlives a call; there is no session to open, hold, or release.
- **Reference frame = the read tool's numbering.** The model addresses lines by
  the 1-based absolute numbers the harness already prints on read; `span`
  interprets the same numbers. The frame is free — it is already on screen.
- **Atomic write (per file).** Write to a temp file in the destination
  directory, `fsync` the temp file, `rename` over the target, then `fsync` the
  directory. A single-file call fully applies or leaves the file untouched.
- **Renumber and echo.** A relocation renumbers the lines below each affected
  point monotonically. Each successful call prints the new numbered view of the
  affected region(s) (§8), so the model never guesses the post-edit frame.

There is no time-based staleness model: the guard is the sole freshness check.
If a file changed under a coordinate such that its guard no longer matches, the
op fails loud and the model re-reads.

## 4. Text Model

- Files are treated as UTF-8 text, split into lines on `\n`. Numbering is 1-based.
- Untouched lines keep their exact bytes, including a `\r` before `\n` (CRLF
  files are preserved).
- A "line" for guarding and addressing is the content between separators,
  excluding the separator itself. For guard matching, a single trailing `\r`
  (CRLF files) is **not** part of the line content; guards are written against
  the logical line text and never need to include `\r`.
- **Relocated bytes are transferred byte-exact**, including each moved line's own
  separator as it appeared in the source.
- **Never join lines.** The tool always guarantees a separator between the
  relocated block and whatever now follows it at the destination, synthesizing a
  `\n` if one would otherwise be missing.
- **Each file's trailing-newline state is preserved.** Removing the source block
  does not change whether the source ends with a terminating `\n`; appending a
  block at the end of the destination adopts the destination's existing
  trailing-newline convention. The block never silently gains or loses a trailing
  newline relative to the file it lands in.
- **Boundary-blank mirroring (seam separators).** When the source block was
  **blank-delimited** — one or more blank lines immediately above and/or below it
  in the source — `span` reproduces that **blank-run width** at the matching
  destination seam, so a relocated section lands as well-separated as it left (a
  PEP 8 top-level `def` keeps its two blank lines, not one). This is a sibling of
  the "never join lines" guarantee above: **"bytes move untouched" still holds for
  the moved block's own bytes** — a synthesized blank is a *separator at the seam*,
  the same category as the synthesized `\n`, never an edit to the relocated
  content. The rule is deliberately narrow:
  - **Self-gating.** Blanks are mirrored on a side **only to the extent the source
    block was blank-delimited on that side.** Moving a line out of tight,
    blank-free code (count `0`) synthesizes nothing — mirroring can never fabricate
    whitespace the source lacked, so it cannot corrupt dense code.
  - **Tops up, never beyond.** Let `s`/`t` be the source's leading/trailing
    blank-run widths. At the destination seam `span` synthesizes `max(0, s - e)`
    leading blanks, where `e` is the blank run already present there (and
    symmetrically for `t`): it tops the seam up to the source's count and never
    exceeds it, so an existing blank is never doubled.
  - **Suppressed at the file edges.** No leading blank is added at the very top of
    a file (line `0` / no preceding line) and no trailing blank at end-of-file (no
    following line) — there is no neighbor to separate from.
  - **Separator convention.** A synthesized blank uses the destination file's own
    newline bytes (`\r\n` if the file is CRLF, else `\n`, §4).
  - **Move-only source-gap collapse.** Removing a block that was blank on **both**
    sides leaves its `s` leading and `t` trailing blanks adjacent at the source
    seam — a merged run of `s + t` where the bystander neighbors had been separated
    only by the block. `move` collapses that **removal-created** run to
    `max(s, t)` (dropping `min(s, t)` blanks), so a relocation never restyles the
    lines it leaves behind: the symmetric PEP 8 `2/2` case stays `2`, the legacy
    `1/1` case stays `1`. It only de-doubles — when either side had no run
    (`min == 0`) nothing is removed, so a single blank left dangling at the new
    end-of-file (e.g. after moving a file's last blank-delimited section) is not
    removed.
  - **Default, no flag.** Mirroring is always on and has no `--reflow` /
    `--no-reflow` toggle. The tool is agent-driven; an opt-in flag would be a
    standing instruction carried on every call — the exact per-turn context cost
    `span` exists to eliminate. The rule is self-gating **and** count-preserving,
    so it needs no escape hatch even for blank-significant code like PEP 8: dense
    code gets zero synthesized blanks, and blank-delimited code gets back exactly
    the run width it had.

## 5. Coordinate Guard

A coordinate is a **guard**, an **optional line**, and optional **context**:

- *guard* — a substring the agent expects on the addressed line. This is the
  primary address: it resolves to the line bearing that content.
- *line* — an **optional** 1-based line number. When present it is a positional
  cross-check (the guard must resolve to exactly this line, or the op fails loud
  `drift`); when omitted, the guard alone addresses the line. The number is
  optional precisely so an agent can sequence a batch of edits without re-reading
  the file to recover line numbers that shifted under an earlier edit — the read
  the tool exists to avoid (§ batching, README).
- *context* — optional ordered list of `offset:guard` neighbor anchors, each
  expected on the line at *resolved line + offset* (`offset` non-zero; negative =
  above, positive = below). It lets a low-entropy line borrow entropy from
  neighbors and is independent of whether a *line* number was supplied.

**Matching.** A line *L* *satisfies* the anchor when *guard* is a substring of
content(*L*) and, for every context entry, its guard is a substring of
content(*L + offset*). Matching is a case-sensitive literal substring test
against logical line content (trailing `\r` excluded, §4).

**Anchor uniqueness — the integrity guarantee.** Before applying, `span` scans
the relevant file for lines satisfying the anchor. The coordinate is valid iff
**exactly one** line satisfies it — and, *when a line number was supplied*, that
the single match is the addressed line. Otherwise the op fails **loud**:

- **zero matches** → `guard-not-found`;
- **two or more matches** → `ambiguous-anchor`. The error **lists every candidate**
  as `<line-number><TAB><content>` plus a hint to add `--<role>-context`, so the
  agent picks a disambiguating neighbor from the listing **without re-reading** the
  file (§8).
- **one match at *L*, a line number was supplied, and *L* ≠ that number** →
  `drift`. The error **names the drifted anchor text** and reports *L* (`anchor
  "…" resolved at line L, not …`); the op is **not** applied — the agent retries
  against the corrected line, or simply drops the number and re-addresses by guard
  alone.
- **one match, no line number supplied** → the match is used. There is no
  addressed line to drift from; the uniqueness scan above is the entire integrity
  check, and it is unchanged.

Uniqueness is what makes a wrong-line edit impossible *everywhere*, including
low-entropy lines and in both addressing modes: a bare `}` guarded by `}` alone is
rejected as `ambiguous-anchor` — with or without a number — until the agent adds a
context entry (e.g. the distinctive line above) that makes the combined anchor
unique. A coincidental same-content line can therefore never be addressed
silently: it is either ambiguous (loud), or, when a number is present, resolves
elsewhere (loud). Omitting the number drops only the positional cross-check, never
the uniqueness guarantee.

**The guard is mandatory; the number is not.** Every guard (and every context
guard) must be non-empty; the sole exception is the top-of-file anchor (line `0`,
§6), which takes no guard. Because the guard *is* the address, a coordinate with
no guard fails loud as `empty-guard` whether or not a number was given — a number
alone cannot address a line, having no integrity check.

**Coverage.** `move` and `copy` guard **both endpoints** (`from`, `to`) **and
`dest`**. Each guarded coordinate is independently subject to the uniqueness rule
— `from`/`to` against the source file, `dest` against the destination file.

## 6. Operations

Addressing is by guard, optionally cross-checked by a line number (§5). Spans are
inclusive `[from, to]`. Position is `before` or `after`, placing the relocated
content relative to `dest`.

- **`move`** — removes lines `[from, to]` from the source file and reinserts them
  before/after `dest`. `from`/`to` guard against the source file (the call's
  target file); `dest` guards against the destination file.
- **`copy`** — inserts a duplicate of `[from, to]` before/after `dest`; the
  source is unchanged. Same guarding as `move`.
- **`move` with `--force` (forced delete-by-reference).** A `move` called with
  **`--force`** removes `[from, to]` and reinserts it nowhere — a delete addressed
  by coordinate, so the removed bytes never pass through the model. Deleting a
  large block through the string editor would force the model to reproduce every
  line verbatim in the editor's `old_string` (the exact re-emit cost and
  corruption risk `span` exists to remove), so a coordinate-addressed delete
  belongs. **`--force` is the sole trigger** for this form and is the explicit
  acknowledgment that content is being removed: any destination addressing
  supplied alongside it (`--dest`, `--dest-guard`, `--dest-context`,
  `--dest-file`, `--before`/`--after`) is a usage error (§7). The trigger is
  `--force` rather than an absent `--dest` because the destination *number* is now
  optional (§5) — a guard-only destination is also "no number" yet is a
  relocation, so absence can no longer distinguish the two. `--force` **never
  weakens** the guard checks: `from`/`to` are resolved with the full §5 integrity
  guarantee, so a forced delete can no more hit the wrong line than a relocation
  can. `copy` has **no** such form — duplicating to nowhere is meaningless.
- **Top-of-file destination.** Addressing `dest` as line `0` with position
  `after` denotes the start of the destination file, takes no guard, and is the
  entry point for relocating to the top of a file (and the only way to target an
  empty destination file). Line `0` is a sentinel for "before the first real
  line," so `--before` has no meaning against it: **`--dest 0` requires `--after`**,
  and `--dest 0 --before` is a usage error (§7). Note the distinction from the delete form above: it is
  **`--force`** that removes the span; `--dest 0` (and any other destination) is a
  relocation. A relocation with no destination at all — no `--dest` number *and*
  no `--dest-guard`, without `--force` — is a usage error (§7), never a silent
  delete.
- **Source vs. destination file.** The destination defaults to the source file
  (the common, intra-file case) and is named explicitly only for cross-file
  relocation. The span content is transferred **by the tool** — it never enters
  the agent's context and is copied byte-exact, regardless of size.
  Intra- vs. cross-file is decided by **canonicalizing** both paths, so a
  `--dest-file` that resolves to the source — via a relative spelling, `./`, or a
  symlink — folds into the intra-file case (and keeps its overlap guard) rather
  than being mistaken for a two-file relocation. A path that cannot be
  canonicalized is treated as distinct (a non-existent `--dest-file` fails the
  existence check regardless, §9).
  - *Intra-file* (destination canonicalizes to the source): `dest` must lie
    **outside** `[from, to]`, otherwise loud rejection (`dest-in-span`). The op is
    a single-file atomic write.
  - *Cross-file* (destination is a different file): no overlap constraint applies
    (`dest` addresses a different file). This is the relocation primitive for
    moving a large block between files without ingesting it — see §9.

### Failure conditions (all loud)

- Anchor not satisfied or ambiguous (§5); or, when a line number is supplied,
  drifted (§5).
- Any **supplied** line number out of range `[1, line_count]` (except the
  top-of-file anchor line `0`). A guard-only coordinate has no number to range-check.
- Intra-file `dest` inside `[from, to]` (`dest-in-span`).
- `from > to` (`from-after-to`).
- Empty or missing guard (`empty-guard`), except the line-`0` anchor.
- A source or `--dest-file` path that **does not exist** (exit 3, §7). Authoring
  files is a non-goal (§11), so a missing destination is a loud I/O failure, never
  an implicit create.

**All coordinates are resolved up front, and reported together.** `span` resolves
`from`, `to`, **and** `dest` before reacting to any single failure, so a uniform
line shift — which drifts all three coordinates at once — is surfaced in **one**
response rather than one serial fix-rerun round-trip per coordinate:

- **exactly one** coordinate failed → that coordinate's **standalone** rich message
  (e.g. the full `ambiguous-anchor` candidate listing, §8);
- **two or more** failed → a **consolidated** report (`N coordinates failed:`) that
  lists **every** role, including the ones that passed (`--dest  ok: line N`), so
  the agent can rebuild the whole command in a single edit.

On any failure the file(s) are left untouched.

## 7. Command-Line Interface

```
span move <file> [--from <n>] --from-guard <s> [--to <n>] --to-guard <s> \
                 [--dest <n>] --dest-guard <s> (--before | --after) \
                 [--dest-file <path>] \
                 [--from-context <off:s> …] [--to-context <off:s> …] \
                 [--dest-context <off:s> …]

span copy <file> …same flags…

span move <file> [--from <n>] --from-guard <s> [--to <n>] --to-guard <s> --force \
                 [--from-context <off:s> …] [--to-context <off:s> …]
                 # forced delete-by-reference: --force, no destination (§6)
```

- `<file>` — the source file, positional. The destination defaults to it.
- `--from/--to/--dest <n>` — **optional** line numbers (§5). When present, each is
  a positional cross-check against its guard (mismatch → loud `drift`); when
  omitted, the guard alone addresses the line. Omitting them lets a batch of edits
  be sequenced without re-reading shifted line numbers.
- `--from-guard/--to-guard/--dest-guard <s>` — the per-coordinate guards, each a
  literal substring (§5). The guard is **required** (the number is not): a number
  alone cannot address a line. (`--dest 0`, top of file, is the lone exception and
  takes no guard.)
- `--before | --after` — required on a relocation; position relative to `dest`.
- `--dest-file <path>` — destination file for a cross-file relocation; omit for
  intra-file.
- `--force` — the sole trigger for the delete-by-reference form of `move` (§6),
  and the explicit acknowledgment that content is removed. It is rejected whenever
  **any** destination addressing is present, and it never weakens the `from`/`to`
  guard checks.
- `--<role>-context <off:s>` — repeatable; one neighbor anchor per occurrence,
  `off` a signed integer offset and `s` the substring (split on the **first**
  colon, so `s` may itself contain colons). E.g. `--to-context -3:"fn parse"`.
- **Top-of-file destination:** `--dest 0 --after` with no `--dest-guard`. `--dest 0`
  requires `--after`; `--dest 0 --before` is a usage error.

**Exit codes.** `0` success; `1` a loud coordinate/guard failure (§6) — the
specific reason word is on stderr; `2` a usage error (bad/missing flags); `3` an
I/O error (unreadable/unwritable file, non-UTF-8 input, or a source/`--dest-file`
path that **does not exist** — never created, §6/§11). Nothing is written unless
the exit is `0`. Usage errors (exit 2) include the forced-delete misuses: a relocation with **no
destination** (no `--dest` number and no `--dest-guard`) and **no** `--force`,
`--force` **with** any destination addressing, a destination-less `copy`, and
`--before`/`--after` with no destination.

Conventions: standard `--help`/`-h` and `--version`; flags may appear in any
order; no hidden state, no config file, no environment dependence.

## 8. Output

**No JSON.** Success prints to **stdout** a one-line summary followed by the
refreshed numbered view; failure prints a single actionable line to **stderr**.

**Numbered view.** The view renders the affected range plus **3 lines of context**
above and below (clamped to file bounds), each with its absolute **post-edit**
line number, in the same `<number><TAB><text>` form a file read prints. Affected
lines are marked with a leading `>` in the gutter; context lines with a space:

```
moved 3 lines → src.rs:80-82
     77    let cfg = load();
     78
     79  mod tests {
  >  80      fn parse_ok() {
  >  81          assert!(run("x"));
  >  82      }
     83  }
     84
     85  // eof
```

This is the refreshed frame; a follow-up edit in the same area needs no re-read.

**Cross-file** relocation touches two files, so it prints **two** sections — the
destination and the source — each with its own `path:` header and numbered view,
separated by a blank line. The source section shows the seam where the block was
removed (its surrounding lines, renumbered); with no surviving affected lines it
carries no `>` markers and its header notes the removal:

```
moved 15 lines → tests.rs:80-94 (from lib.rs:10-24)

tests.rs:
  >  80  fn parse_ok() {
     …
  >  94  }

lib.rs:  (15 lines removed at 10)
      7  }
      8
      9  fn next() {
```

**Forced delete** (§6) prints a `deleted` summary and the source seam, the same
numbered window a cross-file removal shows (no `>` markers — nothing survives at
the destination because there is none):

```
deleted 12 lines from mod.rs:10-21
      7  fn prev() {
      8  }
      9
     10  fn next() {
```

**Failure.** A single actionable block on stderr naming the reason word (§6), the
offending coordinate, and a hint; the file(s) are unchanged. Most failures are one
line; `ambiguous-anchor` and the consolidated report span several. The reason word
is always first.

A single drifted or absent coordinate (note `drift` names the anchor text):

```
error: drift: --from anchor "fn parse" resolved at line 27, not 24; retry with --from 27
error: guard-not-found: --dest-guard "mod tests" not found in tests.rs
error: io: destination file "out.md" does not exist; span relocates into existing files only
```

An ambiguous anchor lists every candidate as `<number><TAB><content>` so the agent
can pick a `--<role>-context` neighbor without re-reading:

```
error: ambiguous-anchor: --to-guard "}" matches 3 lines in src.rs:
   2	}
   4	}
   6	}
  hint: add --to-context (e.g. a distinctive neighbor line) to make it unique
```

When **two or more** coordinates fail at once (the uniform-shift case), one
consolidated report lists every role — including the ones that resolved — so the
whole command can be rebuilt in a single edit:

```
error: 3 coordinates failed:
  --from	drift: anchor "fn parse" resolved at line 3, not 1 (retry with --from 3)
  --to	drift: anchor "}" resolved at line 5, not 3 (retry with --to 5)
  --dest	drift: anchor "fn other" resolved at line 7, not 5 (retry with --dest 7)
```

## 9. Atomicity & Crash Safety

- **Per-call, per-file atomicity.** Temp file → `fsync` → `rename` → directory
  `fsync`. A single-file call is durable once it exits `0`, or leaves the file
  untouched.
- **No unflushed state.** Statelessness means there is nothing to lose: every
  successful call is already on disk.

**Cross-file `move` — rollback by ordering.** A cross-file relocation writes two
files. There is no joint-atomic primitive across two files without persistent
state, which this tool does not have. Instead the order makes the **error path** a
clean rollback and the **crash path** non-destructive:

1. Validate **everything** before touching anything: confirm the source and (for
   cross-file) the `--dest-file` **exist** (a missing file fails here, exit 3, and
   is never created, §6), then resolve **all** guards (`from`/`to` in the source,
   `dest` in the destination — all three up front, §6).
2. Write the **destination first** (temp → `fsync` → `rename` → dir `fsync`).
3. **Only after the destination is durable**, remove `[from, to]` from the source
   (its own atomic temp → `rename`).

Because the irreversible source removal is the *last* step, any failure before it
— guard mismatch, permissions, full disk, bad path — leaves the source
**untouched**. There is nothing to roll back: the removal never began. This is
exactly the guarantee that "if the block cannot be written to the destination,
the source is not modified."

The only window not covered by ordering is a hardware crash *between* the
destination commit and the source removal, which leaves the block in **both**
files — a recoverable duplicate, never data loss. `copy` removes nothing and so
carries no hazard at all; a crash simply leaves it not-yet-applied.

## 10. Adoption — editor hook

`span` is invoked through the agent's shell, so nothing in the binary forces its
use over the built-in string-replace editor. Adoption is therefore a **harness
configuration** problem, and only one available lever is **deterministic**: a
`PreToolUse` hook on the editing tools (`Edit`/`Write`/`MultiEdit`). Everything
else — `CLAUDE.md` guidance — is **probabilistic**: it depends on the model
attending to it, and so degrades exactly when context is full, which is when a
large relocation matters most. The hook is code; it fires identically on every
call regardless of model state. Note the hook's deterministic lever is a **hard
deny only** — it never injects a soft per-edit reminder, which would itself be a
probabilistic nudge bought at a standing context cost.

**Two outcomes, keyed on block size and tell.** The hook has exactly two
responses — **deny** or **silent allow** — and scales the deny to the severity of
getting it wrong. There is **no soft "reminder" outcome**: a per-edit nudge spends
the very context `span` exists to conserve, and the companion `CLAUDE.md` guidance
(below) already carries the proactive "prefer `span`" habit, so the hook is the
hard backstop and nothing else. The two halves of a move present as two independent
tool calls (see below) with different precision economics, so the **deny has two
separate thresholds**:

- **Inserted block (the destination half).** Below `DENY_MIN_LINES` = 5 → **silent
  allow**: at this size re-emission is cheap and a hard block is not worth the
  false-positive risk. At or above 5 **with a verbatim twin** → **deny.** This is
  the context-blowout-and-silent-corruption zone. The deny is a hard gate: the call
  does not execute and the hook's reason is fed back to the model, which re-issues
  as `span`. The **verbatim-twin requirement is the precision key** — a genuinely
  new large block has no twin and passes untouched; only a byte-exact copy of
  existing content (the relocation tell) is stopped, so the gate can be hard
  without blocking legitimate large rewrites.
- **Deleted block (the source-removal half).** Below `DELETE_DENY_MIN_LINES` = 10 →
  **silent allow**. At or above 10 → **deny** with a two-way fork (below) — and,
  crucially, **no twin is required.** In delete-first order the destination half is
  not yet on disk, so a twin filter would never fire; the deny keys on block size
  alone. The deletion bar is a **separate, higher** constant than the insertion bar
  (never reuse 5) because deletion is structurally noisier: `Edit`→`""` is the
  *standard* way to delete a block (high base rate of genuine deletes), it lacks the
  twin precision filter, and a false negative merely falls back to the editor status
  quo (≈ free) while a false positive interrupts a legitimate delete — so the bar is
  biased toward false negatives. A twin in another file is not required, but when
  present it **strengthens** the message ("also appears at X — likely a move").

**Why a hard deny on deletions is now acceptable.** A hard block on a deletion once
risked trapping the model with no clean exit. The forced delete-by-reference form
(`span move … --force`, §6) **is** that clean exit, so the trap rationale is
resolved. The deny therefore hands back a **fork**: relocating? → `span move …
--dest <LINE> … --after` (does *both* halves by reference, so the expensive
destination re-emission never happens by hand); genuinely deleting? → `span move …
--force` (`--force` = delete by reference, no destination). Once the model deletes via `span …
--force` (a Bash call), the hook — which matches only the editor tools — never
fires on it again: noise control is stateless and free, the `rm -i`/`-f` precedent,
firing at most once per session per pattern.

**Actionable deny.** Because the hook had to locate the twin in order to fire, it
already knows the source file and the `[from, to]` range; the deny message carries
those coordinates — ideally the fully assembled `span move`/`span copy` command —
so the model copies a known-good invocation rather than reconstructing one. The
more the hook pre-computes, the higher the rate of a correct switch.

**Detection is per-call, one half at a time.** A move presents to the hook as two
separate tool calls; either is independently sufficient to fire:

- an `Edit`/`MultiEdit` that deletes a large block (large `old_string` → empty
  `new_string`) — the **source-removal** tell;
- a `Write`/`Edit` that inserts a large block found verbatim elsewhere in the
  workspace — the **destination** tell.

Twin matching is **byte-exact** by contract: every line of the candidate block
must equal the corresponding file line in full. This is the false-positive filter
that lets the insertion deny be a hard gate — a near-miss is not a relocation tell.
A "first and last line plus length" heuristic is **not** sufficient and must not be
substituted; it would resurrect false denies on coincidentally-bracketed blocks.

**Reinforcement (probabilistic, but cheap).** A companion `CLAUDE.md` entry
documents the trigger rule ("relocating or duplicating existing lines → use
`span`, don't re-emit") with one worked example of each verb, so the model reaches
for `span` **proactively** and avoids the wasted round-trip of a blocked edit — the
hook is the backstop, `CLAUDE.md` keeps the model off it. A permission allowlist
entry (`Bash(span:*)`) removes per-call approval friction, so the redirect never
stalls at the moment of compliance.

**Scope.** The gate catches only what the detector sees: a verbatim relocation. An
edit that is not a byte-exact move does not fire — which is correct, because then
it is not `span`'s job. The deny blocks the wrong path; the actionable coordinates
are what drive correct `span` usage from there.

## 11. Non-Goals

- **Any JSON interface.** Input is flags; output is plain numbered text; errors
  are plain stderr lines. No JSON in, no JSON out, no `--json` mode.
- **Authoring and replacement.** `span` does not insert new text or replace
  content. Deletion is **no longer** a flat non-goal: a `move` with `--force`
  removes a span **by reference** (§6), carrying coordinates rather than content,
  so it is on-thesis and supported. Authoring new text and replacing existing
  content remain out of scope — `span` still never originates bytes.
- **Rename / search-and-replace.** No find, no replace-all, no regex, no token
  substitution.
- **Batching.** Each `move`/`copy` is a standalone call. There is no multi-op
  coordinate frame; sequence relocations with separate calls.
- **Sub-line / column addressing** — line is the atomic unit.
- **Language / AST-aware structural editing.**
- **Multi-writer concurrency or OS locking.**
- **Cross-file atomicity** beyond the rollback-by-ordering guarantee of §9.
- **A persistent session, or a verify step in the hot path.**

## 12. Transport

`span` is a plain CLI binary, invoked statelessly through the shell, one
relocation per call: flags in, numbered text out, conventional exit code. No
persistent process, no session handle, no IPC, no structured-data envelope. An
intra-file `move`/`copy` names one file; a cross-file `move`/`copy` names two
(the source — positional — and `--dest-file`), and the tool reads both,
guard-checks each, and transfers the bytes itself, all within the one
self-contained call.
