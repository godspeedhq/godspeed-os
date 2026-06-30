# Milestone - Chaos Engineering (`max-carnage`) ✅

**Status:** ✅ Complete - built, merged, and soaked on the HP T630 at 1M-round scale (0 panics).
**Where:** `services/chaos/` (a userspace program), driven from the shell; `osdev test chaos` (C1-C7) + the shell flood-storm regressions.
**Constitutional anchor:** §22 (Chaos), §26 (Architectural Discipline - bounded behavior, loud failure), §4.4 / §26.10 (kernel is mechanism; chaos is policy).

---

## Scope note: chaos is policy, so it lives in userspace

`max-carnage` is **not** a kernel feature. It is a userspace `chaos` program (`services/chaos/src/main.rs`)
that drives the system through its ordinary syscall + capability surface - exactly what §4.4 / §26.10
require (the kernel provides mechanism; deciding *to* stress the system is policy). The kernel gains
nothing for it. This matters: every bug `max-carnage` found is a real bug reachable by ordinary
authorized services, not an artifact of a privileged test backdoor.

The value of the milestone is not the stressor - it is that **chaos-as-truth-revealer surfaced real,
deep, concurrency-only kernel bugs** that no hand-written test would have hit, and each one was
root-caused and fixed, with the soak becoming the live regression bed.

---

## What it is

- ✅ **`max-carnage` is a per-round SWEEP over every live service**, not a single aimed attack. Each round:
  floods every floodable endpoint, kills the reply-style services (shell, fs), rotates which floodable
  service it kills, applies memory pressure, and runs a spawn-storm - all in one pass, so no service is
  ever "safe" for long. Targeted forms (`max-carnage <service>` / `<all-services>`) also exist.
- ✅ **Everything is bounded and counted** (§26.6). Every attack has a fixed ceiling; all four counters
  report what chaos actually *fired*; a run prints round %, elapsed/remaining time, and degrades
  gracefully rather than wedging. Loud failure over silent corruption (§26.7).
- ✅ **The shell is itself a chaos target.** A crash or a deliberate `kill shell` respawns a *fresh prompt*
  (the in-flight command is lost - a re-init, not a resume) rather than leaving a dead session. Nothing
  escapes restartability - the system's last interactive surface is as killable-and-recoverable as any
  other service.
- ✅ **Flood-endpoint discipline + a hardened verdict.** A registered service that idles without `recv`
  lets a flood fill its 16-deep queue forever, so **every idle path must drain**. Critically, the
  flood-storm *verdict* was hardened: a re-send must **land (`Ok`)** to count as "drained"; a `QueueFull`
  means *still clogged* and now **fails** the test. The earlier verdict counted `QueueFull` as success -
  a weak verifier that had been passing a known-broken target. Fixing the verification first then exposed
  three real clogs (`6d8ac24`). Lesson: a test that passes a broken target is the test's bug, not the
  target's.

---

## Bugs it revealed and fixed

Each was found by *sustained* soaking - invisible to single-shot tests, reproducible only under true
multi-core concurrency over hundreds of thousands of rounds:

- ✅ **Concurrent-TLB-shootdown deadlock (~71K rounds).** Two cores issuing TLB shootdowns at once wedged
  the system. Fixed with **per-core shootdown state + a full per-core boot arena** (`smp/percpu.rs`),
  raising `MAX_CORES` 16 → 256. (Merged via `0adab16`; T630 `selfcheck` 182/0.)
- ✅ **Supervisor-respawn wedge (~506K rounds).** The system froze when the supervisor was respawned: the
  respawn ran inside an **`IF=0` timer ISR**, so the spawning core could not ACK the TLB-shootdown IPIs
  it needed → mutual stall. Fixed by **routing the respawn to the scheduler loop (`IF=1`)** where
  interrupts are enabled (`24ea6fc`, hardware-confirmed).
- ✅ **DMA-after-free KERNEL PF (round 4286, T630).** The kill-path frame reclaim followed a corrupt PTE
  to a ~68 GB frame on an 8 GB box → HHDM fault. **Contained** by guarding the page-table reclaim walk
  against out-of-RAM frames (`b9dbc4c`), then **root-caused as hardware DMA-after-free** and **cured** by
  quiescing the controller's bus-mastering on driver death (`ffe1a0f`) and **confined** by a DMA
  permanent-reserve so a stray DMA can never land in a page-table frame (`731a939`). Full treatment in
  milestone *4 - Kernel Hardening* / the DMA-safety stack.
- ✅ **`alloc_mem` reclaim leak (chaos mem-pressure / S7).** Building `chaos mem-pressure` + `mem-hog`
  surfaced a frame leak: the `ReclaimBuffer` (512-cap) silently dropped frames past its cap. Fixed; folded
  into `0adab16`.
- ✅ **Driver flood-clog gaps (`6d8ac24`).** The hardened verdict exposed three idle paths that did not
  drain (logger, xhci no-controller idle, ehci); each idle loop now polls + drains. The `xhci`
  no-controller drain was pinned separately (`32c438a`).

---

## Commits / evidence

| Commit | What |
|--------|------|
| `6d8ac24` | flood-storm verdict hardened (a re-send must *land*); fixes the 3 clogs it then exposed |
| `32c438a` | pin the flood-endpoint drain; `xhci` no-controller idle now drains |
| `4868504` | glitch-free per-service uptime + idempotent boot-complete (chaos readout) |
| `0adab16` | per-core arena + shootdown deadlock fix; chaos mem-pressure alloc_mem leak fix |
| `24ea6fc` | supervisor respawn routed to an `IF=1` point (the 506K-round storm wedge) |
| `b9dbc4c` | page-table reclaim guard (contains the round-4286 DMA-after-free PF) |
| `ffe1a0f` | BME-quiesce on driver kill - the *cure* for the DMA-after-free |
| `731a939` | DMA permanent-reserve - *confines* any stray DMA to a data frame, never a PTE |

**Hardware:** HP T630 (AMD GX-420GI, 4 real cores) - `max-carnage` soaked to the million-round scale with
**0 kernel panics** and graceful degradation throughout. The soak is the project's live regression bed:
every fix above ships in the image the soak runs, so a regression re-appears as a fresh fault in the next
deep run.

> **Why this milestone matters:** "assume the worst → always recover." `max-carnage` is the executable
> form of that - it proves the restartability + bounded-behavior invariants hold not for one operation but
> for millions, under the only condition (true concurrency, sustained) that exposes the hardest bugs.
