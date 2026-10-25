# Utility: `echo`

**Utility:** `echo` - print text
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`echo` prints the rest of its line back to the console, verbatim. The simplest
utility - useful as a console-output sanity check and a building block in
scripts and pipes (`echo hello | write /h.txt`).

## 2. Invocation

| Command | Meaning |
|---|---|
| `echo <text>` | Print `<text>` (the remainder of the line) and a newline. |

The argument is the whole remainder of the line after `echo `, trimmed - not split
into words - so spacing inside the text is preserved as typed. One surrounding pair
of quotes is stripped (`echo "hi"` prints `hi`).

## 3. Output

```
gsh> echo hello from the shell
hello from the shell
```

## 4. Capabilities

- **Console output** only. `echo` reads no kernel state and holds no authority
  beyond printing.

## 5. Non-goals

- **No escape sequences / flags** (no `-n`, no `-e`). It prints the text as given.
  `$name` expansion is done by the shell before `echo` runs, not by `echo` itself;
  the shell refuses to echo a variable holding a `secret` value
  (`gsh: refusing to echo a secret value - it stays off the console`).

## 6. Conformance

Conforms: own `echo help` / `echo version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **Console** as `echo <text>`. See `0_conventions.md` §3.
