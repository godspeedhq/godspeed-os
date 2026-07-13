# Utility Conventions (shared)

**Status:** Canonical. These rules define how *every* GodspeedOS utility behaves.
They were first written inside `1_observe.md` §3 and hoisted here once a second
utility existed (per the note in that section - pull the abstraction into
existence, do not build a framework speculatively, §26.2).

Each utility has its own numbered doc in this folder (`1_observe.md`,
`2_date.md`, …). This file holds only what is common to all of them.

---

## 1. The rules

1. **Every utility has `help`.** A bare utility name with no actionable args, or
   an explicit `<util> help`, prints usage. With a fresh (non-POSIX) vocabulary,
   the system must teach its own verbs at the point of use.
2. **Every subcommand has `help`.** `<util> <subcommand> help` describes that
   subcommand specifically (e.g. `observe now help`).
3. **`help` is the word - the only form. No flags, no synonyms.** There is exactly
   one way to ask for help: the word `help`. No `-h`, no `--help`, no hidden
   aliases. A tolerated-but-undocumented synonym would itself be a hidden, unsaid
   rule - the silent behaviour the system forbids (§26.4, §26.5). `-h` is simply
   `unknown:`, and that response *teaches* the real word.
4. **Subcommands are words, never single-letter flags.** `observe now`, not
   `observe -n`. A word means the same thing across every utility; flag letters
   collide and drift (`-n` = "now" here, "number" there). This is the `ls -Sslah`
   wall-of-letters problem GodspeedOS rejects. Typing economy is a shell-ergonomics
   concern (completion, history - future), not a reason to abbreviate vocabulary.
5. **Every utility has a version, reported by `<util> version`.** Per utility, not
   per subcommand - subcommands evolve with their parent and inherit its version.
6. **`help` output carries the version header.** Line 1 of any help/usage output is
   `<util> <version>`, so the version is always one keystroke away.
7. **Utilities report raw facts; they do not editorialize.** Health verdicts,
   policy, and recommendations belong to purpose-built utilities (e.g. a future
   `status` health view), not bolted onto a metrics or info command.
8. **Vocabulary is not POSIX.** GodspeedOS has no fork/exec/errno/signals, so the
   user-facing words do not borrow POSIX names that would imply that heritage.
   Concretely: the clock's epoch subcommand is `date epoch` ("seconds since 1970"),
   not `date unix`; there is no `time` clock (in Unix `time` measures command
   duration). Pick words that say what the thing *is* in this system.
9. **Every utility and subcommand tab-completes.** The name completes from the command
   list; each subcommand keyword completes at its position (`net a` -> `net arp`, `net ver`
   -> `net version`, `ping c` -> `count`). Rule 4 makes words the vocabulary; completion is
   what makes typing words as cheap as flags. A subcommand that does not complete is one the
   user cannot discover without reading the source. Wire it in the shell's completion tables
   (`SUBCMD_FIRST` / the per-command case) the same commit that adds the subcommand.

   **Declare whether your arguments are file paths (same commit).** When a token is *not* a
   recognized keyword, Tab falls through to **file-path** completion - it lists the current
   directory (`ls /x<tab>`, `read /doc<tab>`). That is correct only for utilities whose
   arguments *are* paths. A utility whose arguments are **service names, numbers, or fixed
   keywords - never paths** (`chaos`, `kill`, `spawn`, `restart`, `ping`, `net`, `drives`,
   `observe`, `date`, `uptime`, ...) must be added to **`NO_PATH_CMDS`** in the shell
   (`services/shell/src/main.rs`, beside `complete_tab`), so Tab at one of its argument
   positions does *nothing* instead of offering unrelated files. (The bug this prevents:
   `chaos max-carnage all-services <tab>`, landing on the rounds argument, listed the root
   directory and offered `/.gsh_history`.) The default is path completion; **opting out is
   explicit and per-command** - a path-taking utility (`ls`, `read`, `write`, `mkdir`, `find`,
   `tree`, `copy`, ...) is simply absent from `NO_PATH_CMDS` and keeps its file completion. So
   when you add a utility: if its args are paths, do nothing; if they are not, add it to
   `NO_PATH_CMDS`.

   **`version` and `help` complete for free - do not list them.** Every utility answers
   `<util> version` and `<util> help` (rules 5-6), so `complete_keyword` appends them to every
   command's first-argument completion automatically. Do **not** add `version`/`help` to a
   `SUBCMD_FIRST` entry (they are deduped in). A utility that takes **no positional first
   argument** (an info command: `about`, `version`, `mem`, `cores`, `uptime`, `status`, ...) has
   the universal `version`/`help` as its *only* first-arg subcommands - add it to **`INFO_CMDS`**
   (beside `NO_PATH_CMDS`) so those two complete and Tab never falls through to a filesystem
   listing. So a new utility is exactly one of: a **path** command (in neither list), a **keyword**
   command with specific subcommands (`SUBCMD_FIRST` + `NO_PATH_CMDS`), or an **info** command
   (`INFO_CMDS`). Pick one; `version`/`help` come along in every case.
10. **Anything that blocks or waits is escapable with `q`.** If a utility can sit waiting - on
    a peer service, on the network, on a long sweep - then `q`/`Q`/ESC MUST abort it and return
    to the prompt, and a wait of more than a moment advertises `(press q to abort)`. A command
    that can wedge the shell with no way out is forbidden (§26.7: loud + escapable over silent +
    stuck). The primitive is `ServiceContext::request_with_reply_abortable` (send once, poll `q`
    while waiting); never block an interactive command on a bare `request_with_reply` to a peer
    that can be slow. The **fs-backed** interactive commands (`ls`/`cd`/`read`/`find`/`tree` and the
    file-reading filters `match`/`count`/`sort`) use the delayed-hint variant `fs_request_q` (SDK
    `request_with_reply_qhint`): silent on a fast reply, it advertises `(q to quit)` only once the wait
    lingers past ~2s - the just-in-time form of the advertisement above, so a snappy op stays quiet.
11. **Quitting stops the TASK, not just the shell.** When a utility is escaped (rule 10), the
    escape must abort the actual WORK the utility set in motion - not merely stop the shell from
    *waiting* on it. If the utility handed a long job to a peer service and the escape only stops
    the shell listening, the peer keeps grinding and the very *next* command blocks behind it: the
    shell looks free but is not. So a command that drives a multi-step peer operation (a sweep, a
    batch) drives it **step-by-step from the shell**, so `q` ends the loop and the peer is only
    ever mid-ONE step. Worked example: `net scan` began as one batch `net-stack` op (sweep all 254
    hosts); pressing `q` unblocked the shell, but net-stack kept sweeping, so a *second* `net scan`
    hung waiting for it. The fix made the shell drive the sweep one host at a time (op 7 -> per-host
    op 6). Escaping the wait is not enough; escape the work.
12. **Output is a pipeable structure.** A utility's result is data, not decoration: a producer
    emits either a typed record `Table` (`docs/records.md`, so `| where` / `| select` /
    `| to json` compose) or plain labelled lines (so `| match` / `| count` compose). Piping is
    the composition model; output that cannot flow onward is a dead end.
13. **If it does not fit the common pipes, `write` still captures it.** Any producer's output
    snapshots to a file with `| write <path>` (redirection is `| write`; there is no `>`, see
    `19_write.md`). So even a utility that is not a record source is never trapped on screen -
    its bytes always have somewhere to go.
14. **Multiple same-type targets are a COMMA-separated list, never spaced.** `kill ehci,xhci,fs`,
    `spawn ping,pong`, `restart fs,logger`, `delete /a,/b`, `mkdir docs,tmp`, `fmt a.gsh,b.gsh`,
    `chaos max-carnage nic-driver,net-stack` - one argument, comma-delimited. NOT spaced
    (`kill ehci xhci fs`): the shell tokenizes a line to a small fixed arg count (`MAX_ARGS = 4`), so a
    spaced list silently caps at ~3 targets, while a comma-list is a SINGLE token and is therefore
    unbounded. Comma is also the one uniform rule - the same separator on every command - so the user
    never has to guess which verb wants which shape. Each target runs the command's normal
    single-target path (same guards, same per-target report); a failure on one does not abort the
    rest; the set is bounded. A single target with no comma behaves exactly as before. The keyword
    `all-services` (kill only) is the whole system set expressed as one target.

### Help output shape (normative)

Every usage row carries **both** a placeholder signature **and** a real example - the
placeholder teaches the grammar, the example teaches what real input looks like (not
everyone knows what `<path>` should be filled with):

```
<util> <version> - <one-line description>

usage:
  <util>                      <default behaviour>
      e.g. <real example>
  <util> <subcmd>             <what the subcommand does>
      e.g. <real example>
  <util> version              print the version
  <util> help                 print this message

subcommand help:
  <util> <subcmd> help        (same shape, focused on the subcommand)
```

`version` output is the name + version number, then the creator credit:

```
<util> <version>
Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors.
```

---

## 2. Built-in vs standalone-service utilities

Two implementation shapes exist, and each utility's doc states which it is:

- **Shell built-in.** Runs inside the shell's own protection domain, using caps the
  shell already holds. Cheap and simple; appropriate for trivial output commands
  (`echo`, `clear`) and read-only info that the shell is already authorised for
  (`mem`, `cores`, `about`, `date`, `status`, `caps`). The shell's authority is not
  *widened* for these - they only read or print.

- **Standalone service brokered by the shell.** A separate task the shell spawns,
  holding a contract-scoped least-authority cap set and nothing more. Used when the
  utility must *not* run alongside dangerous authority. `observe` is the canonical
  example: it needs only to read metrics, so it is its own service holding an
  introspection cap - it cannot kill or restart anything *by construction*, not by
  being careful (§3.1, §26.9). See `1_observe.md` §7.

The dividing question is least authority: if a command would otherwise execute in
the same domain as `spawn`/`kill`/`restart`, prefer a standalone service.

---

## 3. Conformance status (as-built, honest)

**As of 2026-06-14, every utility conforms.** Each one implements its own
`<util> help` (usage with a real example per row) and `<util> version` (number +
"Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors."), and each real subcommand (`date epoch`,
`observe now`, `drives flash` / `label` / `reset`) has its own `<util> <subcmd> help`.

The shell built-ins are driven by a single `help_block` helper (the format lives in
one place, so all of them render identically), with `<util> help` / `<util> version`
intercepted uniformly in the command dispatch; `observe` (a standalone service) was
already spec-first conformant. The earlier gap - built-ins documented only by the
top-level `help` list - is closed.

The last-open item is now closed too: the top-level **`help`** command conforms.
Bare `help` is still the categorised command list, but its first line now carries the
version header (rule 6: `help 0.1.0 - GodspeedOS shell commands`), and `help help` /
`help version` resolve like any other utility's. So **every** command the shell
dispatches - including `help` itself - self-documents and reports a version.
