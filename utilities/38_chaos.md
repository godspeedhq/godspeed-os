# Utility: `chaos`

**Utility:** `chaos` — bounded resilience exerciser (kill a service repeatedly, prove it recovers)
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`chaos` answers **does the system actually recover when a service dies, every time, without the
kernel falling over?** It is the executable, on-device form of the restartability invariant (§6.2,
§14.3): it *kills* a recoverable service `rounds` times and, each round, waits for a fresh instance
to come up before killing again. It then prints a per-round report and a single **`verdict: PASS` /
`FAIL`** — and the fact that the command *returns at all* is itself proof the kernel never panicked
(a panic reboots the machine).

This is a **chaos / fault-injection** test in the §22 sense: total failures are covered by the
identity tests; `chaos` lets an operator reproduce the *between* cases live on real hardware.

## 2. Invocation

| Command | Meaning |
|---|---|
| `chaos kill-storm <svc> [rounds]` | Kill one `<svc>` `rounds` times; verify it recovers each round (default 20). |
| `chaos kill-storm <svc> [n] save <path>` | Same, and also write the report to a file at the end. |
| `chaos max-carnage [rounds] [save <path>]` | **The chaos monkey:** kill a *random* live service each round (everything but the shell). |
| `chaos help` / `chaos version` | Self-documentation (`0_conventions.md`). |

`rounds` is clamped to `1..=100` (`CHAOS_MAX_ROUNDS`) — a deliberate cap (§26.6), not a firehose.

### Targets (`<svc>`)

Only **recoverable** services are valid targets — the kernel itself **cannot be killed**, and
killing a non-recoverable thing would just wedge:

| Target | Recovered by |
|---|---|
| `supervisor` | **the kernel** — Path C / Phase 6 (§6.2): the kernel respawns the supervisor on death, *unconditionally and forever* (no bound — a bound would re-introduce the reboot and be a DoS). |
| `block-driver` | the **supervisor** (Phase D, §6.1) — re-inits the controller on respawn. |
| `fs` | the **supervisor** (Phase D) — re-mounts to a consistent state via its crash-consistency journal (`docs/persistence.md` §6.8). |

> The **only unkillable component is the kernel** (`{kernel}`). `chaos` can storm anything above it;
> there is nothing it can do to bring the kernel down — "do anything except shotgun the kernel."

## 3. Output

```
gsh> chaos kill-storm supervisor 4
chaos kill-storm supervisor: 4 rounds — kill, then wait for the supervisor to respawn it...
=== chaos kill-storm supervisor: report ===
target: supervisor (kernel-respawned); rounds: 4
round   1: killed gen 3 -> recovered gen 4
round   2: killed gen 4 -> recovered gen 5
round   3: killed gen 5 -> recovered gen 6
round   4: killed gen 6 -> recovered gen 7
recovered: 4/4; kernel: alive (no panic — this command returned)
verdict: PASS
```

The companion kernel log shows the recovery path each round, e.g. for the supervisor:

```
kernel: supervisor died — respawning (#N) (Path C / Phase 6)
supervisor: adopted running block-driver (slot 5)   ← reconciliation: adopt the live services, don't duplicate
supervisor: ready
```

## 4. How recovery is detected

Each round reads the target's **task generation** (a restart bumps it, §7.5 — the same number
`observe` shows in its `RESTARTS` column), kills the target, then waits for a *new* generation to
appear. The wait is bounded by **real wall-clock time** (the RTC, `CHAOS_RECOVER_SECS = 8 s`), **not**
a yield count: a yield count is not portable — it was generous in QEMU but too short on real hardware
for the heavier, kernel-driven *supervisor* respawn, which made `chaos kill-storm supervisor`
under-count recoveries even though the supervisor genuinely came back every time. The loop breaks the
instant a new generation appears, so fast targets (`fs`, `block-driver`) stay fast; only a genuinely
slow recovery pays the larger budget. Before each kill `chaos` also waits for the target to be *alive*
(it may still be mid-respawn from the previous round), so no round is wasted killing a not-yet-present
task.

## 5. The report avoids a catch-22

`chaos kill-storm fs` kills the very service that stores files — so the report is **recorded in
memory** during the storm (a bounded buffer, never touching `fs`) and only **printed to the console**
at the end (fs-independent, captured by the serial log). An optional `save <path>` then materialises
it to a file *after* the target has recovered, with a bounded retry (if `fs` was the target it may
still be finishing its re-mount). If the save never lands in budget, the console report stands.

## 5b. `max-carnage` — the chaos monkey

`chaos max-carnage [rounds]` reads the **live task set** (exactly what `observe now` shows) and, each
round, kills **one at random** — everything is fair game **except the shell** (killing it would kill
this command) and the **kernel** (not a task, cannot be killed). Recoverable victims
(supervisor/block-driver/fs) are confirmed back up; the rest stay dead, which is *expected* (nothing
auto-restarts them). The victim is chosen with a tiny `xorshift64` PRNG seeded from the **TSC** (so the
sequence differs every run).

The point is **not** per-service recovery — it is that the **kernel survives any sequence of random
service deaths**. The verdict is therefore about kernel survival: the report existing at all proves no
panic (a panic reboots before it could print). A recoverable victim that did not come back in budget is
flagged per-round, but does not fail the verdict — it may be the §6.2 supervisor-downtime edge case (a
service that died while the supervisor was itself mid-respawn), a known service-level limitation, not a
kernel failure.

```
gsh> chaos max-carnage 8
chaos max-carnage: 8 rounds — kill a RANDOM live service each round (everything but the shell)...
=== chaos max-carnage: report ===
rounds: 8; victims killed: 8
round   1: killed fs             -> recovered
round   2: killed ehci           -> killed (revives on the next supervisor respawn)
round   3: killed supervisor     -> recovered
round   4: killed block-driver   -> recovered
round   5: killed logger         -> killed (revives on the next supervisor respawn)
round   6: killed fs             -> recovered
round   7: killed supervisor     -> recovered
round   8: killed xhci           -> killed (revives on the next supervisor respawn)
directly-restarted victims recovered: 5/5
(services not directly restarted — e.g. logger/xhci/ehci — come back on the next
 supervisor respawn, which re-runs the boot sequence; run `observe now` for the live set)
kernel: SURVIVED 8 random kills (no panic — this command returned)
verdict: PASS (kernel survived)
```

> **The whole tree regrows from the kernel.** Only the *directly*-restarted services (supervisor by
> the kernel; block-driver/fs by the supervisor) recover on their own death. The rest (`logger`,
> `xhci`, `ehci`, …) are not watched individually — but the moment `max-carnage` kills the
> **supervisor** (a valid random target), the kernel respawns it, and the supervisor **re-runs its
> boot sequence**, re-spawning every service it owns *fresh*. So a long carnage run that hits the
> supervisor tends to **fully restore the system**. Hardware-proven on the HP T630: `chaos
> max-carnage 30` killed the supervisor 6× and every service was alive again at the end (`observe`:
> xhci/ehci/logger all `Ready`, no kernel panic). A *re-init*, not a resume (§14.2/§25) — a revived
> driver re-enumerates its devices and resumes polling; in-flight state is not preserved.

## 6. Capabilities

`chaos` is capability-clean: it uses only `kill` (`SERVICE_CONTROL`, held by the shell) and
`task_stat` (`INTROSPECT`) — both already held, nothing ambient. It cannot kill the kernel because the
kernel is not a task; it cannot kill the supervisor *casually* through the normal `kill`/`restart`
commands (those refuse `CORE_SERVICES` at the command layer) — deliberate supervisor chaos is
explicit, through `chaos kill-storm supervisor`.

## 7. Bounded & loud (§26.6 / §26.7)

- Rounds clamped to `1..=100`; per-round results live in fixed stack arrays (no heap).
- Each kill and each recovery is logged; a `FAIL` verdict names the round that did not recover in
  budget. Nothing is silent.
- The kernel's supervisor respawn is itself **loud and unbounded** — it logs a running count
  (`respawning (#N)`) and never gives up, so a sustained real fault is visible to an operator rather
  than hidden behind a cap that would eventually reboot.

## 8. Tested

- `osdev test shell` — `chaos kill-storm block-driver 5` (5/5), `chaos kill-storm supervisor 4` (4/4),
  and `chaos max-carnage 8` (kernel survived the random carnage), each asserting the kernel stays alive.
- `osdev test files` — `chaos kill-storm fs` storms + the directory-reacquire regression (a client
  reacquires `fs` by name after its restart).
- Hardware-proven on the HP T630: `chaos kill-storm fs 30` → 30/30; the supervisor stormed dozens of
  times across a session (`observe` showed `RESTARTS 20`+) with **no kernel panic**.
