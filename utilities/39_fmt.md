# Utility: `fmt` - format a script to the GodspeedOS standard

**Status:** **Spec-first; building on `feat/gsh-fmt`.** The `.gsh` formatter - one canonical layout,
non-negotiable, applied in place. Written before the code (as `observe` was, per `0_conventions.md` §3).
Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`fmt <path>` rewrites a `.gsh` script **in place** to the single GodspeedOS canonical layout. You run
it knowing exactly what happens - there is nothing to decide. It is the `gofmt` / `terraform fmt` /
`rustfmt` idea, for gsh: the point is not prettiness, it is **uniformity**. A canonical format kills
bikeshedding and makes diffs mean something - you can see what *changed* instead of squinting past ten
differently-cramped lines.

```
# before  (real sysadmin energy)
let   mut n=0
if $n>5{echo big}else{echo small}
  for i in range 1 5{echo $i}

# after   fmt /script.gsh
let mut n = 0
if $n > 5 {
    echo big
} else {
    echo small
}
for i in range 1 5 {
    echo $i
}
```

### Boring on purpose - zero style options

There is exactly **one** canonical layout and **no** knobs to change it. The second `fmt` grows a
style flag, you have re-introduced the argument it existed to end. `fmt` has subcommands (below), but
none of them change *how* the code is formatted - only *what fmt does with the result*.

## 2. The canonical standard (what "formatted" means)

1. **Indent** 4 spaces per block depth. No tabs.
2. **Braces, K&R.** `header {` on one line, body indented, `}` at the block's own indent. `} else {`
   and `} else if <cond> {` stay on one line.
3. **One statement per line.** A `;` statement separator becomes a newline (the biggest de-jarring
   win). `cmd ; cmd ; cmd` becomes three lines.
4. **One space between tokens.** Runs of whitespace between tokens collapse to a single space;
   leading/trailing whitespace per line is stripped.
5. **`#` comments** keep one space after the `#`; a full-line comment sits at the current indent, a
   trailing comment is one space after the statement.
6. **Blank lines** collapse (2+ become 1); no leading or trailing blank lines; the file ends in
   exactly one newline.
7. **Strings are sacred.** Content inside `'…'` / `"…"` is never touched - not its spacing, not its
   case, nothing.
8. **Idempotent.** `fmt` of already-formatted text is a no-op. `fmt(fmt(x)) == fmt(x)` is an
   acceptance test, not an aspiration.

> **Scope note (honest).** `fmt` canonicalizes *layout* - indentation, statement-per-line, brace
> placement, inter-token whitespace, blank lines. It does **not** re-space the insides of a token
> (`blk-16`, `val-$x`), because gsh uses `-`/`+` inside words as well as as operators, and guessing
> which is which would risk changing meaning. Micro-spacing of arithmetic is a later slice, gated on
> the tokenizer's exact rules. Layout is the jarring part; that is what ships first.

## 3. Surface

Subcommands are **words** and come **before** the path (`fmt` is a subcommand tool like `drives`, not
a file-first verb like `write`). There is no `write` subcommand - the default *is* the write.

```
fmt <path>            format the script IN PLACE (the default; no subcommand)
    e.g. fmt /script.gsh
fmt check <path>      is it already canonical? Ok + silent if yes; Err + loud if no. Never writes.
    e.g. fmt check /script.gsh
fmt version           print the version
fmt help              print usage
```

- **`fmt <path>`** - the default. Format in place, on the fly. Safe to run blind: it only moves
  whitespace and newlines, so it can never change what the script *does* (§2.7-2.8).
- **`fmt check <path>`** - the discipline / CI gate. Returns `Err` (via the `Result` model,
  `32_result.md`) and names the file if it is not canonical, so a script can gate on it
  (`fmt check /s.gsh ; if result == Err { fail "run fmt" }`). Changes nothing on disk.
- Bare **`fmt`** prints usage (a path is the actionable arg; without one there is nothing to do).

## 4. Guardrails & safety (loud, never silent - §26.7)

Because the default writes in place with no preview, `fmt` is **all-or-nothing** and never leaves a
damaged file:

- **Won't parse -> refuse, file untouched.** A syntactically broken script (unbalanced braces, a bad
  block header) gets a loud error and **no** write. `fmt` never lays a half-formatted result over a
  broken one; fix the syntax, then format.
- **Too big to format in bounds -> refuse, file untouched.** Unlike `run` (which may truncate for
  *execution* - harmless), writing a truncated format back would *delete* script content. So an
  over-large script gets a loud refusal, never a silent chop.

Neither guardrail is an option - they are the safety that lets the zero-preview default be trusted.

## 5. Later (separate so it can grow)

- **`fmt diff <path>`** - print the changes `fmt` would make without writing (needs a bounded diff).
- **`fmt <dir>`** - format every `.gsh` under a directory in one call.
- **Host `osdev fmt` + VS Code format-on-save.** The single definition of "GodspeedOS standard" is
  factored into a `no_std` core crate so the shell built-in and a host `osdev fmt` share it - one
  canonical format, never two that drift. This is where the gofmt/terraform muscle-memory lands.

## 6. Implementation shape & conformance

A **shell built-in** (`0_conventions.md` §2): it reads the file via `fs` (like `read`/`run`), formats
the bytes, and writes them back via `fs` - using caps the shell already holds, no widened authority.
The formatter is a bounded, streaming **token-level re-emitter**: it reuses `read_statement` (the
quote-aware statement boundary), `find_open_brace`/`find_matching_brace` (quote-aware brace matching),
and `raw_token`, and tracks an explicit indent depth on `{`/`}` (no native recursion, like the
executor's `frames` stack) - emitting into a bounded buffer, loud on overflow (§26.6). It never
evaluates the script; it only re-lays-out its tokens, which is why it is provably semantics-preserving.

Conforms to `0_conventions.md`: its own `fmt help` / `fmt version` via the shared `help_block`,
`fmt check help` for the subcommand, listed under **Console** in the top-level `help`.
