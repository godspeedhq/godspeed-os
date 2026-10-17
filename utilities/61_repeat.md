# Utility: `repeat`

**Utility:** `repeat` - run a command again and again, `q` stops it
**Status:** Built 2026-10-12. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`repeat` runs a command, then runs it again: a set number of times, or until you press `q`, with an
optional pause between runs. It is for doing the same thing more than once - `kill xhci` four times
twenty seconds apart on a hardware card, `status` every two seconds while something settles - so the
loop is typed as one line rather than run by hand.

**The name is what it does.** It replaced the library script `watch` (2026-10-12), which did most of this
under a name for one reason to use it: watching something change. Run `kill xhci` through it and nothing
is being watched; an action is being repeated. `watch` is also the POSIX word for the tool, which is the
reflex this vocabulary does not carry (rule 8). Typing `watch` answers `try repeat` (`FOREIGN_HINTS`) and
runs nothing.

## 2. Invocation

| Command | Meaning |
|---|---|
| `repeat <command ...>` | Run it, then again, until `q`. |
| `repeat wait <seconds> <command ...>` | The same, with that pause between runs (1..3600, the `wait` utility's bounds). |
| `repeat count <n> <command ...>` | Run it `n` times (1..10000), then stop. |
| `repeat count <n> wait <seconds> <command ...>` | `n` runs with the pause between them. `count` and `wait` may come in either order. |
| `repeat help` / `repeat version` | As every utility (rules 1-6). |

`count` and `wait` are `repeat`'s own words, read only before the command: so the `count` and `wait`
commands themselves cannot be repeated, which nobody needs. **A repeat of a repeat is refused** - the whole
count goes in one: `repeat count 4 <command>`.

## 3. Behaviour

- **Each run is announced**: `repeat 2 of 4: kill xhci`, or `repeat 2: status` with no count. The
  command's own output follows it, as typed at the prompt. `[q] quit` is the shell's, once, 2 s in, not
  repeated on every run's line.
- **`q` stops it between runs**: during the pause, or - with no pause - when it looks at the keys after a
  run. A command that waits has its own `q` (rule 10).
- **The end is a verdict**: `repeat: 4 run(s), 1 failed`, with `- stopped by q` when it was. It is `Err`
  when any run failed, so a script can branch on it, and a failure in the middle of a long run is a count
  rather than a line that scrolled away.
- **Runs inside a script** as well as at the prompt. It is a built-in for that reason: a library script
  cannot be run from another script, and gsh cannot tell a number from a word, so `watch` could neither
  serve a script nor read a count.
- **Each run is a script line.** The command runs one level down (`execute` at depth + 1), so `run`,
  `selfcheck` and the library scripts are refused inside it, and the refusal names `repeat` (it named only
  scripts until `backlog/84` S1). Full-screen views (`help`, `docs`) print instead of opening, because a
  view waiting for a key would stop the runs.
- **Its output is the caller's.** The announcements, each run's output and the verdict go where the
  command's output was sent: the screen at the prompt, the variable when it runs in a function whose
  output is captured, `$(f)` (`backlog/84` S2). A refusal of its own options still goes to the screen.
  `$(repeat ...)` itself is refused, since a capture takes a producer, and `run ... save <path>` saves
  only the run's report, for `repeat` as for every command.
- **Keys typed while it runs are kept.** Between runs, and during its pause, a key that is not `q` is kept
  for the prompt rather than dropped, so a command typed ahead is still there when it ends (`backlog/84`
  S3).
- **No screen clearing.** Clearing was there to make `watch` a live view; repeating is not that. For a live
  view of the system, `observe`.

## 4. Pipes (rule 12)

`repeat` does not pipe: it runs commands, and each prints what it prints. `repeat ... | ...` is refused
(`'repeat' cannot start a pipe`), so a pipeline cannot be repeated either. Recorded under rule 12 as an
action.

## 5. Capabilities

None of its own. It runs commands with the shell's authority, as typing them would; the clock and the
console, through the same pause `wait` makes.

## 6. Conformance

`repeat help` / `repeat version` (rules 1-6); listed by the shell's top-level `help`; in `NO_PATH_CMDS`, and
its first word completes from the commands and its two option words (rule 9). Pinned by `selfcheck`
(`scripts/selfcheck/10-meta.gsh`): a counted run that holds, one that fails, and the refusals.
