# Utility: `chaos`

**Utility:** `chaos` - bounded resilience exerciser (kill a service repeatedly, prove it recovers)
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`chaos` answers **does the system actually recover when a service dies, every time, without the
kernel falling over?** It is the executable, on-device form of the restartability invariant (§6.2,
§14.3): it *kills* a recoverable service `rounds` times and, each round, waits for a fresh instance
to come up before killing again. It then prints a per-round report and a single **`verdict: PASS` /
`FAIL`** - and the fact that the command *returns at all* is itself proof the kernel never panicked
(a panic reboots the machine).

This is a **chaos / fault-injection** test in the §22 sense: total failures are covered by the
identity tests; `chaos` lets an operator reproduce the *between* cases live on real hardware.

## 2. Invocation

| Command | Meaning |
|---|---|
| `chaos kill-storm <svc> [rounds]` | Kill one `<svc>` `rounds` times; verify it recovers each round (default 20). |
| `chaos kill-storm <svc> [n] save <path>` | Same, and also write the report to a file at the end. |
| `chaos flood-storm <svc> [rounds]` | **Saturate** `<svc>`'s IPC queue with a `try_send` burst until `QueueFull`, then verify it drains and stays alive. The *other* axis: "overwhelmed", not "gone". |
| `chaos max-carnage <target> <rounds> [yes] [seed <n>]` | **The chaos monkey:** each round, `all-services` picks a random subset of the live services - the shell and the supervisor included - floods each one it can and kills every one it picked (5b). Runs exactly the count you type; a live progress line ticks `%`/ETA; `q` aborts. A `[y/N]` confirm precedes the run; a 4th word **`yes`** skips it for unattended runs, and the warning still prints in full. `seed <n>` gives the random storm its seed; every random run prints the one it used (5b). |
| `chaos mem-pressure [rounds]` | Spawn a `mem-pressure` task that allocates to its limit, kill it, and confirm the memory is reclaimed. |
| `chaos spawn-storm [count]` | Spawn `mem-pressure` tasks until the task pool or memory ceiling REFUSES one (a loud `Err`, no panic), then kill them all and confirm full reclaim. |
| `chaos link-flap [n]` | Simulate a cable unplug/replug `n` times (default 1); `net-stack` reconfigures itself. Needs a live `nic-driver`. |
| `chaos help` / `chaos version` | Self-documentation (`0_conventions.md`). Bare `chaos` prints a short list of the modes. |

`kill-storm` clamps `rounds` to `1..=100` (`CHAOS_MAX_ROUNDS`, §26.6) - it stores per-round generation
detail in fixed stack arrays. **`max-carnage` has no round cap**: its report is a constant-size
per-*service* aggregate, so the round count is a loop counter, not a resource (same reasoning as the
unbounded supervisor respawn, §6.2). It runs exactly what you type (bounded only by `u32`); `q` aborts.
It also takes no `save` - it destroys `fs`, so a save would fight the storm; its report is
console-only (§5b).

### Targets (`<svc>`)

Only **recoverable** services are valid `kill-storm` targets - the kernel itself **cannot be killed**,
and killing a non-recoverable thing would just wedge. The list is `CHAOS_RESTARTABLE` in the shell
(the same list `kill all-services` expands to); anything else is refused:

| Target | Recovered by |
|---|---|
| `supervisor` | **the kernel** - Path C / Phase 6 (§6.2): the kernel respawns the supervisor on death, *unconditionally and forever* (no bound - a bound would re-introduce the reboot and be a DoS). |
| `block-driver` | the **supervisor** (Phase D, §6.1) - re-inits the controller on respawn. |
| `fs` | the **supervisor** (Phase D) - re-mounts to a consistent state via its crash-consistency journal (`docs/persistence.md` §6.8). |
| `xhci` / `ehci` / `dwc2` | the **supervisor** - a fresh driver is granted its device again and re-enumerates. |
| `events`, `nic-driver`, `net-stack`, `time`, `control` | the **supervisor** - a fresh instance. (`events` holds the trace ring, which a restart empties; nothing in it is a log line, §11.4.) |

`shell` is NOT a `kill-storm` target (the storm runs inside it); `max-carnage` kills it, and `kill
shell` recycles it. The supervisor respawns each of these on its own death because the supervisor's
`MANAGED` roster marks them watched (`SPAWN_FLAG_WATCHED`), so the kernel sends the supervisor a
death notification - the kernel restarts only the supervisor itself. Even if a death notification is
lost (e.g. the supervisor was itself mid-respawn during a storm), the supervisor's reconcile scans the
task table (`managed_alive`: a `task_stat` slot that is valid and not Dead) and respawns any managed
service with no live task - services self-heal.

> The **only unkillable component is the kernel** (`{kernel}`). `chaos` can storm anything above it;
> there is nothing it can do to bring the kernel down - "do anything except shotgun the kernel."

## 3. Output

```
gsh> chaos kill-storm supervisor 4
chaos kill-storm supervisor: 4 rounds - kill, then wait for the supervisor to respawn it...
=== chaos kill-storm supervisor: report ===
target: supervisor (kernel-respawned); rounds: 4
round   1: killed gen 3 -> recovered gen 4
round   2: killed gen 4 -> recovered gen 5
round   3: killed gen 5 -> recovered gen 6
round   4: killed gen 6 -> recovered gen 7
recovered: 4/4; kernel: alive (no panic - this command returned)
verdict: PASS
```

The companion kernel log shows the recovery path each round, e.g. for the supervisor:

```
kernel: supervisor died - respawning (#N) (Path C / Phase 6)
supervisor: adopted running block-driver (slot 5)   ← reconciliation: adopt the live services, don't duplicate
supervisor: ready
```

## 4. How recovery is detected

Each round reads the target's **task generation** (a restart bumps it, §7.5 - the same number
`observe` shows in its `RESTARTS` column), kills the target, then waits for a *new* generation to
appear. The wait is bounded by **real wall-clock time** (the RTC, `CHAOS_RECOVER_SECS = 8 s`), **not**
a yield count: a yield count is not portable - it was generous in QEMU but too short on real hardware
for the heavier, kernel-driven *supervisor* respawn, which made `chaos kill-storm supervisor`
under-count recoveries even though the supervisor genuinely came back every time. The loop breaks the
instant a new generation appears, so fast targets (`fs`, `block-driver`) stay fast; only a genuinely
slow recovery pays the larger budget. Before each kill `chaos` also waits for the target to be *alive*
(it may still be mid-respawn from the previous round), so no round is wasted killing a not-yet-present
task.

## 5. The report avoids a catch-22

`chaos kill-storm fs` kills the very service that stores files - so the report is **recorded in
memory** during the storm (a bounded buffer, never touching `fs`) and only **printed to the console**
at the end (fs-independent, captured by the serial log). An optional `save <path>` then materialises
it to a file *after* the target has recovered, with a bounded retry (if `fs` was the target it may
still be finishing its re-mount). If the save never lands in budget, the console report stands.

## 5a. `flood-storm` - saturate the queue

`chaos flood-storm <svc> [rounds]` is the **other resilience axis**: not "service gone" (kill-storm)
but "service **overwhelmed**". Each round it bursts **`try_send`** at the target's IPC endpoint until
the kernel returns `QueueFull` - proving the queue bounds at depth 16 (§8.5) rather than growing - then
yields to let the service drain and re-sends to confirm it recovered. It is **`try_send`, never
blocking `send`** (§8.9): blocking into a full queue would hang the shell flooding *itself*.

- **Targets:** anything with a registered recv endpoint. The shell acquires a SEND cap to `<svc>` **by
  name** (`AcquireSendCap`) - so `fs`, `events`, `block-driver`, even `supervisor` are floodable.
- **Payload:** a minimal benign message the target drains and drops - no writes, no side effects. The
  test stresses the *queue*, not the disk.
- **Verdict:** `PASS` = the service survived every flood (no `EndpointDead`) and still accepts messages.
  A flood that *crashes* a service is caught and reported - a finding, not a hang; a restartable one
  respawns and the storm continues.

> **Aside (hardening note).** This said `AcquireSendCap` was **ungated**. It is gated now: the kernel
> mints a SEND cap by name only to a caller holding `ACQUIRE_ANY` (the shell, the supervisor, the
> probes) or for a name the caller was spawned with as a send peer (`handle_acquire_send_cap`). The
> shell holds `ACQUIRE_ANY`, which is what lets it flood any named service.

## 5b. `max-carnage` - the chaos monkey

`chaos max-carnage <target> <rounds> [yes]` runs in the `chaos` SERVICE, not in the shell, and that is
what lets the shell be a victim: killing the shell does not end the run. It reads the **live task set**
(exactly what `observe now` shows) and, each round, picks its victims by the target:

- **`all-services`** - a coin flip per live service, so about half of them each round, at least one. The
  shell and the supervisor are victims like any other; the only tasks never picked are `chaos` itself
  and the `mem-pressure` tasks it spawns, and the kernel, which is not a task.
- **a service name** - that one every round; **a comma list** - every one listed, every round.

What happens to a victim: every one picked is **flooded and then killed** - its queue filled with a
`try_send` burst (the §8.6 back-pressure and queue-drained-on-death cases), then the kill. `shell` and
`fs` are only killed, never flooded, because a flood corrupts their reply streams. (In an aimed run at
one service, it is flooded and killed every round.) This said the run "rolls a creative action
mix - kill, flood, flood-then-kill, or kill-then-flood" and spared the shell; neither has been true since
the run moved into its own service. `chaos` waits for a live shell before it hands the console back.

Every service in the supervisor's `MANAGED` set is respawned on its own death; `chaos` does NOT check
that it came back - asking the supervisor, which is itself being killed, would not be ground truth. The victims are chosen with a tiny `xorshift64` PRNG. (This
said it was seeded from the TSC; it is not - the TSC is unreliable on the T630 - and the seed mixes the
hardware random number where there is one, the monotonic counter, and the wall clock when it is set.)

**The seed is printed, and can be given.** An `all-services` run says its seed when it starts and again in
its report, and `chaos max-carnage all-services <rounds> seed <n>` runs on that seed. On a display the
start line is overwritten as soon as the table draws; it stays on serial, and the report repeats it:

```
gsh> chaos max-carnage all-services 1000 yes seed 4242
chaos: seed 4242 (given) - `chaos max-carnage all-services <n> seed 4242` replays these draws, not this run's timing
...
=== chaos max-carnage: report ===
seed: 4242
```

What that buys, stated so it is not overclaimed: **a seed replays the draws, not the run.** One draw is
made per *live* service each round, and which services are live depends on restart timing across the
cores, so two runs on one seed part ways at the first round whose timing differs. A seed turns a break
"somewhere in a long run" into one run's named decision stream; it does not make the break repeat on
demand. An aimed run (one service, or a list) draws nothing, and a seed given to one says so.

The point is that the **kernel survives any sequence of random service deaths**. The verdict is
therefore about kernel survival: the report existing at all proves no panic (a panic reboots before it
could print). Whether every service came back is a separate question, and the run does not answer it:
ask afterwards - `status`, `hardware`, and `selfcheck`, which fails if a service it needs did not return.

While it runs, a table redraws in place with each service's kill and flood counts, and the serial log
gets one line per round (`chaos round N: swept ...`) - the panel overwrites itself, so serial is the
history. `q` stops it early, from the serial console in an `all-services` run (it kills the USB keyboard
drivers). The report, on the Raspberry Pi 4 on 2026-10-09:

```
=== chaos max-carnage: report ===
total: 1000 rounds, 7014 kills, 6045 flooded, 1000 mem-pressure, 1000 spawns (999 refused). kernel: alive (this command returned).
```

(A seeded run also prints `seed: <n>` after the header line; this run predates the seed.) This section
used to show a report with per-service "recovered" counts and a verdict line, from before `0cb8985b`
(2026-07-09) and before floods, memory pressure and spawns joined every round; no run prints that now.

`999 refused` is by design: the first `mem-pressure` holds its memory until the run ends, and every later
one is refused as already running.

All output is **ASCII** (the framebuffer font has no em-dash/ellipsis - they render as `?` on the
panel) and `[q] quit` matches the rest of the shell (`observe`, `paginate`).

> **The whole tree regrows from the kernel.** Only the *directly*-restarted services (supervisor by
> the kernel; block-driver/fs by the supervisor) recover on their own death. The rest (`events`,
> `xhci`, `ehci`, …) are not watched individually - but the moment `max-carnage` kills the
> **supervisor** (a valid random target), the kernel respawns it, and the supervisor **re-runs its
> boot sequence**, re-spawning every service it owns *fresh*. So a long carnage run that hits the
> supervisor tends to **fully restore the system**. Hardware-proven on the HP T630: `chaos
> max-carnage 30` killed the supervisor 6× and every service was alive again at the end (`observe`:
> xhci/ehci/events all `Ready`, no kernel panic). A *re-init*, not a resume (§14.2/§25) - a revived
> driver re-enumerates its devices and resumes polling; in-flight state is not preserved.
>
> **Corrected 2026-10-08:** `events`, `xhci` and `ehci` are watched now - every service in the
> supervisor's `MANAGED` set is respawned on its own death (`services/CLAUDE.md`) - so the tree no
> longer waits for a supervisor kill to regrow. The T630 run above is from when it did.

## 6. Capabilities

`chaos` is capability-clean: it uses only `kill` (`SERVICE_CONTROL`), `task_stat` (`INTROSPECT`),
`AcquireSendCap` (`ACQUIRE_ANY`) for the floods, and `spawn` for the memory modes - nothing ambient.
The shell holds these for the modes it runs itself; `max-carnage` runs in the `chaos` SERVICE, whose
own spawn row in the supervisor grants it `SPAWN`, `INTROSPECT`, `SERVICE_CONTROL` and `ACQUIRE_ANY`. It cannot kill the kernel because the kernel is not
a task. (This said the normal commands refuse the supervisor; only `spawn` and `restart` do - `kill
supervisor` is allowed and the kernel respawns it, `11_kill.md` §3.)

## 7. Bounded & loud (§26.6 / §26.7)

- `kill-storm` clamps rounds to `1..=100`; its per-round generation detail lives in fixed stack arrays
  (no heap). `max-carnage` is uncapped - its per-*service* aggregate is constant-size for any count, so
  there is nothing to bound but the loop counter; `q` aborts.
- Each kill and each recovery is logged; a `FAIL` verdict names the round that did not recover in
  budget. Nothing is silent.
- The kernel's supervisor respawn is itself **loud and unbounded** - it logs a running count
  (`respawning (#N)`) and never gives up, so a sustained real fault is visible to an operator rather
  than hidden behind a cap that would eventually reboot.

## 8. Tested

- `osdev test shell` - `chaos kill-storm block-driver 5` (5/5), `chaos kill-storm supervisor 4` (4/4),
  and `chaos max-carnage all-services 5` (a report with its round count, kills and floods, and the
  kernel alive), each asserting the kernel stays alive.
- `osdev test files` - `chaos kill-storm fs` storms + the directory-reacquire regression (a client
  reacquires `fs` by name after its restart).
- Hardware-proven on the HP T630: `chaos kill-storm fs 30` → 30/30; the supervisor stormed dozens of
  times across a session (`observe` showed `RESTARTS 20`+) with **no kernel panic**.
