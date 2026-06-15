# Utilities: `first` / `last` — keep the first or last N lines

**Status:** **Built + QEMU-verified** (`osdev test files`). Two shell built-in FILTERS, like
`match`/`count`/`sort`: the direct form `first [N] <path>` / `last [N] <path>` and the pipe form
`<producer> | first [N]` / `| last [N]`. Trails `CLAUDE.md`; does not amend it.

---

## 1. What they are — and why not "head" / "tail"

`first N` keeps the first N lines of its input; `last N` keeps the last N. Other systems call
these **`head`** and **`tail`**. Those names are *not* ciphers like `grep`/`wc` — they are
guessable metaphors (a list has a head and a tail), so they are defensible. But `first`/`last`
clear the project's bar — *"would a layperson guess right with no prior knowledge?"* — with zero
metaphor to learn: **`read /log | last 20`** is unambiguous to someone who has never touched a
terminal. They pair as cleanly as head/tail, and read naturally with the count.

## 2. Usage

```
first 0.1.0 — keep the first N lines (default 10)
last  0.1.0 — keep the last N lines (default 10)

usage:
  <producer> | first [N]   first N lines of piped input
  <producer> | last [N]    last N lines of piped input
  first [N] <path>         first N lines of a file
  last  [N] <path>         last N lines of a file
  <first|last> version     print the version
  <first|last> help        print this message

N defaults to 10.   <path> = [index:]label/path | /abs | rel
```

## 3. Behaviour

`N` is optional and defaults to **10** (`read /log | last` is "the last few"). A numeric arg is
the count, a non-numeric arg is the path, in either order (`first 20 /log` = `first /log 20`).
Blank lines are skipped (consistent with the other line filters).

- **`first`** streams the first N lines in one pass — no buffer, any N.
- **`last`** keeps the most recent `TAKE_MAX` (1024) line spans in a ring buffer, so it is
  correct even for input far larger than the ring; an N beyond 1024 is capped with a loud note
  (§26.6). The classic "watch a growing log" follow mode (`tail -f`) belongs to true pipe
  streaming — a deliberate future, see `docs/pipes.md`.

Both consume input; neither is a pipe *producer*. Being filters they compose:
`find *.txt | sort | first 5`, `read /log | match error | last 20`.

## 4. Implementation

Shell built-in FILTERS (`is_filter_builtin`, with `match`/`count`/`sort`): they run
**in-process**, so they are **not** subject to the 4 KiB pipe service-boundary cap and can take
from a full 64 KiB stage buffer. `cmd_take(last: bool)` serves both verbs; the pipe path routes
through `run_filter_builtin`. The direct form `read`s the file itself (`fs` `ReadFile`, op 11) —
no new `fs` surface.

## 5. Later (separate so it can grow)

- A `follow` mode for `last` (stream new lines as a file grows), once true pipe streaming lands.
- Byte/character ranges, if a need appears.

## 6. Conformance

Both conform to `0_conventions.md`: their own `first`/`last` `help` (usage with a real example
per row) and `version` (number + creator credit), via the shared `help_block` helper.
