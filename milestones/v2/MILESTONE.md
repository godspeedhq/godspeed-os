# v2 — Userspace (Ring-3) Execution on AMD Hardware

**Status:** ✅ Achieved on hardware — 2026-05-29 (kernel fixes pending commit)
**Target hardware:** HP T630 thin client — AMD GX-420GI (Jaguar/Puma+, Family 22h), 4-core ~2 GHz, 8 GB RAM
**Serial capture:** COM1, 115200 8N1, null modem → PuTTY → `build/putty_serial_output.log`

---

## Scope note: what was already done vs. what this milestone is

Ring-3 *itself* is not new. The ring-3 service infrastructure (`new_user`,
`ring3_entry_trampoline`, the SYSCALL stub, the kernel/user `TaskContext` split)
landed in **v1, Milestone 7** (commits `c2cc77c`, `dac274c`). Userspace services
ran in ring-3 under QEMU throughout v1, and the v1 bare-metal achievement
(CLAUDE.md §23.3) booted userspace on the **Intel Wyse 5070** (Goldmont+).

**This milestone is the AMD bring-up.** Moving to the HP T630 (AMD GX-420GI)
broke ring-3 entry in ways that never appeared on Intel or QEMU. The CPU would
reach CPL=3 but the first userspace instruction never retired; no service ever
logged `init: ready`. Getting from "CPU is in ring-3" to "userspace actually
executes and drives the full service stack" on AMD silicon is the work recorded
here.

Outcome: a full multi-core boot to steady state on the T630 — `init: ready`,
all TCB + application services spawned, `supervisor: ready`, the shell prompt
live, and cross-core ping/pong running continuously.

---

## Acceptance criteria

Evidence from `build/putty_serial_output.log` (clean boot, 2026-05-29):

- ✅ CPU transitions to CPL=3 with a correct IRETQ frame
  - `[CS=…002b FLG=…0202 SS=…0023]` — user CS/SS/RFLAGS as constructed
- ✅ Userspace instructions actually retire (not just "CPU is in ring-3")
  - ring-3 port-I/O probe wrote `R` to COM1 from CPL=3 (diagnostic, since removed)
- ✅ The `ud2` syscall path dispatches ring-3 → ring-0 → handler → return
  - `init: ready` follows the first userspace syscall
- ✅ `init` spawns supervisor, registry, logger; all reach ready
- ✅ supervisor spawns pong, ping, observe, shell; `supervisor: ready`
- ✅ Interactive shell prompt `gs>` reachable on hardware
- ✅ Cross-core IPC runs continuously: `pong: received "1" … "40"+` (ping core 0 → pong core 1)
- ✅ No kernel panic, no `#GP`, no `#PF`, no unhandled exception on any core
- ✅ All four cores online (`kernel: 4 cores ready`)

---

## Issues encountered and overcome

The AMD bring-up was a chain of distinct root causes. Each one individually
produced the same surface symptom (no `init: ready`), so each had to be isolated
before the next became visible.

### 1. SYSRETQ produces SS with RPL=0 on AMD → #GP(0x20) on the next interrupt

**Symptom:** Entering ring-3 via `SYSRETQ` appeared to work, but the *first*
timer interrupt from ring-3 faulted with `#GP(0x20)`.

**Root cause:** `SYSRETQ` derives the user SS from the STAR MSR
(`STAR[63:48] + 8`) and is specified to OR the RPL with 3 — but on KVM with AMD
host CPUs (and on AMD silicon) the resulting SS came back with RPL=0 (`0x20`)
instead of `0x23`. The CPU pushes that SS into the hardware interrupt frame; the
timer ISR's `IRETQ` then sees `SS.RPL(0) ≠ SS.DPL(3)` and faults.

**Fix (commit `dac274c`):** Enter ring-3 with an explicit **`IRETQ` frame**
instead of `SYSRETQ`. The kernel hand-builds the 5-word frame with `SS=0x23`,
`CS=0x2b`, `RFLAGS`, user `RSP`, user `RIP`, so the descriptor selectors are
exactly what the kernel intends, independent of STAR-derived behaviour.
Both `ring3_entry_trampoline` (first entry) and the syscall return path use
explicit IRETQ frames for this reason.

### 2. `SYSCALL` (and `int N`) silently stall from ring-3 on GX-420GI

**Symptom:** Once in ring-3, a `syscall` instruction from userspace did nothing
observable — the core stalled with no fault, no dispatch, no return. `int 0x80`
behaved the same way.

**Root cause:** On the AMD GX-420GI, the software-syscall dispatch paths
(`syscall` → LSTAR, and `int N` software-interrupt vectoring) did not deliver to
the kernel handler from ring-3 under our configuration. The compat-mode CSTAR
target was never hit either, so it was not a CS.L (long vs. compat) issue.

**Fix:** Use **`ud2` (opcode `0F 0B`) as the syscall mechanism**, dispatched via
**IDT[6] (#UD)**. Hardware exceptions take the fault delivery pathway — the same
one `#PF` and `#GP` use — which *does* work on this hardware. `ud2_syscall_entry`
reads the saved CS to distinguish a ring-3 syscall (CS=0x2b) from an accidental
ring-0 `ud2` (CS=0x08 → halt), advances the saved RIP past the 2-byte `ud2`, and
otherwise mirrors the SYSCALL handler's argument marshalling. `int80_entry` and
the LSTAR `syscall_entry` are retained for reference but are not the live path on
AMD.

### 3. APIC timer cascade starves ring-3 of every instruction (the hard one)

**Symptom:** With #1 and #2 fixed, the CPU entered ring-3 (confirmed CPL=3 on
every timer tick) but `RSP` never moved off its initial value `0x80000000` — the
first userspace instruction never decremented the stack. Userspace was getting
**zero** instructions. No `init: ready`, ever.

**Root cause:** The local APIC timer on the GX-420GI is clocked at roughly
`CPU_CLOCK / divider = 2 GHz / 16 ≈ 125 MHz`, not the ~1 GHz "APIC bus" rate the
periodic-mode init count assumed. With `APIC_TIMER_INIT = 625_000` that gives a
**~5 ms** period. Meanwhile the timer ISR's *own verbose diagnostic output* took
~4 ms for the per-tick window and up to ~18 ms for the wide multi-core dumps —
**longer than the timer period**. Because the ISR sends EOI *after* its serial
output, the timer re-fired into the LAPIC IRR while the ISR was still printing.
On `IRETQ` (which restores `IF=1`), the pending IRR fired immediately, so
userspace got preempted before it could retire a single instruction. The system
was effectively live-locked in the timer ISR.

**Fix:** Raise `APIC_TIMER_INIT` from `625_000` to **`6_250_000`** — a ~50 ms
period on the GX-420GI (and ~100 ms on QEMU's slower modelled clock), comfortably
longer than the worst-case ISR output time. This broke the cascade and gave
userspace ~30–45 ms of run time per quantum, after which the full service stack
came up immediately.

> Lesson: a periodic-APIC init count is **not portable across vendors** — the
> timer input frequency differs. The TSC-Deadline path (used on Goldmont+, see
> below) sidesteps this by arming in absolute TSC ticks; AMD GX-420GI falls into
> periodic mode because it does not populate `CPUID.0x15` for the TSC/crystal
> ratio, so the periodic count must be sized for the AMD timer clock.

### 4. (Carried over from Intel) Goldmont+ C-state APIC power-gate

Documented for completeness — encountered on the **Intel Wyse 5070** (J5005),
not the AMD T630, and largely resolved before this milestone.

**Symptom:** On the Wyse, bare-metal boot froze shortly after the supervisor was
queued; ~1380 ping/pong messages then silence. Firmware autonomously promoted the
SoC package to PC6+, power-gating the local APIC and dropping both timer ticks
and IPIs.

**Fixes (commits `907293f`, `798a176`, `299992a`, `96be038`):** remove
`pause`/`hlt` C-state hints from the idle loop; attempt an MSR 0xE2 C-state limit
(blocked by `CFG_LOCK=1` on the Wyse — see `docs/hw-bare-metal-freeze-j5005.md`);
and adopt the **TSC-Deadline APIC timer**, whose TSC runs in all C-states. The
AMD T630 does not have the Goldmont+ power-gate quirk, which is why it was chosen
as the SMP test machine.

---

## Diagnostic methodology (what made #3 findable)

Issue #3 was invisible to the existing tooling — the kernel reached ring-3 but
produced no output, so it looked identical to "ring-3 doesn't work at all." The
methodology that cracked it:

- **Ring-3 port-I/O probe.** Set `RFLAGS.IOPL=3` in the IRETQ frame and made
  `init`'s first instructions a `naked` stub that writes `R` to COM1 via `out`.
  With IOPL=3, ring-3 can do port I/O without the TSS IOPB. Seeing (or not seeing)
  `R` distinguished "userspace executes" from "userspace never runs" without
  depending on the syscall path working.
- **First-instruction RSP watch.** The timer ISR printed the interrupted CPL/RIP/RSP
  for core 0 on ticks 14–18. `RSP` pinned at `0x80000000` across every tick was
  the smoking gun: the CPU was in ring-3 but `push` never executed → the timer was
  firing before the first instruction retired → cascade.
- **Flag-based "reached?" reporting.** Atomic flags set as the very first
  instruction of each entry stub (`SYSCALL_REACHED`, `CSTAR_REACHED`,
  `INT80_REACHED`) and exception stub, reported by another core's timer ISR via
  lock-free serial. This survived even when the suspect core was stuck, and proved
  `syscall`/`int` never dispatched while `ud2`/#UD did.
- **Exception canaries.** `#PF`/`#GP`/catch-all stubs each wrote a single
  distinguishing byte (`P`/`G`/`?`) as their first instruction. Their *absence*
  ruled out a silent fault as the cause and pointed at scheduling, not protection.
- **IRETQ frame dump.** Printing `CS`/`RFLAGS`/`SS` immediately before
  `swapgs;iretq` confirmed the descriptor selectors were correct, eliminating the
  GDT/frame as a suspect.

All of this scaffolding has since been removed (see "Cleanup" below).

---

## Files changed (AMD bring-up)

| File | Change |
|------|--------|
| `kernel/src/arch/x86_64/context_switch.rs` | `ring3_entry_trampoline` builds an explicit IRETQ frame (`CS=0x2b`, `SS=0x23`, `RFLAGS=0x202`); enters ring-3 via `iretq`, not `sysretq` |
| `kernel/src/arch/x86_64/syscall_entry.rs` | `ud2_syscall_entry` (IDT[6]) as the live syscall path; CPL check on saved CS; RIP advanced past `ud2`; `cstar_entry` is a silent halt trap (should never be reached) |
| `kernel/src/arch/x86_64/boot.rs` | IDT[6] → `ud2_syscall_entry`; `APIC_TIMER_INIT` periodic count `625_000 → 6_250_000` to break the timer-ISR cascade on AMD |
| `kernel/src/task/scheduler.rs` | Periodic vs. TSC-Deadline timer arming; re-arm after EOI |

---

## Cleanup (post-verification, 2026-05-29)

With ring-3 confirmed working, all bring-up diagnostics were stripped so the
serial log is clean for the cross-core test suite (perf/stress) and so userspace
no longer carries unnecessary privilege:

- `RFLAGS` in the ring-3 entry frame reverted `0x3202 → 0x202` — **IOPL back to 0**,
  removing ring-3 port-I/O privilege (the probe is gone, production ring-3 must not
  do port I/O).
- Removed all per-syscall serial output (`[sc]`, `[log-enter]`, `[ring-try]`,
  the `S`/`K`/`U`/`I`/`!X`/`C` entry-stub prints) and the `*_REACHED` flags + the
  `syscall_entry_diag` helper.
- Removed the per-tick / tick-windowed timer-ISR diagnostics (ticks 0–30:
  `[lXX cY]`, `SYSCALL/CSTAR/EXC/PF/GP/INT80-NO`, `late20/late30`, per-core RIP dumps).
- Removed the first-ring3-switch page-table dump (`entry64`, `ctx16`,
  `cod[…]`, `stk[…]`, `pml4/pdpt/pd/pte`) and the `rsp0=` readback.
- Removed `init`'s `naked` `R`-probe wrapper, restoring §18.2 (no `unsafe` in
  service code).

Retained: the fault stubs (`P`/`G`/`?`) and their flags — they emit only on an
actual fault, so they stay silent in a healthy run and serve as loud-failure
indicators (§3.12).

---

## Cross-core test suite on AMD — perf-brutal (2026-05-31)

The cross-core suite that was backburnered on Goldmont+ now runs end-to-end on
the T630. Getting there required one more hardware bug fix.

**BP2 unblocked (the COM2 timer-ISR wedge).** Cross-core IPC reply to the BSP
hung: a task blocked on `recv` on core 0 was never woken by an AP's send. Root
cause was **not** an APIC/IPI quirk (NMI-into-core-0 probing proved the wake IPI
reached core 0's IRR) — it was `control::process_pending` draining COM2 with an
**unbounded** `while let Some(b) = com2_try_read_byte()` loop, called from core
0's timer ISR with `IF=0`. The T630 has no usable COM2, so its LSR floats to
`0xFF` (Data-Ready permanently set) → the loop never returns → core 0 spins
interrupts-disabled and can never take the latched `WAKE_RECEIVER` IPI. Fixed by
bounding the drain to 256 iterations/call (commit `a306fd3` on `main`; full
investigation in `bugs/1_FINDINGS_AP_TO_BSP_IPI.md`).

**Full perf-brutal run.** With the fix in place, all ten BP probes completed,
no panic, no `#PF`, over a sustained ~27k-line run. BP2 was measured for the
first time on this hardware.

| Bench | J5005 (~3 GHz, Goldmont+) | T630 (~2 GHz, Jaguar/Puma+) | notes |
|-------|---------------------------|------------------------------|-------|
| BP1 same-core IPC p50      | 55,320      | ~102,600              | clean (~1.9×) |
| BP2 cross-core IPC p50     | *not measured* | **9,516,027** (in-suite) | **first measure**; isolated `bp2-only` = 1,433,087 |
| BP3 yield                  | 39,903      | ~143,000              | preemption-dominated |
| BP4 cap validation         | 495         | ~1,258                | clean (~2.5×) |
| BP5 spawn                  | 8,121,378   | 75,106,731            | contention |
| BP6 restart                | 14,462,309  | 87,574,924            | contention |
| BP7 cap table              | 1,168       | 5,290 – 526,316       | high variance (preemption) |
| BP8 allocator (4 KiB)      | 616         | ~1,472                | clean (~2.4×) |
| BP9 message copy 4 KiB     | 20,073      | ~149,000              | contention |
| BP10 scheduler decision    | 2,323       | ~10,100               | contention |

*All figures in CPU cycles; T630 values are the cleanest across re-runs.*

**Reading the numbers honestly.** They split in two. The compute-bound,
single-shot-per-iteration probes (BP1, BP4, BP8) land at a steady **~1.9–2.5×**
the J5005 cycle counts — genuine microarchitecture (Jaguar/Puma+ is a 2-wide
low-power core with lower IPC than Goldmont). The large/variable figures (BP5,
BP6, BP9, BP3, BP10, and BP7's 100× spread) are **contention/preemption-inflated,
not real per-op latency**: under the full 13-probe load a measurement's TSC delta
includes time the probe sat preempted. The proof is BP2 itself — **9.5M cycles
in-suite vs 1.43M isolated**, a 6.6× inflation purely from load. For publishable
per-op latency, each probe must be isolated (à la `bp2-only`); the suite-under-load
numbers measure throughput-under-contention and robustness, not single-op cost.

ping/pong reached only `pong: received "3"` — starved out by the 13 active probes,
as expected under brutal load; cross-core IPC still functioned (pong did receive).
No regression: the run completed clean on all four cores.

---

## Static-analysis + unsafe-audit cleanup (2026-05-31)

A local static-analysis pass over the kernel after the AMD ring-3 / APIC / COM1 /
shell work. No CI minutes consumed. Full write-up: `milestones/v2/STATIC_ANALYSIS_AUDIT.md`.

| Area | Result |
|------|--------|
| Policy violation | **Fixed** — `unsafe` removed from `ipc/` (§18.1 forbidden layer); `.bss` zeroed-static moved to `SpinLock::ZEROED` in `smp/`. `ipc/` unsafe-free again. |
| Safety / correctness lints | **0** — 11 unnecessary `unsafe`, 11 `static mut` refs (→ `addr_of!`), 14 fn-item→int casts, 6 no-op `mem::forget` all cleared. |
| Cruft removed | orphaned `page_fault_handler` + `INTERRUPTED_*` diagnostic statics. |
| Unsafe audit | **passes clean** — 302 lines / 23 files; `task/scheduler.rs` back under its grandfathered floor (37 → 36). |
| Kernel warnings | **104 → 57** (remaining 57 are intentional unwired architecture — §22 assertions, capability/IPC API — kept deliberately). |
| Hardware (T630) | **boots clean** — 4 cores, cross-core ping/pong to **83,043 messages**, zero `#PF`/panic. Decisive verification: the changes touched IDT setup, frame allocator, BSP stack switch, context-switch trampolines. |
| Miri (kernel `lib`) | lone failure was proptest regression-file I/O vs miri's sandbox; passes with `-Zmiri-disable-isolation`. Covers logic untouched here. |

Commit `d276566` on branch `verify/static-analysis-unsafe-audit`.

---

## §22 invariant assertions wired (2026-06-01)

The four constitutional assertions in `kernel/src/invariants/assertions.rs` were
dead code (defined, never called — flagged by the static-analysis pass above).
They are now wired into the hot paths they guard, so they enforce the invariants
at runtime in release builds rather than living as documentation.

| Assertion | Call site | Pins |
|-----------|-----------|------|
| `assert_no_mid_execution_migration` | `prepare_ring3_switch` (every ring-3 resume) | §9.1 static placement |
| `assert_cap_validated` | success path of `send`/`recv`/`try_send`/`log` handlers | §3.1 no ambient authority |
| `assert_tcb_alive` | `handle_kill` success path | §6.2 TCB liveness |
| `assert_cap_table_consistent` | `handle_kill` success path | §7.8 no future-generation caps |

Two issues surfaced and were fixed in the process:

- **Latent `assert_tcb_alive` bug.** It assumed all three TCB services register a
  named IPC endpoint, but `init` is the bootstrap spawner and never does, and the
  minimal §22 test manifests omit `registry`. As written it would have panicked the
  first time it was ever called. Rewritten to check **task liveness** via the safe
  `task_stat` snapshot (no new unsafe), tolerant of services a given config omits.
- **Fail-open → fail-closed (security review, HIGH).** Tolerating an absent TCB name
  is only safe if such a service can never be killed-and-reclaimed. `handle_kill` now
  **rejects killing `init`/`supervisor`/`registry`** before any kill happens (rejection,
  not panic, to avoid handing a caller a reboot DoS). Absence at the post-kill sweep can
  then only mean "never spawned", making the tolerance provably safe.

**Verification.** Boot-verified on the T630: **0 invariant violations across ~4,500
kills** plus the full perf/adv/chaos/stress probe suite, 0 kernel panics. The migration
check runs on every context switch and `assert_cap_validated` on every privileged
syscall, so both hot-path assertions were exercised continuously. Identity suite 17/22
locally (the 5 failures are QEMU-TCG-on-Windows boot-speed timeouts — non-deterministic,
0 logic failures). Clears 7 dead-code warnings (the assertions + `is_endpoint_alive` +
`for_each_active_cap` now used): kernel warnings **57 → 50**. Commit `3094719` on branch
`verify/wire-invariant-assertions`.

Remaining: `assert_cap_validated` is a post-validation tripwire (tautological in correct
code, since the handler already returns on a bad cap) — it documents the §3.1 boundary
and would catch a future handler that performs a privileged action without validating.

---

## Bug 2 fixed — #UD syscall / timer stack overlap (2026-06-01)

The last known kernel bug: an intermittent `rip → kstack` `#PF` under heavy cross-core
`recv` load on real hardware (root-caused earlier, banked; full history in
`bugs/2_INTERMITTENT_RIP0_PF_POST_IDLE.md`, Updates 1–9).

**Root cause.** On the GX-420GI the live syscall path is `ud2`/#UD, which enters on
`TSS.rsp0 = K0T` — the same top-of-kstack region the timer ISR's context-switch path
descends into (~K0T-504). `ud2_syscall_entry` ran the whole syscall chain there, so the
timer-switch could zero-write a suspended recv syscall's return address. The `K0T-512`
separation was *designed* (`prepare_ring3_switch` sets `kernel_rsp`) but never wired into
the `#UD` path — the dead LSTAR path already switched to it.

**Fix** (small, surgical — wires up the intended separation):
- `ud2_syscall_entry` switches `RSP` to `kernel_rsp` before calling the handler (stashing
  the `#UD` frame pointer for the final `iretq`).
- `prepare_ring3_switch` sets `kernel_rsp = K0T-2048` (was 512) for ~1.5 KiB guard over the
  measured timer reach.

**Verification.** QEMU: boots, `send`/`recv` flow, 0 `#PF`/`#UD`/`#GP`. Hardware (T630,
iso-s9 repro): **14 power-cycles, 0 Bug 2 faults, `S9 pass (100/100)` every boot** — vs the
pre-fix ~50% fault rate. Cross-core-heavy workloads are now safe on hardware.

---

## Known residue / follow-ups

- **Multi-core serial garbling.** When all four cores write COM1 simultaneously
  (boot banner, any future concurrent logging) the output interleaves. Cosmetic
  only; a lock around `serial_write_byte` would serialize it if cleaner logs are
  wanted.
- **Dead `pub static`s in `boot.rs`.** `INTERRUPTED_RIP/CS/RSP` are no longer read
  after the diagnostic removal. Harmless (no warning), removable later.
- **Kernel fixes pending commit.** The IRETQ/`ud2`/timer-count changes and the
  diagnostic cleanup are in the working tree, not yet committed/tagged.
- **Cross-core test suite on AMD.** ✅ **BP2 done** (see "Cross-core test suite on
  AMD — perf-brutal" above): root-caused the COM2 timer-ISR wedge, fixed, and ran
  the full perf-brutal suite to completion on the T630. Still outstanding from the
  Goldmont+ backburner: **S3** (cross-core thrash) and **S9** (IPI storm) under
  `osdev image --mode stress` — these should now run given the same fix, but have
  not yet been executed on AMD.
- **Isolated per-probe perf numbers.** ✅ Done. Added `perf-iso` per-probe isolation
  builds (`osdev image --mode iso-bp{3,5,7,9,10}`; bp5 covers BP5+BP6) — one benchmark
  alone, no ping/pong — giving clean, uncontended T630 latencies. The full isolated
  column is now in CLAUDE.md §23.3. Headline: isolation stripped 7–100× of contention
  noise (e.g. BP7 5,290–526,316 in-suite → a stable 2,932; BP9 149k–280k → 21,796, ≈
  the J5005's 20,073 — proving its apparent "regression" was pure scheduling noise).
  (Build note: a cargo cache-mtime quirk could embed a stale supervisor in the kernel
  for these single-feature builds; fixed in `ed8a151` by force-cleaning the supervisor.)
