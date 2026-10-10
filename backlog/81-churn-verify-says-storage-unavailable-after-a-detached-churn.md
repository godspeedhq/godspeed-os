# 81. `churn verify` says "storage unavailable" right after a detached churn

**Status: OPEN - found 2026-10-09 on `feat/audio-finish`, and present on `main` (1c477970) and on v0.22.0
too, so it is not new. Not investigated past the first look below.**

## What is seen

`osdev test jobs` fails one of its 58 checks, the same one every run:

```
jobs: FAIL - `churn verify` recognised what the detached writer wrote, over a NON-EMPTY set - `NONE torn`
across zero files is the vacuous pass this refactor exists to prevent
jobs: 57 passed, 1 failed
```

The serial log at the failure:

```
6    done     100%      churn 12
gsh> foreground 6
job 6 is done
  churn: writing continuously - CUT THE POWER AT ANY POINT
  churn: 327 writes, 65 renames, 65 deletes in 12s
gsh> churn verify
churn verify: storage unavailable
gsh> churn reset
churn: could not remove /churn - see fs's log
```

## Where

`services/shell/src/main.rs` `cmd_churn_verify`: its `list_dir` of `/churn` returns an error that is not
`NotFound` or `Cancelled`, and every such error prints "storage unavailable". `fs` logged no death and no
error at that moment; the lines before it are sweeps (op 27, `drives check`/`scrub`) taking about 6 s
each, so `fs` was alive. The next command's `churn reset` failed the same way.

## Reproduced

Three runs on `feat/audio-finish`, one on `main` at `1c477970`, one on the `v0.22.0` tag: the same check
failed every time, 57 passed. `osdev test jobs` was not among the suites run for v0.22.0 or v0.23.0, which
is how it shipped twice.

## Ruled out

- **The audio work** (system sounds, `copier`'s "done" sound): the failure predates it.
- **The dogfood branch** (v0.23.0): it fails on v0.22.0.

## Next

1. Print the actual `gs::Error` in `cmd_churn_verify` instead of a fixed sentence - "storage unavailable"
   names one of several errors and hides which (CLAUDE.md 26.7: a failure says what failed).
2. With that, tell a timed-out request (`OutcomeUnknown`, `fs` busy after the churn) from a refused one.
3. Add `osdev test jobs` to the suites run before a release.
