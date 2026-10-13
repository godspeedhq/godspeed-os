# 89. Waits that trap the user without `[q] quit`

**Status: OPEN, 2026-10-13.** Found by `audits/2026-10-13-quit-audit.md`, which has the evidence (file:line for
every site), what passes and what was not checked. Nothing is fixed yet: the operator decides the order.

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

The operator chooses: the primitive first (it removes Q1, Q2, Q5 and most of Q4 at once), or the user-facing
traps first (Q3 `run`, Q3 `chaos kill-storm`, Q4 `delete ... recursive`, Q6 `drives flash`), which are small and
independent.

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

Not exercised by this table, because QEMU does not slow them: the waits on other services (`hardware`, `date`,
`events persist`, the radios, the network). They answered in under 2 s here, so the table records them as quick,
not as passing. The next slice slows those services the same way.

**Seen once, cause unknown (2026-10-13):** one `osdev test script` run on this slice timed out in its first
`selfcheck` (300 s) and every case after it with it; three runs since, on the same code, passed 11/0 and
`selfcheck` 594/0. The slow `fs` did not leak into it (each suite rebuilt `fs` with its normal features). The
suite now saves its serial (`build/tests/script_serial.log`), so a repeat leaves something to read.
