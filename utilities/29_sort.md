# Utility: `sort` — order the lines

**Status:** **Built + QEMU-verified** (`osdev test files`). A shell built-in FILTER, like
`match`/`count`: the direct form `sort <path>` and the pipe form `<producer> | sort`. Trails
`CLAUDE.md`; does not amend it.

---

## 1. What it is

`sort` orders its input lines and prints them. Unlike `grep`/`wc`, the POSIX name `sort` is
already plain English, so it keeps it — naming for what it does is the rule, not renaming for
its own sake. Its natural use is a pipe (`find *.txt | sort`, `read /names | sort reverse`).

## 2. Usage

```
sort 0.1.0 — order the lines (ascending, or reverse)

usage:
  <producer> | sort         sort piped input
  sort <path>               sort a file's lines
  sort reverse [path]       sort descending
  sort version              print the version
  sort help                 print this message

<path> = [index:]label/path | /abs | rel   (see docs/drives.md §4.1)
```

## 3. Behaviour

Lines are ordered **lexicographically by bytes** (ascending), or descending with `reverse`.
`reverse` is a keyword wherever it appears after the verb (`sort reverse /f` and `sort /f
reverse` both work), so it reads the same piped or direct. Blank lines are dropped. Equal lines
keep no defined relative order (the sort is unstable — there is no heap for a stable merge), but
since they are equal that is invisible.

Bounded (§26.6): `sort` orders the first `SORT_MAX_LINES` (1024) lines and says so if there are
more — it never silently drops the rest.

`sort` consumes input; it is never a pipe *producer*. Being a filter it can sit mid-pipe
(`find *.txt | sort | count`) or end one.

### On records — sort by a column
`sort` spans **both** pipe worlds (`docs/records.md`): on a text stream it line-sorts as above; on
a **record** stream (e.g. after `status`) it orders rows **by a column** — `sort mem`, `sort
reverse name`. The column name follows the verb, with `reverse` leading so the column lands at the
end and nothing dangles (`sort reverse mem`, same shape as `match except`). The comparison is
numeric when the column's values are numeric, else textual. The record verbs proper
(`where`/`select`/`to`/`from`) live in `utilities/31_records.md`; `sort` is documented here because
its text form predates them.

## 4. Implementation

A shell built-in FILTER (`is_filter_builtin`, with `match`/`count`): it runs **in-process**, so
it is **not** subject to the 4 KiB pipe service-boundary cap and can sort a full 64 KiB stage
buffer. It records each line as a `(start, end)` pair into a fixed `SORT_MAX_LINES` array on the
stack and `sort_unstable_by`s the index array (no heap — `sort_unstable` is in-place), then emits
the lines in order. The direct form `read`s the file itself (`fs` `ReadFile`, op 11) — no new
`fs` surface.

## 5. Later (separate so it can grow)

- `sort unique` — drop duplicate lines (needs a stable or post-pass dedupe).
- `sort number` — numeric order rather than lexicographic.
- A larger / streaming sort once true pipe streaming lands (the 1024-line bound is the
  store-and-forward limit, not a fundamental one).

## 6. Conformance

Conforms to `0_conventions.md`: its own `sort help` (usage with a real example per row), the
`sort reverse help` subcommand help, and `sort version` (number + creator credit), via the
shared `help_block` helper.
