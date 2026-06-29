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
count 0.1.0 - count lines, words, and bytes

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

`count` consumes input; it is never a pipe *producer*. In a pipe it is normally the last stage
(it collapses many lines into one summary), but being a filter it can also feed onward
(`find *.txt | count | write /n.txt` writes the summary to a file).

## 4. Implementation

A shell built-in FILTER (`is_filter_builtin`, alongside `match`): it runs **in-process**, so it
is **not** subject to the 4 KiB pipe service-boundary cap and can count a full 64 KiB stage
buffer. The pipe form consumes the previous stage's buffer (`write_count` in
`run_filter_builtin`); the direct form `read`s the file itself (`fs` `ReadFile`, op 11) - no new
`fs` surface.

## 5. Later (separate so it can grow)

- `count lines` / `count words` / `count bytes` - emit just the one bare number, for when the
  value feeds something else (a future filter that takes a count).
- Counting across multiple files, once a multi-file producer exists.

## 6. Conformance

Conforms to `0_conventions.md`: its own `count help` (usage with a real example per row) and
`count version` (number + creator credit), via the shared `help_block` helper.
