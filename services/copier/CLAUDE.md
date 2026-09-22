# copier/

The service behind `background` (`utilities/55_background.md`, `docs/job-control-design.md`).

## What it is

A job runner for work that must outlive the prompt. The shell spawns it on `background <cmd>`, sends
it one job, and stops thinking about it; `jobs` and `foreground` ask it where it has got to.

Two kinds of job, and the test for adding a third is **not** "is the command slow":

| kind | work |
|---|---|
| `KIND_COPY` | stream one file `src` to `dst` in `IO_CHUNK` (3556-byte) pieces |
| `KIND_DELETE_TREE` | one `OP_DELETE_TREE` request; `fs` does the whole walk |

The test is **is the command's value its EFFECT or its OUTPUT.** This service holds no
`console_push` capability, so a command whose product is a report has nowhere to write it. That is
what rules out `selfcheck`, `chaos`, `find`, `run` and `drives check` - not a policy, a fact about
what a job can reach.

## Why it is a service at all

`docs/job-control-design.md` §4 has the full argument. The short form: this shell has no threads and
its main loop blocks on keys, so detached work is either a task or a state machine advanced between
keystrokes. As a task, stopping it is `kill`, the shell's 256 KiB stack does not have to hold it,
and a job that faults is a service that faults rather than a shell that faults.

## What it holds, and what it does not

`fs` and its log. **No console**, no spawn, no reboot, no network. A detached job therefore cannot
write over a prompt somebody is typing at, because it holds nothing that reaches the console.

The honest limit: the design describes minting a READ cap for the source and a WRITE cap for the
destination and handing over exactly those. The shell's `spawn` takes a name and nothing else
(`utilities/10_spawn.md` §5), so the real bound is this contract's `fs`, entire. Strictly less than
the shell's own authority, and not the bound the design claimed.

## Spawned on demand, NOT restarted

The `recorder` shape, for the same reason: a respawned copier would not know what it was copying, so
it would be alive and doing nothing while `jobs` reported `running`. The shell sees the death as the
job being `lost`, which is the truth.

## Three things that are easy to get wrong here

1. **The `fs` reply is `[tag, status, ..]`, not `[status, ..]`.** `fs` echoes the correlation tag as
   byte 0. Reading byte 0 as the status makes every success look like a failure - the file is
   written and the service reports that it could not be. The shell does not hit this only because
   its `fs_take_tagged` strips the tag first.
2. **`Slow` is not `Failed`.** A passed deadline means `fs` is alive and busy; a failed send means
   the endpoint is gone. Only `READ_AT` and `WRITE_AT` may be re-sent, because a positional write
   into an already-allocated extent is idempotent. `WRITE_NEW` and `DELETE` are not, and re-sending
   one reports a failure for work that succeeded (carnage §3.5). This was a real bug: a `drives
   check` held `fs` for 6.2 seconds and a job died claiming "writing the destination failed".
3. **While a tree delete runs, this service answers nothing.** It is single-threaded and blocked in
   the request. The shell must read that as busy, not as gone, or a job is declared `lost` at
   exactly the moment it is working.

## An interrupted copy deletes its destination

`fs` allocates the whole extent up front, so an interrupted copy leaves a file of the right size
with an undefined tail - worse than no file, because `dir` shows the expected bytes and nothing says
the content is garbage. Cancel and failure both remove it, and say so. A removal that itself fails
is reported too.

## Tests

`osdev test jobs` (36/0), part of `osdev test fs-all`.
