# Utility: `count` - how many lines, words, and bytes

**Status:** **Built + QEMU-verified** (`osdev test files`). A shell built-in FILTER, like
`match`: the direct form `count <path>` and the pipe form `<producer> | count`. Trails
`CLAUDE.md`; does not amend it.

---

## 1. What it is - and why not "wc"

`count` reports how many **lines**, **words**, and **bytes** its input has. It is the tool
every other system calls **`wc`** ("word count") - a cryptic abbreviation that doesn't even
name its most common use (counting *lines*). GodspeedOS names utilities for what they do
(`write` not `touch`, `match` not `grep`), so the counter is **`count`**: a plain verb a
layperson reads correctly. Its natural partner is a pipe - *"how many?"* after a filter:
`find *.txt | count`, `read /log | match error | count`.

## 2. Usage

```
count 0.4.0 - count lines, words, and bytes

usage:
  <producer> | count   count piped input
  count <path>         count a file
  count version        print the version
  count help           print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour

Output is one labelled line: `N lines, M words, K bytes` (singular when a count is 1, e.g.
`1 line, 1 word, 6 bytes`). Unlike `wc`'s three bare numbers, each is named - no guessing which
column is which.

- **lines** - newline count, plus one for a final unterminated line (so a file with no trailing
  newline still counts its last line).
- **words** - runs of non-whitespace bytes.
- **bytes** - the raw size.

**On a record stream it counts ROWS** and prints the bare number (`status | count` → `14`), since
lines, words and bytes mean nothing for a table.

`count` consumes input; it is never a pipe *producer*. In a pipe it is normally the last stage
(it collapses many lines into one summary), but being a filter it can also feed onward
(`find *.txt | count | write /n.txt` writes the summary to a file).

## 4. Implementation

A shell built-in FILTER (`run_filter_builtin`, alongside `match`): it runs **in-process**, so it
is **not** subject to the 4 KiB pipe service-boundary cap and can count a full 16 KiB stage
buffer. The pipe form consumes the previous stage's buffer (`write_count` in
`run_filter_builtin`); the direct form reads the file itself (`gs::fs::Fs::read_into`, streaming
`READ_AT`) - no new `fs` surface - into a fixed `FILTER_READ_MAX` (8192-byte) buffer, and refuses
a larger file loudly rather than counting part of it (pipe it instead: `read <path> | count`).
`count` with neither a path nor piped input prints `count: a path is required (or pipe input:
<producer> | count)`.

## 5. Later (separate so it can grow)

- `count lines` / `count words` / `count bytes` - emit just the one bare number, for when the
  value feeds something else (a future filter that takes a count).
- Counting across multiple files, once a multi-file producer exists.

## 6. Conformance

Conforms to `0_conventions.md`: its own `count help` (usage with a real example per row) and
`count version` (number + creator credit), via the shared `help_block` helper.

**Does NOT currently conform to rule 10** (`0_conventions.md` §1.10), found 2026-10-09. The `fs`
read of a file goes through a `gs::fs::Fs` handle that is lent no notice, so each request is bounded
(`gs::call::DEFAULT_SECS`, 5 s) but prints no `[q] quit` and cannot be ended with `q`; the
`Cancelled` branches in the handler are unreachable. This said the request was q-abortable via
`fs_request_q`, which no longer exists. `dir` is the one fs-backed command that still lends the
notice (`16_dir.md` §6); the same `.noticing(...)` here is the fix.
