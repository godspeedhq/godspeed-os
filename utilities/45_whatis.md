# Utility: `whatis`

**Utility:** `whatis` - what runs when a name is typed (kind + origin)
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`whatis <name>` answers **"what runs when I type this name at the prompt?"** It is the honest
replacement for POSIX `which`: there is no `$PATH` here and no executables on the filesystem,
so there is no path to return - a name's truth in GodspeedOS is its **kind and origin**. That
is also an authority answer (§26.9): a built-in runs in the shell's protection domain, a
library script is gsh run by the shell, a service runs in its own domain with its own
capability set.

The name is one glued word (`whatis`, like `selfcheck`) - command names are single words;
hyphens belong to argument keywords (`all-services`, `kill-storm`).

## 2. Invocation

| Command | Meaning |
|---|---|
| `whatis <name>` | Print the name's kind (one line) and return. Unknown names are a loud `Err`. |

## 3. Output

```
gsh> whatis ls
ls: shell built-in

gsh> whatis health
health: library script (gsh, baked into the image)

gsh> whatis fs
fs: standalone service (running - task 5, core 1)

gsh> whatis pong
pong: standalone service (not running)

gsh> whatis where
where: record-pipe stage (runs only inside a pipe)

gsh> whatis banana
banana: unknown
```

- **Running services show their live task + core** - identity is the name; the task/core is
  merely where it lives right now (Invariant 11), so the parenthetical is a snapshot, not part
  of the identity.
- **The pipe-only verbs get their own kind** - "why does typing `where` bare fail?" is a real
  confusion this line dissolves. The dual commands (`sort`, `match`, `count`, `first`, `last`)
  run bare on files too, so bare they are built-ins.
- **`unknown` returns `Err`** (`assert fails whatis banana` holds).

## 4. The lens (one deliberate ambiguity)

The answer is about **the name at the prompt**. `ping` is both a built-in command and a demo
service; typing `ping` runs the built-in, so `whatis ping` says built-in. Similarly
`whatis version` and `whatis help` cannot be asked - the universal `<util> version|help`
subcommand intercept answers about `whatis` itself first (consistency across every utility
beats the edge case).

## 5. Data source

- The shell's own command/library tables (built-ins, baked library scripts, pipe verbs).
- Live task lookup (`task_stat` via the INTROSPECT cap the shell already holds) for the
  running-service case; a static known-services list for the not-running case.

## 6. Non-goals

- **Not a description.** `whatis` says what a name *is*; `help` says what it *does*.
- **Not a path.** If a user-editable `/lib` ever exists, the origin distinction
  (`baked into the image` vs `/lib/x.gsh, on disk`) becomes this utility's most valuable
  answer - the "is this something I shipped or something dropped on the disk?" question.

## 7. Conformance

Conforms: `whatis help` / `whatis version`; listed by top-level `help` under **System**; in
`NO_PATH_CMDS` (its argument is a name, never a file path); its argument tab-completes from
the command-name set; a pipe producer (`whatis ls | write /k.txt`). Pinned by `selfcheck`
(built-in / service / pipe-stage lines, loud unknown, usage refusal).
