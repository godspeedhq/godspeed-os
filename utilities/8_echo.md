# Utility: `echo`

**Utility:** `echo` — print text
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`echo` prints the rest of its line back to the console, verbatim. The simplest
utility — useful as a console-output sanity check and a building block once
scripting and pipes exist (Appendix D).

## 2. Invocation

| Command | Meaning |
|---|---|
| `echo <text>` | Print `<text>` (the remainder of the line) and a newline. |

The argument is the whole remainder of the line after `echo `, trimmed — not split
into words — so spacing inside the text is preserved as typed.

## 3. Output

```
gs> echo hello from the shell
hello from the shell
```

## 4. Capabilities

- **Console output** only. `echo` reads no kernel state and holds no authority
  beyond printing.

## 5. Non-goals

- **No escape sequences / interpolation / flags** (no `-n`, no `-e`). It prints the
  text as given. Variable interpolation and redirection are a future
  shell-scripting concern (Appendix D), not part of `echo` itself.

## 6. Conformance

Built-in: no `echo help` / `echo version` yet; listed by the shell's top-level
`help` under **Console** as `echo <text>`. See `0_conventions.md` §3.
