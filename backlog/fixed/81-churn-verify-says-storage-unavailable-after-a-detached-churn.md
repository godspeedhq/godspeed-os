# 81. `churn verify` says "storage unavailable" right after a detached churn

**Status: CLOSED 2026-10-11 (`bf01d20d`, `feat/ask`) - the shell's channel to `fs` was one reply out of
phase; the SDK now drains abandoned replies before every mailbox request. `osdev test jobs` 60/0.** Found
2026-10-09 on `feat/audio-finish`; present on `main` (1c477970) and on v0.22.0 too. The record of how it
was found is kept below; the cause is the last section.

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

## Cause and fix (2026-10-11)

Step 1 above was done first (`e.as_str()`): the error was `Malformed`. Instrumenting the `fs` client
showed what that meant - every shell request to `fs` was receiving its PREDECESSOR'S reply (`tag 83 got
81`, `tag 84 got 82`) and rejecting it on the tag. The channel had fallen one reply behind and stayed
there:

1. During `background drives check`, `copier`'s sweep held `fs` for about 6 s (the op 27 lines above).
2. The shell's history save after the next command (`OP_WRITE_FILE`, best-effort, short patience) timed
   out waiting behind it. Its reply arrived later and stayed in the shell's reply mailbox.
3. Every later request took the oldest reply in the mailbox - the previous request's - so each failed
   and left its own reply for the next. `churn verify` and `churn reset` were simply the first commands
   whose failure the suite checked. Every history save from then on was lost too, silently.

The repair already existed: `drain_stale_replies` describes exactly this desync, and
`request_with_reply_deadline_into` drained before issuing (`backlog/fixed/37`). But the two other paths that
wait on the reply mailbox did not - `request_with_reply_call_err`, which `gs::call` and therefore every
`gs::fs` call uses, and `request_with_reply_deadline_outcome_into`. One helper, `drain_before_request`,
now drains and logs, and all three call it. The log says so when it happens:

```
sdk: discarded 2 abandoned repl(y/ies) before a request to `fs` - an earlier request timed out
...
churn verify: 7 file(s) checked, 0 empty, NONE torn - every file holds one generation end to end
```

QEMU: `osdev test jobs` 60/0 (was 59/1), shell 272/0/2, identity 24/0, files 249/0. Step 3 above (run
`jobs` before a release) is still worth doing; it is how this shipped twice.
