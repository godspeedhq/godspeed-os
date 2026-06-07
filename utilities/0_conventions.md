# Utility Conventions (shared)

**Status:** Canonical. These rules define how *every* GodspeedOS utility behaves.
They were first written inside `1_observe.md` §3 and hoisted here once a second
utility existed (per the note in that section — pull the abstraction into
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
3. **`help` is the word — the only form. No flags, no synonyms.** There is exactly
   one way to ask for help: the word `help`. No `-h`, no `--help`, no hidden
   aliases. A tolerated-but-undocumented synonym would itself be a hidden, unsaid
   rule — the silent behaviour the system forbids (§26.4, §26.5). `-h` is simply
   `unknown:`, and that response *teaches* the real word.
4. **Subcommands are words, never single-letter flags.** `observe now`, not
   `observe -n`. A word means the same thing across every utility; flag letters
   collide and drift (`-n` = "now" here, "number" there). This is the `ls -Sslah`
   wall-of-letters problem GodspeedOS rejects. Typing economy is a shell-ergonomics
   concern (completion, history — future), not a reason to abbreviate vocabulary.
5. **Every utility has a version, reported by `<util> version`.** Per utility, not
   per subcommand — subcommands evolve with their parent and inherit its version.
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

### Help output shape (normative)

```
<util> <version> — <one-line description>

usage:
  <util>              <default behaviour>
  <util> <subcmd>     <what the subcommand does>
  <util> version      print the version
  <util> help         print this message

subcommand help:
  <util> <subcmd> help
```

---

## 2. Built-in vs standalone-service utilities

Two implementation shapes exist, and each utility's doc states which it is:

- **Shell built-in.** Runs inside the shell's own protection domain, using caps the
  shell already holds. Cheap and simple; appropriate for trivial output commands
  (`echo`, `clear`) and read-only info that the shell is already authorised for
  (`mem`, `cores`, `about`, `date`, `status`, `caps`). The shell's authority is not
  *widened* for these — they only read or print.

- **Standalone service brokered by the shell.** A separate task the shell spawns,
  holding a contract-scoped least-authority cap set and nothing more. Used when the
  utility must *not* run alongside dangerous authority. `observe` is the canonical
  example: it needs only to read metrics, so it is its own service holding an
  introspection cap — it cannot kill or restart anything *by construction*, not by
  being careful (§3.1, §26.9). See `1_observe.md` §7.

The dividing question is least authority: if a command would otherwise execute in
the same domain as `spawn`/`kill`/`restart`, prefer a standalone service.

---

## 3. Conformance status (as-built, honest)

The full convention (per-command `help` + `version`, help-shape header) is
implemented today **only by `observe`**, which was built spec-first against these
rules. The simple shell built-ins (`echo`, `about`, `mem`, `cores`, `date`,
`clear`, `status`, `caps`, `spawn`, `kill`, `restart`, `reboot`) are documented by
the shell's top-level `help` (categorised command list) but do **not** yet
implement their own `<util> help` / `<util> version`. Closing that gap — giving
every built-in its own `help`/`version` — is future work, recorded here rather than
left as a silent inconsistency (§26.4). Each built-in's doc repeats this note.
