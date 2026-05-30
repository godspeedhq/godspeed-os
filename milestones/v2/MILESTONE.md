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

## Known residue / follow-ups

- **Multi-core serial garbling.** When all four cores write COM1 simultaneously
  (boot banner, any future concurrent logging) the output interleaves. Cosmetic
  only; a lock around `serial_write_byte` would serialize it if cleaner logs are
  wanted.
- **Dead `pub static`s in `boot.rs`.** `INTERRUPTED_RIP/CS/RSP` are no longer read
  after the diagnostic removal. Harmless (no warning), removable later.
- **Kernel fixes pending commit.** The IRETQ/`ud2`/timer-count changes and the
  diagnostic cleanup are in the working tree, not yet committed/tagged.
- **Cross-core test suite on AMD.** The Goldmont+ backburner items — **B2/BP2**
  (cross-core IPC latency), **S3** (cross-core thrash), **S9** (IPI storm) — were
  blocked on Goldmont+ IPI delivery. With cross-core ping/pong now running on AMD,
  these are the immediate next runs (`osdev image --mode bp2-only` / `stress`).
