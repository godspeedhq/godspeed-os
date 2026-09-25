# 52 - `osdev test shell` flakes on `sock` and `serve`, and the failure CASCADES into `scrollback`

**Status:** OPEN - measured across five runs on one machine in one session, not root-caused.
**Found:** 2026-09-25, during Audit 7, by a control experiment that was looking for something else.

## What was measured

Five runs of `osdev test shell` on 2026-09-25, same machine, same session. The only difference
between the trees is **comment text** - `git diff -U0` reports ZERO non-comment lines changed in all
seven edited files - so every run below is the same system semantically.

| # | tree | result | failures |
|---|------|--------|----------|
| 1 | `6f4ccd76` | **206 / 0** | - |
| 2 | + Audit 7 comment edits, run under load | 205 / 1 | `sock` |
| 3 | + same edits, run ALONE | 199 / 7 | `sock`, `serve` x3, `scrollback` x3 |
| 4 | edits stashed (control) | **206 / 0** | - |
| 5 | edits restored | **206 / 0** | - |

The same tree both passes and fails, so this is not a code defect introduced by those edits. Run 3
was not under contention, which rules out host load as the whole story - run 2 was, run 3 was not,
and run 3 was worse.

## The shape of it

**The failures cluster, and they cluster on the two commands that leave the machine.** `sock` sends a
DNS query through SLIRP to the host's real resolver; `serve` needs a host-side TCP client to connect
INTO the guest. Both depend on something outside QEMU answering in time. Nothing else in the suite
does.

**`scrollback` is a CASCADE, not a third fault.** In run 2 `sock` failed alone and `scrollback`
passed. In run 3 `serve` failed and the three `scrollback` cases immediately following it failed too.
They are consecutive in the run order, and every test after them passed - including the whole chaos
block and `kill shell`. So the session recovers; what it does not do is recover in time for the next
three assertions.

That is worth separating because it inflates the apparent damage: one flaky network test reads as
seven failures, which makes a variance problem look like a regression.

## Why this matters more than a re-run

The operator's standing bar is that **intermittent is not acceptable** - it has to be
deterministically working. A suite that is green on the third attempt is not green; it is a suite
that cannot distinguish "the code broke" from "the network was slow", and it was very nearly read as
a regression here. The only reason it was not is that a control run with the changes stashed happened
to be cheap.

There is a second cost, already paid once in this session: run 2's single `sock` failure was
attributed to host contention and moved past. It was not until run 3 produced seven that anybody
looked properly.

## What has NOT been established

- **Whether the host or the guest gives up first.** `sock` reported "nothing came back" and `serve`'s
  three cases are one connection attempt, so either side could be the one timing out.
- **Whether it reproduces on another machine**, or on this one tomorrow. One session, one host.
- **Whether the `serve` -> `scrollback` cascade is a shell state problem or a harness sync problem.**
  The harness reads the serial stream, so a late reply can desync what it matches against - which is
  the same class as the `CallDeadline` lesson (CLAUDE.md 8.2), one layer up.

## The cheapest next step

Not a rewrite. Instrument the two tests so a failure says WHICH side was slow: record the elapsed time
for `sock`'s round trip and `serve`'s accept, and print it on failure. A timeout that reports its own
duration separates "never arrived" from "arrived 200 ms after we stopped listening", and that one
distinction decides whether the fix is a bound or a bug.

Then, if it is a bound: these are the only two suite tests that depend on a peer outside QEMU, and
`docs/gsfs-carnage.md`'s reasoning applies - a test whose result depends on something we do not
control needs either a bound generous enough to be meaningless or a peer we do control.

## Related

- `backlog/28` - `serve`'s release "does not always land", already recorded.
- The `sock` path itself was fixed twice on 2026-09-24/25 (`82705c59`, `5716da17`); this entry is
  about the TEST's reliability, not the command's correctness, which the T630 is the judge of.
