# Utility: `match` - keep the lines that match (the grep-equivalent)

**Status:** **Built + QEMU-verified** (`osdev test files`). A shell built-in FILTER: the direct
form `match <pattern> <path>`, the pipe form `<producer> | match <pattern>` (slots into the
multi-stage middle), the `except` inverse, `*`/`?` glob (shared with `find`), and minimal
quoting. Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is - and why not "grep"

`match` reads lines of text and keeps the ones that match a pattern; it drops the rest. It is
the line filter every other system calls **`grep`** - a name that is the fossilised keystrokes
`g/re/p` from the 1970s `ed` editor (**g**lobal **r**egular **e**xpression **p**rint). The name
describes *how it was once implemented*, not *what it does*, and it is opaque to anyone who
doesn't know the history (the same family as `awk` = its authors' initials and `sed` = stream
editor). GodspeedOS names utilities for what they do (`write` not `touch`, `delete` not `rm`),
so the line filter is **`match`**: a real English verb, and - as the user noted - the same
sense as Rust's `match` (test a value against a pattern; the truthiness decides the outcome).
If a line matches, it is kept; if it doesn't, it is dropped. One honest verb covers it.

## 2. Usage

```
match 0.1.0 - keep the lines that match a pattern

usage:
  <producer> | match <pattern>   keep piped lines that match <pattern>
  match <pattern> [path]         keep lines of <path> (or the cwd-relative file) that match
  match except <pattern> [path]  keep the lines that do NOT match (the inverse)
  match version                  print the version
  match help                     print this message

<pattern> = substring (default) | *? glob (like find)
<path>    = [index:]label/path | /abs | rel
```

## 3. Agreed behaviour

- **Two ways in, like `find`.** A **pipe sink** (`read /log | match error`) and a **direct**
  form (`match error /log`). Both reach the same place; a person uses whichever reads better.
  This mirrors `find <name> [path]`, so it is consistent, not bloat.
- **Matching reuses `find`'s model.** A plain word is a **substring** match (`match error`); a
  pattern with `*`/`?` is an anchored **glob** (`match *.txt`). One mental model across `find`
  and `match`, no new rules. (Shares the `contains` / `glob_match` helpers.)
- **The inverse is `except`, a leading keyword.** `match except error` keeps every line that
  does *not* match - no cryptic `grep -v`. `except` *leads* the pattern because trailing would
  read wrong ("match error except…?"); same shape as `write append`, and it is the keyword
  only when a pattern follows (so matching the literal word "except" stays possible). It is the
  same matcher with the boolean flipped - cheap, so it ships with the first cut.
- **Whole lines.** Like `grep`, `match` keeps the whole line in which the pattern appears, not
  just the matched token.

## 4. Quoting (folded in with `match`)

`match` is the command that forces the quoting question: every argument is one
whitespace-delimited token today, so `match hello world` is broken (pattern `hello`, stray
`world`). The honest fix - *not* a POSIX reflex - is grouping a multi-word argument:

> A token wrapped in a matching pair of `'…'` or `"…"` is one argument; the surrounding pair
> is stripped. **No escapes, no nested quotes, no variable expansion**; single and double
> behave identically.

That gives `match "hello world"` (the real need) and `echo "I am text"` → `I am text` (the
visual bounding the user likes, quotes vanishing as expected), while deliberately stopping
before bash's footguns (`'can'\''t'`, `$VAR`, `[[ ]]`). The one honest cost: with no escape
character, emitting a lone literal quote is fiddly - an acceptable trade for a system that
isn't trying to be bash.

## 5. Implementation

A shell built-in **filter**: input bytes → matching lines out (`match_lines`, sharing `find`'s
`contains` + `glob_match`). In a pipe it consumes the previous stage's buffer (`is_filter_builtin`
→ `run_filter_builtin` in `stage_filter`/`stage_sink`); as a built-in it runs **in-process**, so
it is **not** subject to the 4 KiB service-boundary cap and can filter a full 64 KiB stage
buffer. The direct form `read`s the file itself (`fs` `ReadFile`, op 11) - no new `fs` surface.
`match` is a FILTER, never a pipe producer: `match … /file | …` is refused (use `read /file |
match …`). Minimal quoting lives in the shared `tokenize`/`strip_quotes` helpers, so it benefits
every command, not just `match`.

## 6. Later (separate so it can grow)

- **`match regex <expr> [path]`** - full regular expressions, **gated behind the explicit
  `regex` word** so a layperson never trips over `^$.*`; only someone who asks for regex gets
  it. Default stays friendly (substring + glob). Needs a `no_std` regex engine, so it is a
  clear future opt-in, not the first cut.
- Case-insensitive matching (an `ignore-case` keyword), if wanted.
- `count` (how many matched), once a counting filter exists.

## 7. Conformance

Conforms to `0_conventions.md`: its own `match help` (usage with a real example per row), the
`match except help` subcommand help, and `match version` (number + creator credit), via the
shared `help_block` helper.
