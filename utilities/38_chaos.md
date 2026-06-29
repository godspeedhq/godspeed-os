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
| `chaos max-carnage [rounds]` | **The chaos monkey:** each round, kill **OR flood** a *random* live service (everything but the shell), rolling a creative mix (kill / flood / flood-then-kill / kill-then-flood). Runs exactly the count you type; a live progress line ticks `%`/ETA; `q` aborts. |
| `chaos help` / `chaos version` | Self-documentation (`0_conventions.md`). |

`kill-storm` clamps `rounds` to `1..=100` (`CHAOS_MAX_ROUNDS`, §26.6) - it stores per-round generation
detail in fixed stack arrays. **`max-carnage` has no round cap**: its report is a constant-size
per-*service* aggregate, so the round count is a loop counter, not a resource (same reasoning as the
unbounded supervisor respawn, §6.2). It runs exactly what you type (bounded only by `u32`); `q` aborts.
It also takes no `save` - it destroys `fs`, so a save would fight the storm; its report is
console-only (§5b).

### Targets (`<svc>`)

Only **recoverable** services are valid targets - the kernel itself **cannot be killed**, and
killing a non-recoverable thing would just wedge:

| Target | Recovered by |
|---|---|
| `supervisor` | **the kernel** - Path C / Phase 6 (§6.2): the kernel respawns the supervisor on death, *unconditionally and forever* (no bound - a bound would re-introduce the reboot and be a DoS). |
| `block-driver` | the **supervisor** (Phase D, §6.1) - re-inits the controller on respawn. |
| `fs` | the **supervisor** (Phase D) - re-mounts to a consistent state via its crash-consistency journal (`docs/persistence.md` §6.8). |
| `shell` | the **supervisor** - a fresh prompt (the in-flight command is lost - a re-init, not a resume, §14.2). |
| `xhci` / `ehci` / `logger` | the **supervisor** - drivers re-grant MMIO/DMA/IRQ + re-enumerate; logger re-drains the ring buffer. |

The kernel notifies the supervisor on the death of this **directly-restartable set** so it respawns
them immediately. And on death a service's name is **unregistered from the kernel directory** (§14.2),
so even if its death-notification is lost (e.g. the supervisor was itself mid-respawn during a storm),
the supervisor's reconcile finds the name *missing* and respawns it - services self-heal.

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
  name** (`AcquireSendCap`) - so `fs`, `logger`, `block-driver`, even `supervisor` are floodable.
- **Payload:** a minimal benign message the target drains and drops - no writes, no side effects. The
  test stresses the *queue*, not the disk.
- **Verdict:** `PASS` = the service survived every flood (no `EndpointDead`) and still accepts messages.
  A flood that *crashes* a service is caught and reported - a finding, not a hang; a restartable one
  respawns and the storm continues.

> **Aside (hardening note).** `AcquireSendCap` is currently **ungated** - any task can mint a SEND cap
> to any named service. That is what makes broad flooding possible, and it is also an ambient-authority
> gap (§3.1) the naming design meant to close (Path C / Phase 4). Flooding stays compatible with a
> future gate (the shell would hold the recovery cap). On the hardening list with the reboot gate.

## 5b. `max-carnage` - the chaos monkey

`chaos max-carnage [rounds]` reads the **live task set** (exactly what `observe now` shows) and, each
round, picks **one at random** and rolls a **creative action mix** - kill, flood, flood-then-kill, or
kill-then-flood (the §8.6 queue-drained-on-death and EndpointDead-back-pressure cases) - everything is
fair game **except the shell** (killing it would kill
this very command, which runs *inside* the shell) and the **kernel** (not a task, cannot be killed).
The shell is itself restartable - a direct `kill shell` respawns a fresh prompt - but `max-carnage`
can't be the one to kill it, because a fresh shell wouldn't resume the in-flight carnage loop.
Directly-restarted victims (the whole named set - supervisor, block-driver, fs, shell, xhci, ehci,
logger) are confirmed back up each round; only demo services like `ping`/`pong` (full build) revive on
the next supervisor respawn (see below). The victim is chosen with a tiny `xorshift64` PRNG seeded
from the **TSC** (so the sequence differs every run).

The point is **not** per-service recovery - it is that the **kernel survives any sequence of random
service deaths**. The verdict is therefore about kernel survival: the report existing at all proves no
panic (a panic reboots before it could print). A recoverable victim that did not come back in budget is
reported per-service (`recovered < killed`), but does not fail the verdict - it may be the §6.2
supervisor-downtime edge case (a service that died while the supervisor was itself mid-respawn), a
known service-level limitation, not a kernel failure.

While it runs, a single **self-updating heartbeat line** (so the screen isn't frozen for a long run)
shows progress, a running ETA, and the abort hint - `q` stops it early. The final report is a
**per-service aggregate** (constant size for any round count) plus a built-in `observe now` survivor
line:

```
gsh> chaos max-carnage 1000000
chaos max-carnage: 1000000 rounds - kill a RANDOM live service each round (all but the shell). Press q to quit.
max-carnage: 250000 / 1000000 (25%) - 250000 kills - ETA 9m00s - kernel alive - q to quit   ← live, refreshes in place
=== chaos max-carnage: report ===
rounds: 1000000; victims killed: 1000000
  supervisor     killed 166k, recovered 166k
  block-driver   killed 142k, recovered 142k
  fs             killed 143k, recovered 143k
  xhci           killed 137k, recovered 137k
  ehci           killed 139k, recovered 139k
  logger         killed 133k, recovered 133k
directly-restarted recoveries confirmed: 1000000/1000000
kernel: SURVIVED 1000000 random kills (no panic - this command returned)
survivors (live now): supervisor logger block-driver fs shell xhci ehci  (7 live)
verdict: PASS (kernel survived)
```

All output is **ASCII** (the framebuffer font has no em-dash/ellipsis - they render as `?` on the
panel) and `q to quit` matches the rest of the shell (`observe`, the help pager).

> **The whole tree regrows from the kernel.** Only the *directly*-restarted services (supervisor by
> the kernel; block-driver/fs by the supervisor) recover on their own death. The rest (`logger`,
> `xhci`, `ehci`, …) are not watched individually - but the moment `max-carnage` kills the
> **supervisor** (a valid random target), the kernel respawns it, and the supervisor **re-runs its
> boot sequence**, re-spawning every service it owns *fresh*. So a long carnage run that hits the
> supervisor tends to **fully restore the system**. Hardware-proven on the HP T630: `chaos
> max-carnage 30` killed the supervisor 6× and every service was alive again at the end (`observe`:
> xhci/ehci/logger all `Ready`, no kernel panic). A *re-init*, not a resume (§14.2/§25) - a revived
> driver re-enumerates its devices and resumes polling; in-flight state is not preserved.

## 6. Capabilities

`chaos` is capability-clean: it uses only `kill` (`SERVICE_CONTROL`, held by the shell) and
`task_stat` (`INTROSPECT`) - both already held, nothing ambient. It cannot kill the kernel because the
kernel is not a task; it cannot kill the supervisor *casually* through the normal `kill`/`restart`
commands (those refuse `CORE_SERVICES` at the command layer) - deliberate supervisor chaos is
explicit, through `chaos kill-storm supervisor`.

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
  and `chaos max-carnage 8` (kernel survived the random carnage), each asserting the kernel stays alive.
- `osdev test files` - `chaos kill-storm fs` storms + the directory-reacquire regression (a client
  reacquires `fs` by name after its restart).
- Hardware-proven on the HP T630: `chaos kill-storm fs 30` → 30/30; the supervisor stormed dozens of
  times across a session (`observe` showed `RESTARTS 20`+) with **no kernel panic**.
