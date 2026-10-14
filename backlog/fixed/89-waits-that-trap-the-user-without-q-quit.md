# 89. Waits that trap the user without `[q] quit`

**Status: FIXED 2026-10-14 in QEMU; all five hardware cards passed.** `osdev test quit`, the test the
operator named, shows the rule met: 35 cases, 0 below the bar, `fs` and four services slowed (the tables
below). Found by
`audits/2026-10-13-quit-audit.md`, which has the evidence (file:line for every site), what passes and what was not
checked. The observer route is built, slice by slice, with `osdev test quit` at 35 cases and none below the bar,
and a ratchet holds it (the end of this file). Not closed by it, and recorded below: the waits between
commands, which no command clock covers, and the `q` on `wifi radio off hard` question.

## The rule asked for

"Any utility or blocking action that requires a user to wait should have [q] quit or [b] background (if it's
backgroundable) beyond 2 seconds ... GodspeedOS should always be responsive and should never trap the user
behind a block." Waits on the user (confirms, `input`, full-screen programs with their own quit key) are
exempt.

## What is wrong, by cause

- **Q1. `[q]` is opt-in, not built into the wait.** Every default wait in `gs` and the SDK is silent and cannot
  be quit; 32 `Fs` handles and 32 service requests in the shell are such waits, most over 2 s. No checker
  notices a new one.
- **Q2. A loop of fast requests never shows `[q]`.** The 2 s clock is per request, so `read`, `copy`,
  `copy ... recursive`, `find`, `tree`, `dir` and a big `| write` run unbounded with no `[q]`, and `q` is not
  read until they end.
- **Q3. Commands with no console read at all**: `run` and the library scripts (`abortable = false`, and loops
  never look for `q`); the shell's `chaos kill-storm`, `mem-pressure`, `spawn-storm` (between spawns only) and
  `flood-storm`, which also settle with 60,000 yields - a count, not a duration.
- **Q4. Silent single waits over 2 s**: `delete ... recursive` (120 s, and the same in `selfcheck`'s tidy,
  `churn reset`, `ensure_scratch`); the plain mutating commands (5 s each); `net lease` / `net dns` failure /
  `sock` / `serve` start (`net_link_up`, `ns_deadline`); `foreground` / `jobs` / `background` (8-28 s, `q` and
  `b` read between 8 s asks); `events persist` (8-16 s); `date` / `date sync`; tab completion; the history on
  the key path; `spawn` / `restart` lists; `hardware`; the sticky resume before the first prompt.
- **Q5. `q` works and says nothing**: `wait N`, `ping <ip> count N`, `sock`'s `send_to`, `selfcheck <part>`.
- **Q6. `q` works and says the wrong thing**: `drives flash` / `reset` ("storage unavailable" while formatting),
  `wifi join` ("not joined" while joining), `read`, `copy` (a partial destination), `drives label`, `for line`,
  `foreground` ("stopped" unacknowledged), the never-cleared `ctx.fs_unknown`.
- **Q7. `q` is shown and lost**: `wifi_ask` clears `ctx.quit` at its start, so `wifi hardware` and
  `wifi hardware use` go on after a `q`; `fcap reuse` shows `[q]` and ignores it; inside `selfcheck` and
  `repeat`, a `q` taken by the running statement ends only that statement.
- **Q8. The written rule** (`utilities/0_conventions.md` rule 10) has no 2 s threshold, no `[b]`, an exception
  that is now stale (the `wifi radio` requests became quittable on 2026-10-13), dated notes describing gaps
  fixed on 2026-10-10, and a wrong reason for its "still silent" list (four of the five are not kernel `Call`s).
  Several utility specs still declare non-conformance that was fixed. This part is documentation and is the
  audit's to correct once the rule's new wording is agreed.

## The route (the audit's section 7)

The fix is a primitive, not 64 edits: a reply-matched sliced wait in the SDK, turned on by holding the
console-read slot rather than by an argument, the `[q] quit` words printed in one place, a COMMAND-level 2 s
watch so a loop of fast requests is covered, a sub-second clock, one type-ahead policy, `[b]` as a leave key
with honest "still running" wording, and a ratchet checker that refuses a new silent wait. Q3, Q6 and Q7 need
their own changes either way. Q8 is the rule text, rewritten to say: 2 s, on the slowest board; `[q]` always,
and `[b]` where the work can run detached; never trapped; and what `q` must say when the work cannot stop.

## The route chosen (2026-10-13)

The operator flipped section 7: **the shell is the observer** (`audits/2026-10-13-quit-audit.md` 7a). For a
command that is a separate program the shell owns the 2 s clock, the `[q]` words and the kill; for a built-in it
starts one COMMAND clock at Enter that every wait reads between slices. `[b]` is derived from authority: a
separate program, holding no console authority, whose output fits a bounded transcript - with a reasoned
"never" for `selfcheck` and `chaos`.

## Next step

*(Superseded: the operator chose the observer route, below, and it is built.)* The hardware cards, then
`backlog/fixed/`. Whether the waits between commands need a clock of their own is the one open design
question.

## Evidence: `osdev test quit` (QEMU, 2026-10-13)

`fs` built with `slow-replies` (700 ms per reply, 4 s for delete tree, check, scrub and flash); 22 kinds of wait,
each sent, watched for `[q] quit`, then quit. The bar is rule 10's: `[q]` by 4 s, the prompt back within 3 s.

**Before** (the first slice only, `1395264a`): the first run found a defect of its own - after a `q`, the abandoned
request's late reply was taken by the NEXT command, so `mkdir`, `rename`, `cd` and the recursive `copy` and
`delete` failed for nothing ("storage unavailable", "see fs's log"). `run` could not be stopped and held the shell,
so every row after it measured nothing. `wait 6` said nothing; `help | write` said nothing; every quit was
reported as a failure ("read: storage error", "copy: failed - fs refused the allocation", "write: append failed").

**After** (the command clock, the tag-sifted watched request, scripts and `selfcheck` stopping on the command's
quit, `wait` under the watch, the words): every case that runs past 2 s says `[q] quit` by 3.7 s at the latest,
the prompt is back within 0.3 s, and each says what it did. A stray late reply is dropped by its tag.

| command | verdict | [q] after | [b] | prompt back after q | ran (no q) | last line |
|---|---|---|---|---|---|---|
| `dir /q` | QUICK | - | - | - | 1.5 s | 3 entries |
| `read /q/big.txt` | PASS | 2.0 s | - | 0.2 s | - | edit <path>            fread: stopped |
| `copy /q/big.txt /q/big2.txt` | PASS | 2.7 s | - | 0.2 s | - | copy: stopped - the destination is incomplete; delete it, or copy again |
| `find h2.txt /` | PASS | 2.4 s | - | 0.2 s | - | fs: op 14 took 702419 us, 2 block ops, 0% of it inside them |
| `tree /` | PASS | 2.4 s | - | 0.2 s | - | ├── system/ |
| `write append /q/big.txt more` | PASS | 2.6 s | - | 0.1 s | - | write: stopped - the file is as it was, or holds the whole change - never half (`read` it) |
| `help | write /q/h3.txt` | LATE NOTICE | 3.6 s | - | 0.1 s | - | pipe: stopped |
| `mkdir /q/m1` | QUICK | - | - | - | 1.8 s | created /q/m1 |
| `rename /q/big2.txt big3.txt` | QUICK | - | - | - | 1.5 s | renamed /q/big2.txt → big3.txt |
| `copy /q/tree /q/tree2 recursive` | PASS | 2.0 s | - | 0.3 s | - | copy: stopped |
| `delete /q/tree2 recursive` | PASS | 2.4 s | - | 0.1 s | - | delete: stopped - fs goes on removing the tree; `dir` shows what is left |
| `drives check` | PASS | 2.0 s | - | 0.1 s | - | drives: aborted (the filesystem finishes its pass in the background) |
| `drives scrub` | PASS | 2.2 s | - | 0.1 s | - | drives: aborted (the filesystem finishes its pass in the background) |
| `cd /q/tree/a` | LATE NOTICE | 3.7 s | - | 0.1 s | - |  |
| `seal /q/big.txt yes` | LATE NOTICE | 3.6 s | - | 0.1 s | - | seal: stopped |
| `wait 6` | PASS | 2.4 s | - | 0.1 s | - | fs: op 10 took 736586 us, 19 block ops, 4% of it inside them |
| `hardware` | QUICK | - | - | - | 0.9 s | 3 device(s) with a driver, 5 without - hardware <device> for one in full |
| `date` | QUICK | - | - | - | 0.8 s | (last known 2026-10-13 21:43:53 UTC from /system/services/time/floor - a floor, not a read |
| `events persist status` | QUICK | - | - | - | 1.5 s | events persist: lines were LOST - the window wrapped faster than the disk could take them |
| `chaos kill-storm events 3` | QUICK | - | - | - | 1.6 s | verdict: PASS |
| `selfcheck files` | PASS | 2.7 s | - | 0.3 s | - | selfcheck: STOPPED at your request after 1 of 1 part(s) - this is NOT a pass. |
| `run /q/loop.gsh` | PASS | 2.3 s | - | 0.1 s | - | run: STOPPED at your request - the statements after this point did not run |

**After the service slice** (`f7a18a97`: every service request under the watch, and `time`, `recorder`, `copier`
and `net-stack` slowed to 2.5 s per reply): 28 cases, none below the bar. `date`, `events persist status` and
`stop`, `net lease`, `net dns`, `background copy` and `jobs` with a live job now each say `[q]` by 2.9 s and come
back within 0.7 s. Still not exercised: `hardware`, whose supervisor and drivers are not slowed (slowing the
supervisor would slow every spawn), and the shell's chaos modes, which ran quick at 3 rounds.

| command | verdict | [q] after | [b] | prompt back after q | ran (no q) | last line |
|---|---|---|---|---|---|---|
| `dir /q` | PASS | 2.7 s | - | 0.0 s | - | 4 entries |
| `read /q/big.txt` | PASS | 3.3 s | - | 0.1 s | - | read: stopped |
| `copy /q/big.txt /q/big2.txt` | PASS | 3.6 s | - | 0.1 s | - | copy: stopped - the destination is incomplete; delete it, or copy again |
| `find h2.txt /` | PASS | 3.4 s | - | 0.1 s | - | fs: op 25 took 716046 us, 2 block ops, 0% of it inside them |
| `tree /` | PASS | 3.6 s | - | 0.1 s | - |  |
| `write append /q/big.txt more` | PASS | 3.6 s | - | 0.1 s | - | write: stopped - the file is as it was, or holds the whole change - never half (`read` it) |
| `help | write /q/h3.txt` | PASS | 3.5 s | - | 0.1 s | - | pipe: stopped |
| `mkdir /q/m1` | PASS | 2.8 s | - | 0.1 s | - | mkdir: failed (already exists, or parent missing? try 'mkdir <path> parents') |
| `rename /q/big2.txt big3.txt` | QUICK | - | - | - | 1.5 s | rename: failed - see fs's log |
| `copy /q/tree /q/tree2 recursive` | PASS | 2.0 s | - | 0.2 s | - | copy: stopped |
| `delete /q/tree2 recursive` | PASS | 3.6 s | - | 0.1 s | - | delete: stopped - fs goes on removing the tree; `dir` shows what is left |
| `drives check` | PASS | 2.0 s | - | 1.2 s | - | drives: aborted (the filesystem finishes its pass in the background) |
| `drives scrub` | PASS | 3.8 s | - | 0.1 s | - | drives: aborted (the filesystem finishes its pass in the background) |
| `cd /q/tree/a` | PASS | 3.8 s | - | 0.1 s | - | fs: op 29 took 4146556 us, 13 block ops, 0% of it inside them |
| `seal /q/big.txt yes` | PASS | 2.4 s | - | 0.0 s | - | sealed /q/big.txt |
| `wait 6` | PASS | 2.0 s | - | 0.1 s | - |  |
| `hardware` | QUICK | - | - | - | 0.8 s | 3 device(s) with a driver, 5 without - hardware <device> for one in full |
| `date` | PASS | 2.7 s | - | 0.1 s | - | no network link - plug the cable in and it will set itself, or run 'date sync' |
| `events persist status` | PASS | 2.9 s | - | 0.1 s | - | events persist: recorder did not answer |
| `net lease` | PASS | 2.5 s | - | 0.1 s | - |  |
| `net dns example.com` | PASS | 1.5 s | - | 0.7 s | - | net: aborted |
| `jobs` | QUICK | - | - | - | 0.8 s | no jobs |
| `background copy /q/big.txt /q/bg.txt` | PASS | 2.3 s | - | 0.1 s | - | shell: ended `copier` - its job is over and nothing it said is left unread |
| `jobs` | PASS | 2.9 s | - | 0.1 s | - | 1    running  100%      copy /q/big.txt /q/bg2.txt |
| `events persist stop` | PASS | 2.6 s | - | 0.0 s | - | block-driver: op 2 spent 7778 us in the driver (slow #3) |
| `chaos kill-storm events 3` | QUICK | - | - | - | 1.6 s | verdict: PASS |
| `selfcheck files` | PASS | 2.7 s | - | 0.2 s | - | selfcheck: STOPPED at your request after 1 of 1 part(s) - this is NOT a pass. |
| `run /q/loop.gsh` | PASS | 2.2 s | - | 0.1 s | - | run: STOPPED at your request - the statements after this point did not run |

**Seen once, cause unknown (2026-10-13):** one `osdev test script` run on this slice timed out in its first
`selfcheck` (300 s) and every case after it with it; three runs since, on the same code, passed 11/0 and
`selfcheck` 594/0. The slow `fs` did not leak into it (each suite rebuilt `fs` with its normal features). The
suite now saves its serial (`build/tests/script_serial.log`), so a repeat leaves something to read.

**The chaos and message slice (2026-10-14):** the shell's chaos modes ask the command's watch at every round and in
their waits and settle (`chaos_stopped`, `chaos_settle`), and stop with "STOPPED at your request after N of M
round(s) - this is not a pass"; `spawn-storm` stops spawning on a quit and still removes every task it started.
Measured: `chaos mem-pressure 10` `[q]` 2.6 s, stopped after 9 of 10; `chaos flood-storm events 20` `[q]` 2.0 s.
`kill-storm` and `spawn-storm` finished under 2 s in QEMU, so their quit is in the code and not yet seen. The words:
`drives flash` and `drives reset` say the format goes on in `fs`; `wifi join` says the radio may still be joining;
`foreground` says "stopped" only when `copier` acknowledged the cancel. None of the three can run in this table
(a flash would wipe the test disk, QEMU has no radio, `foreground` needs a job number) - shown by the code only.
Quit table 31 cases, 0 below the bar; files 270/0, shell 272/0, script 11/0.

**The `[b]` slice (2026-10-14): a detachable command typed at the prompt runs as a job.** `copy <file> <dst>`,
`delete <dir> recursive`, `drives check` and `drives scrub`, typed, start as the job `background` would start
and the console attaches to it (`run_typed_as_job`), so the notice is `[q] quit   [b] background` and `b`
answers `[backgrounded] job N`. `copier` writes the shell's own lines, so what the command prints is the same
either way, after one line naming the job; a check or scrub that finds damage ends the job as failed, as the shell's form returns an error. A
typed job is not in `jobs` and takes no number unless it goes on without the console: detached with `b`, or
left running by a `q` it cannot stop (a check, a scrub, a tree delete, or one whose start was not yet answered). In a script, a pipe or a function, or
while a job is already running, the command runs in the shell with `[q]` alone.

Four things the table found on the way, each fixed and re-measured, in order: the attach loop waited on
`copier` a quarter of a second at a time (3.4 s from `b` to the prompt), so it looks every 50 ms instead; a
status question before every typed start put a slow `copier`'s answer in front of the keys, so it is asked only
when a job is believed live; `q` on a copy waited on two of `copier`'s answers (5.3 s), so it takes one short
look; and the start itself held both keys (up to 5.6 s), so it is sent and its answer read by the attach loop.
`q` before `copier` has answered the start says exactly that.

Quit table, `copier` slowed to 2.5 s a reply as well as `fs`: 35 cases, 0 below the bar; the four `~` cases
press `b`.

| command | verdict | [q] after | [b] | prompt back after q | ran (no q) | last line |
|---|---|---|---|---|---|---|
| `dir /q` | QUICK | - | - | - | 2.9 s | 4 entries |
| `read /q/big.txt` | PASS | 3.0 s | - | 0.1 s | - | read: stopped |
| `copy /q/big.txt /q/big2.txt` | PASS | 3.5 s | yes | 0.4 s | - | copy: stopped - copier had not yet answered the start, and the cancel goes in behind it; ` |
| `find h2.txt /` | PASS | 3.3 s | - | 0.1 s | - |  |
| `tree /` | PASS | 2.7 s | - | 0.1 s | - | / |
| `write append /q/big.txt more` | PASS | 3.5 s | - | 0.1 s | - | write: stopped - the file is as it was, or holds the whole change - never half (`read` it) |
| `help | write /q/h3.txt` | PASS | 3.5 s | - | 0.1 s | - | pipe: write failed mid-stream |
| `mkdir /q/m1` | QUICK | - | - | - | 3.3 s | created /q/m1 |
| `rename /q/big2.txt big3.txt` | QUICK | - | - | - | 2.1 s | rename: failed - see fs's log |
| `copy /q/tree /q/tree2 recursive` | PASS | 2.2 s | - | 0.1 s | - | copy: stopped |
| `delete /q/tree2 recursive` | PASS | 2.5 s | - | 0.1 s | - | delete: stopped - fs goes on removing the tree; `dir` shows what is left |
| `drives check` | PASS | 3.6 s | yes | 0.1 s | - | check: stopped waiting - copier had not yet answered the start; if it began, it is one req |
| `drives scrub` | PASS | 2.6 s | yes | 0.1 s | - | scrub: stopped waiting - copier had not yet answered the start; if it began, it is one req |
| `cd /q/tree/a` | PASS | 3.5 s | - | 0.1 s | - |  |
| `seal /q/big.txt yes` | PASS | 2.6 s | - | 0.1 s | - | seal: stopped |
| `wait 6` | PASS | 2.6 s | - | 0.1 s | - |  |
| `hardware` | QUICK | - | - | - | 2.1 s | 3 device(s) with a driver, 5 without - hardware <device> for one in full |
| `date` | PASS | 3.4 s | - | 0.1 s | - | no network link - plug the cable in and it will set itself, or run 'date sync' |
| `events persist status` | PASS | 2.8 s | - | 0.1 s | - | events persist: recorder did not answer |
| `net lease` | PASS | 2.5 s | - | 0.1 s | - | shell: discarded a net-stack reply for tag 0 while awaiting 1 (an earlier request was over |
| `net dns example.com` | PASS | 1.5 s | - | 0.7 s | - | net: aborted |
| `jobs` | PASS | 2.1 s | - | 0.1 s | - | 3    running     -      drives scrub |
| `background copy /q/big.txt /q/bg.txt` | PASS | 2.7 s | - | 0.1 s | - | background: refused - no reason recorded |
| `jobs` | PASS | 2.9 s | - | 0.1 s | - | reason: reading the source failed |
| `events persist stop` | PASS | 2.8 s | - | 0.0 s | - | shell: ended `recorder` - its capture is closed |
| `chaos kill-storm events 20` | QUICK | - | - | - | 2.1 s | verdict: PASS |
| `chaos flood-storm events 20` | PASS | 2.2 s | - | 0.1 s | - | verdict: PASS (queue saturated + service drained + stayed alive) |
| `chaos mem-pressure 10` | PASS | 2.5 s | - | 0.1 s | - | chaos mem-pressure: STOPPED at your request after 10 of 10 round(s) - this is not a pass;  |
| `chaos spawn-storm 40` | QUICK | - | - | - | 0.9 s | verdict: PASS (ceiling held loudly + full reclaim + no service lost) |
| `selfcheck files` | PASS | 2.9 s | - | 0.1 s | - | selfcheck: STOPPED at your request after 1 of 1 part(s) - this is NOT a pass. |
| `drives check` | PASS | 2.7 s | yes | 0.1 s | - | [backgrounded] job 4 |
| `drives scrub` | PASS | 2.7 s | yes | 0.1 s | - | [backgrounded] job 5 |
| `delete /q/tree recursive` | PASS | 2.7 s | yes | 0.1 s | - | [backgrounded] job 6 |
| `copy /q/big.txt /q/big5.txt` | PASS | 2.7 s | yes | 0.1 s | - | [backgrounded] job 7 |
| `run /q/loop.gsh` | PASS | 3.2 s | - | 0.1 s | - | run: STOPPED at your request - the statements after this point did not run |

A fifth, found by the jobs suite rather than the table: a start sent to a `copier` spawned a moment before had
no name to reach and was never delivered, and the attach loop waited for its answer with no bound. The start now
waits (twenty passes of 50 ms) for the name, and an unanswered start ends after 12 s, saying so. The table above
ran on the build before that change, which touches neither the keys nor the notice; on the final build, jobs
60/0, files 270/0, shell 272/0 (2 skipped), script 11/0.

**The ratchet (2026-10-14): `scripts/quit_wait_check.py`, on the build path.** It counts, in the shell, every
wait that reads no key: the SDK's plain bounded requests, `gs::ipc::recv` and `recv_within_ms`, `gs::call`'s
plain requests, and an `Fs` or `Net` handle made without the watch. Each must say, in a comment line among the
four above it, why it cannot trap anyone (`// quit-ok: <reason>`), and the number of those exceptions (10) may
fall and may not rise. `--selftest` proves it fires on an unmarked wait and passes a marked one. What it
cannot see is said in its header: a loop of sleeps that never reads `q` (the table above measures those), and
the waits between commands, which have no command clock.

Writing it found three silent waits, each fixed:

- **A finished job's transcript was read back with no `[q]`**, up to 8 s a page (`replay_transcript`, on the
  attached finish and on `foreground`). It is under the command's watch now, sifted by op, and a `q` there says
  "(stopped reading what it said)".
- **`q` on `foreground` waited up to 8 s for `copier` to acknowledge the cancel**, silent, after a `q` that rule
  10 gives 3 s. It takes one 300 ms look, as `quit_attached` does, and says "asked to stop" when unanswered.
  The first build of this failed `osdev test jobs` 39/21: `copier` answered the cancel after the look, and the
  next unsifted question to it - the next `background` start - took that answer as its own, so every job after
  it was refused. The 8 s wait had hidden it. Every question to `copier` is now sifted by its op
  (`copier_ask_watched`), so a late answer is dropped wherever it lands.
- **Tab completion of a network name could be silent for 3 s**: two 1.5 s asks to the radio. Tab is a key, not
  a command, so there is no `[q]` to offer; the second ask is now made only for a send that never left (back
  inside 250 ms), which keeps it under 2 s.

**Between commands, not under any clock, recorded rather than closed:** the history save at Enter (before the
command clock starts), `tidy_helpers` after it, and the audio shortcuts. The history save is held to 2 s a request
(`HIST_SAVE_SECS`); none of the three was measured.

**From the docs and comments audit the same day** (`audits/2026-10-14-docs-and-comments-audit.md`), two code
edges, fixed: `[b]` was offered during a typed `copy`'s or `delete`'s first look at its path even when a job
was already running, where the command then runs in the shell and `b` does nothing - it is offered now only
when no job is believed live; and `copier` kept a mkdir helper nothing called since churn moved to its scratch
folder. One question is the operator's, not the code's: `q` on `wifi radio off hard` stops the WAIT and says
the cut may still happen, since the cut cannot be stopped once asked (rule 11 asks that quitting stop the
task; here it cannot, and the words say so).

## Hardware cards

**HP T630, 2026-10-14, passed** (`build/serial_output.log`, x86 `os-usb.img` at `3ed46d5a`, AHCI SSD):

- `drives flash data`, then `y`: formatted and mounted with no reboot; `fs` wrote the standard tree.
- A typed `drives check` ran as a `copier` job: `[q] quit   [b] background` printed 1.7 s after `copier` was
  spawned (`fs` then took 3.9 s for the sweep), and the verdict printed as the shell's own lines.
- A second typed `drives check`, then `b`: `[backgrounded] job 1` printed in the same millisecond as the key.
  `jobs` listed it `done`, and `foreground 1` said "job 1 is done" and replayed its four lines.
- `selfcheck`: `ran 592, failed 0, skipped 0`, every part with `[q] quit` on its banner, including the layout
  block (`/system` with its four folders, `/users/default`, `pwd`, `home`), `background drives scrub` as job 2
  and `assert fails background selfcheck`.
- Not exercised on this card: a typed `copy` of a large file with `q`, `delete <dir> recursive` with `b`, and
  `q` on `foreground` of a job still running (job 1 had finished). The `ResourceId(102) gen mismatch ...
  liveness=Alive` lines after `selfcheck` kills `events` are `backlog/86`, open and not this branch's.

**Dell Wyse 5070, 2026-10-14, passed** (`build/serial_output.log`, the same image, `3ed46d5a`, SATA SSD):

- `drives flash data`, then `y`: formatted in 0.23 s, mounted with no reboot.
- A typed `drives check` ran as a `copier` job: `[q] quit   [b] background` printed 1.35 s after `copier` was
  spawned, and the sweep finished 0.1 s later (1.4 s in `fs`), so the `b` pressed for it landed at the prompt.
- `selfcheck`: `ran 592, failed 0, skipped 0`; `background drives scrub` became job 1.
- Not exercised: the three checks the T630 left (a large `copy` with `q`, `delete <dir> recursive` with `b`,
  `q` on `foreground` of a running job); this disk is too fast for a check to stay running long enough.
- **Seen, not fixed (no new card mid-run):** `drives flash` printed `[q] quit` the moment `y` was answered,
  for a format that then took 0.23 s. The command clock starts at Enter and `read_confirm` does not restart
  it, so the time a person spends answering `[y/N]` counts toward the 2 s, though rule 10 exempts a wait on
  the user. Harmless (the notice is true, only early); the fix is to restart the clock when `read_confirm`
  returns, and it waits for the cards in hand to finish.
**Raspberry Pi 2, 2026-10-14, passed** (`build/serial_output.log`, the same commit, `3ed46d5a`, the USB stick):

- `drives flash data`, then `y`: the format took 20.7 s in `fs`, with `[q] quit` 1 s after the `y` (the early
  notice the Wyse showed, here over a wait that earned it).
- `dir` at the first prompt lists `/users/default` (holding `history`), and after `cd /` the root holds
  `system` and `users` and nothing else. (`home` was typed and not entered: the echo runs on into `dir` with
  no Enter between; `selfcheck` covers it.)
- A typed `drives check`: `[q] quit   [b] background` 2 s after `copier` was spawned, `b` gave
  `[backgrounded] job 1` 0.9 s later, and the sweep ran on for 34 s in `fs`. `foreground 1` said "job 1 is
  done" and replayed the four lines once it ended.
- `selfcheck`: `ran 585, failed 0, skipped 1` (the skip is `hw-enumerator`: no PCI on this board).
- **Seen, not fixed:** `jobs` while the check ran took 8 s - `[q] quit` at 2 s, then the table with the row as
  last seen (`running`). `copier` reads nothing while it is inside its one `fs` request, so the status question
  waits out `copier_status`'s 8 s and comes back `Busy`. The rule is met (a `[q]` past 2 s), but `jobs` is a
  look and should not take 8 s; for a check, a scrub or a tree delete, which are each one request, it could
  say "running" from the row without asking. Recorded with the clock edge above, for after the cards.
- `foreground 1`, typed while the check ran, took about 8 s by the operator's watch, and the log shows no
  `[q]` line for it. It opens with the same `copier_status` question, so 8 s is that question's bound; the
  missing notice has an explanation that fits both boards, not yet proved: the history save at Enter runs
  before the command clock starts and waits up to 2 s on an `fs` busy with the check, silent (the "between
  commands" gap above). Both logs then show "sdk: discarded 1 abandoned reply before a request to `fs` - an
  earlier request timed out", which is that save. So up to about 4 s from Enter can pass with no `[q]`: 2 s
  of save, then 2 s of clock. A wait the operator timed at 8 s does not fit that, and stays open. The fix
  for both is to stop the save waiting on a busy `fs`, and to stop `jobs` and `foreground` asking a `copier`
  that is inside one request; for after the cards.
- Not exercised here either: a large `copy` with `q`, `delete <dir> recursive` with `b`, and `q` on
  `foreground` of a running job.
**Raspberry Pi 4, 2026-10-14, passed** (`build/serial_output.log`, the same commit, `3ed46d5a`, the USB stick
through `xhci`):

- `drives flash` of drive 0, then `y`: 14.1 s in `fs`, `[q] quit` 0.3 s after the `y`; `tree /` showed the standard
  tree and `users/default/history`. `home` and `pwd` answered `/users/default`.
- A typed `drives check`: `[q] quit   [b] background` 1.7 s after `copier` was spawned, `b` gave
  `[backgrounded] job 1` 1.4 s later; the sweep took 21 s in `fs`.
- `jobs` during it: `[q] quit`, then the table 8 s after the clock began, the row as last seen - the edge the
  Pi 2 showed, the same on a second board.
- `foreground 1` during it: "job 1 is done" and the four lines replayed when the sweep ended, about 5 s after
  the prompt, with no `[q]` line - the Pi 2's missing notice, seen again.
- `selfcheck`: `ran 594, failed 0, skipped 0`.
- Not exercised: `q` on `foreground` of a running job (the job ended first), a large `copy` with `q`,
  `delete <dir> recursive` with `b`.
**StarFive VisionFive 2 Lite, 2026-10-14, passed** (`build/serial_output.log`, the same commit, `3ed46d5a`,
the USB stick through `xhci`):

- `drives flash` of drive 0, then `y`: 14.6 s in `fs`, `[q] quit` 0.8 s after the `y`.
- A typed `drives check`: `[q] quit   [b] background` 1.6 s after `copier` was spawned, `b` gave
  `[backgrounded] job 1` 1.3 s later; the sweep took 19.4 s in `fs`.
- `jobs` during it: `[q] quit`, then the table about 6 s later, the row as last seen - the third board.
- `foreground 1` after the sweep ended: "job 1 is done" and its four lines within 0.6 s. Just before it the
  log shows "sdk: discarded 1 abandoned reply before a request to `fs` - an earlier request timed out": the
  history save made at `jobs`'s Enter, given up while `fs` was inside the check - the save the Pi 2 and Pi 4
  explanation above rests on, seen leaving its reply behind on a third board.
- `selfcheck`: `ran 594, failed 0, skipped 0`.

All five boards have now run this branch's image. Never exercised on any of them, because the job ended
before the key: `q` on `foreground` of a running job, a large `copy` with `q`, `delete <dir> recursive` with
`b`. `osdev test quit` and `osdev test jobs` cover each in QEMU.

## After the cards: the three edges fixed (`ca40aa75`), and the SBCs slower

**Fixed in `ca40aa75`, QEMU only so far:** `jobs` and `foreground` no longer wait on a `copier` inside one `fs`
request (one 300 ms question, its answer looked for, a new one at most every 2 s); a `q` read by the status
wait now stops `foreground`'s loop, which before ran on until the job ended; the command clock runs from Enter,
and the history save is skipped while a one-request job holds `fs`; `read_confirm` restarts the clock.
`osdev test quit` 37 cases, 0 below the bar, with two new ones: `foreground` of a live check (`q` back in
0.1 s) and `jobs` during a live scrub (answered at once). `osdev test jobs` 60/0.

**VisionFive card for `ca40aa75`, 2026-10-14, passed** ("much quicker this time", the operator): during a
19.4 s check, `jobs` answered with the row (`running`) as soon as it was typed; `foreground 1` printed
`[q] quit   [b] background` at once and `b` gave the prompt back; a second `jobs` answered at once; once the
check ended, `foreground 1` replayed its four lines. `drives flash` printed `[q] quit` 1.3 s after its request
went to `fs`, a 14.5 s format. `selfcheck`: `ran 593, failed 0, skipped 1` (DNS: no internet on that network).
**Pi 4 card for the same fix (`87356d2e`, docs only after `ca40aa75`), 2026-10-14, passed:** a 19.4 s check;
`jobs` twice during it, each answered with the row at once; `foreground 1` twice, each `[q] quit   [b]
background` at once, the first left with `b`, the second attached to the end: "job 1 done" the moment `fs`
answered. Seen, not fixed: `foreground` that stays attached to the end says "job 1 done" and does NOT print
what the check found - the live loop has never replayed the transcript (`finish_attached` does, for a typed
command); `copier` keeps it, so a second `foreground 1` shows it. No `selfcheck` on this card. The Pi 2 is next.

**The SBCs were slower than the x86 machines at backgrounding and foregrounding** (the operator, 2026-10-14):
the Pi 2, Pi 4 and VisionFive each took longer than the T630 and Wyse from Enter to the notice, from `b` to
`[backgrounded]`, and in `jobs` and `foreground`. Part of it is the disk: a USB stick against a SATA SSD, so a
check runs 19 to 34 s rather than 1.4 to 3.9 s, and `copier` is deaf for all of it, which is when `jobs` and
`foreground` waited their 6 to 8 s. The logs do not show the rest: the time from `copier` spawned to the notice
is 1.3 to 2.0 s on all five, so the spawn is not visibly slower, and Enter is not in the serial capture. Not
measured further; recorded rather than opened as its own entry.
