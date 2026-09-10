# 16. riscv64 chaos: core 1 takes interrupts and never switches away

**Status:** open. Captured 2026-09-10 at `34e229d0`, `chaos max-carnage`, round 2 of 100.

This is the best data yet on the chaos wedge, and it says something different from what earlier runs
suggested. Recording it before the next attempt so the reading is not re-derived.

## What the machine said

```
KERNEL PANIC: LIVENESS WEDGE: core 1 made NO progress for 40020514 counter ticks
  (1x the 39998000 allowed); it was running task slot 2; it has taken 94726 timer
  interrupts, last vector 0x00000020; detected by core 3.

 h1=8/93628/s8   h2=5/94726   h3=5/76119/s37   h4=5/95404/s21
 idle sample (hart: armed-in/last-seen-ago/stie/stip) now=1984061925
   h1=-38301619/38384132/STIE/no-stip
   h2=-40378897/40458944/STIE/no-stip
   h3=-398424/438420/STIE/no-stip
   h4=-398420/438416/STIE/no-stip
```

## What that rules OUT

- **Not a dead hart.** Core 1 is `h2`, and it has taken **94726 timer interrupts - more than any
  other hart** (h1 93628, h3 76119, h4 95404). The timer is firing and the trap handler is running.
- **Not a lost timer wakeup.** That was the standing suspicion by analogy with x86 audit A8-1 and
  `project_x86_idle_lost_wakeup`. A hart that is being interrupted 94726 times is not asleep on a
  consumed deadline.
- **Not a location that "varies".** Earlier runs recorded stages 1, 5 and 12 and I read that as lock
  contention. Stage 5 is `neutral-sched`, which is stamped BEFORE the switch, so every healthy hart
  also rests at 5 - h3 and h4 are at 5 and are fine. The stage is not discriminating and was never
  evidence for contention.

## What it points AT

Interrupts arrive, the scheduler runs, and **it never switches away from task slot 2**. Two facts
narrow it:

1. **`h2` is the only hart with no syscall stamp.** h1 has `s8`, h3 `s37`, h4 `s21`; h2 has none. So
   whatever slot 2 is doing, it is not making syscalls - it is spinning in userspace, or the stamp is
   being cleared.
2. The watchdog's definition of "progress" is a context switch. A core whose run queue holds exactly
   one runnable task that never blocks would look identical to this, and would be a **false positive** -
   the scheduler correctly re-running the only thing it has. Under chaos, services are killed
   constantly, so a core briefly holding one non-yielding task is plausible.

So the next question is not "why is the hart stuck" but **"what is task slot 2, and was core 1's run
queue empty apart from it?"** Neither is in the dump today.

## A second, separate defect in the same output

**The panic printed TWICE and the two spliced into each other:**

```
kernel: hart stages at halt (stage/irqs) -KERNEL PANIC: panicked at ...
```

Core 3 and core 2 both detected the wedge and both panicked. The `DUMPED` compare-exchange was meant
to leave a single writer on the lock-free serial path, and it did not - the second panic began
mid-line of the first. A panic report that can be interleaved is a panic report that can be
unreadable exactly when it matters most, and this one was only legible by luck.

## Next step

Add to the wedge report: the NAME of the task in the offending slot, and the offending core's run
queue depth. Those two turn "no progress" into either a real wedge or a false positive, and neither
needs a guess. Fix the panic serialisation at the same time, since it is in the same report path.

Reproduce in QEMU first (`scripts/riscv_run.py --cmd`) - `project_riscv64_chaos_wedge` records that
the board was being used as a debugger and should not be again.

---

## ROOT CAUSE FOUND, by reading rather than by booting (2026-09-10)

**Nothing above ring 0 wedges the machine. The kernel's own tick handler blocks on a lock it takes
with interrupts masked, and chaos is only the load that makes the wait long.**

The chain:

1. Core 1 sat at **stage 5, `NEUTRAL_SCHED`** - stamped by `arch/riscv64/mod.rs::timer_tick`
   IMMEDIATELY before `scheduler::timer_tick_from_irq`, with stage 6 (`TICK_DONE`) stamped
   immediately after. So it entered neutral tick code and never returned.
2. `CORE_LAST_TICK_TSC`, the progress stamp the watchdog reads, is written 69 lines INTO
   `timer_tick_from_irq` (scheduler.rs:1434). The first thing that function does is
   `drain_pending_kstack(cid)`.
3. That calls `free_kstack` and `free_page_table_root` -> `memory::allocator::free_frame`, which takes
   **`alloc_lock()`** - inside a trap handler, interrupts masked. The lock's own comment says
   "IRQ-safe: ALLOC_LOCKED is also taken in interrupt context", which prevents same-core re-entry and
   says nothing about a CROSS-CORE holder.
4. Under chaos the deferred-free queue is non-empty on nearly every tick (constant kill + respawn) and
   the allocator is contended by the spawns, so a core can spin there indefinitely.

**THE DISCRIMINATOR WAS IN THE LOG ALL ALONG.** The panic printed twice, 31 ms apart, from two
different cores - and both report **exactly `94726`** timer interrupts for core 1. The count is
FROZEN, not climbing. The core is not taking interrupts at all; it is inside one trap with them
masked. (The first reading of this note said "94726, more than any other hart" and drew the opposite
conclusion. h4 had 95404. The number was never the point - its STABILITY across two reports was.)

## The fix, and it is a pattern this repo already established

`arch/riscv64/mod.rs::ccache_flush_all` carries the argument verbatim:

> Every caller is inside a trap handler with interrupts masked, so a hart spinning for the holder
> cannot be preempted, cannot service an IPI, and cannot be seen to be alive - which is the exact
> shape of the wedge this port is chasing.

That reasoning was applied to the cache controller and given a try-lock. It was not applied to the
allocator lock on the same path. `drain_pending_kstack` should **try** the lock and leave the work
queued for the next tick when it cannot get it: a deferred free that waits one more tick costs
nothing, and a wait that cannot be preempted costs the machine.

Neutral kernel code, so it lands on every port. Reproduce chaos in QEMU first.

---

## CORRECTION, and the real defect - it is in `arch/riscv64/`, where it should have been looked for first

**The "allocator lock" root cause above is WITHDRAWN.** Checked: `free_frame` wraps `alloc_lock()` in
`crate::smp::without_interrupts`, exactly as its contract requires, and riscv64 implements masking
correctly (`local_irq_save` is an atomic `csrrc sstatus, SIE`, not a stub). x86 and ARM reach the same
`free_frame` on the same path. So nothing there is riscv64-specific and the inference does not survive
being checked. It was built on a real observation and stated with more confidence than it had earned -
the same error as citing `crc-err 0`.

The operator's point is what found the real one: **a new ISA is introduced without touching the rest,
so if the kernel wedges, the fault is in that ISA's own folder.** The neutral scheduler and allocator
have survived 1000 chaos rounds on x86, 100 on the Pi 2 and max-carnage on the Pi 4. The riscv64 arch
layer is weeks old. Elimination points one way.

### The defect: `tp` is the kernel's per-hart identity, and userspace owns it

```
arch/riscv64/trap.rs:436     sd x4, 32(sp)      save the interrupted tp into the frame
     ... nothing reloads the kernel's hart id ...
arch/riscv64/trap.rs:488     ld x4, 32(sp)      restore tp FROM THE FRAME on the way out
```

`x4` is `tp`, and `core_id()` (mod.rs:1531) is `mv {}, tp`. `tp` is written exactly three times in the
whole port: once at boot (mod.rs:344), once at AP boot (mod.rs:3039), and by that epilogue - which
writes it on **every return to user mode**, from task state.

So for the entire duration of any trap taken from user mode, the kernel's idea of which hart it is
running on is whatever the interrupted task last had in `tp`. On RISC-V that register is the
userspace THREAD POINTER: user code is architecturally entitled to write it.

Everything the wedge involves is indexed by it: `CORE_LAST_TICK_TSC` (the watchdog's own progress
stamp), `note_stage`, `note_irq`, and every other `PerCore` lookup reached from a trap.

This port's own note calls the invariant out - "tp holds hart id (nothing ever writes it)" - while the
trap epilogue writes it on every sret. It has been correct by accident, not by construction.

### Why this matters beyond one wedge

A core whose `tp` is wrong stamps ANOTHER core's progress slot, and then looks dark to the liveness
watchdog while it is demonstrably running. That is the exact captured signature: stage stamps that
advanced and then froze, an interrupt count IDENTICAL in two panic reports 31 ms apart, and a progress
slot that never updated.

It is also a Commandment problem, not only a bug: a userspace task can make the kernel misidentify its
own core by writing one register. Nothing above ring 0 may be able to do that.

**NOT claimed: that this is proven to be the cause of the captured wedge.** It is a defect that must be
fixed on its own merits, and the wedge's signature is consistent with it. Proving causation means
fixing it and re-running.

### The fix needs design, not a patch

`sscratch` is already the sp latch (it holds the interrupted `sp` from user, and zero while kernel code
runs), so there is no free CSR holding per-hart state and no kernel-owned source for the hart id inside
a trap. Candidates:

1. **`sscratch` points at a small per-hart struct** `{ kernel_sp, hartid }` instead of holding `sp`
   directly - the standard RISC-V approach (Linux, xv6). Correct, and a real refactor of a delicate
   entry path.
2. **Derive the hart id from `sp`** by giving each hart a power-of-two-aligned trap stack and masking.
   No CSR needed; changes stack allocation.
3. **Reserve `tp` for the kernel in both privilege modes**, with the epilogue writing the hart id
   rather than the saved value. Cheapest, and NOT sufficient on its own: a task can still write `tp`
   between `sret` and its next trap, so the kernel would still be trusting a user-writable register.
   Rejected for that reason.

(1) is the right one. QEMU can exercise it: `scripts/riscv_run.py --cmd`.

---

## STATE AFTER FIVE FIXES (2026-09-10 late) - the remaining question is the KILL/PICK RACE

Five real defects were found and fixed in `arch/riscv64`, each by reading that folder rather than
theorising, none touching neutral kernel code (`shared_surface_check` confirms it per commit):

| # | defect | commit | effect |
|---|--------|--------|--------|
| 1 | `tp` (hart id) restored from a USER-writable register on every trap return | 764575f1 | wedge unchanged; made the DUMP trustworthy, which found 2-5 |
| 2 | `sv39` walks followed an unchecked PPN; a bad phys address HANGS this ISA instead of faulting | ef3d3398 | stage 12 gone for good |
| 3 | `satp` installed unchecked | 664ce3ca | guarded (never fires) |
| 4 | the switch was ONE naked function, so it could neither check nor report - split Pi-style | 4b0c7c67 | failures became panics |
| 5 | kernel `sp` restored unchecked - worse than the root, because a trap runs ON it | 43b55ab1 | guarded (never fires) |

Plus the instrument that made the last two possible: `note_stage` records its caller's RETURN ADDRESS
and the dump prints it (`h2=17/2235/s37@0x40215d3c`). Resolve with `llvm-nm` + `llvm-objdump`. Three
stage numbers had been ambiguous before this - a stage names a FUNCTION and a function has callers.

**WHERE IT STANDS.** Two captures in a row land at the SAME resolved site:

```
h2 (wedged)  -> timer_tick_from_irq + 0x65c   immediately after `prepare_ring3_switch` returns
h3/h4 (ok)   -> timer_tick_from_irq + 0x0aa
```

with `switch_context` next, and BOTH new guards silent - so the root was valid and the stack pointer
was valid. The hart still disappears.

**THE STRUCTURAL READING, and why the next step is NOT a sixth guard.** Every one of findings 2, 3 and
5 is the same shape: a value read from a context that is being RECLAIMED underneath the reader.
`pick_next` checks `TASK_VALID` and `TASK_STATE == Ready`, and between that check and
`switch_context` consuming the context, another core can kill the task, free its kernel stack and
reclaim its page tables. Guarding each field in turn treats the symptom; the window is the disease.

**NEXT (do this rather than more guards):**
1. Establish whether the wedged task was killed concurrently - the capture already hints at it: slot 5
   `block-driver` and slot 3/4 `time` are chaos targets, and the last one carried a syscall stamp
   (`s37`) where the previous had none.
2. Decide the fix in the kill/pick ordering: either a task cannot be reclaimed while any core may
   still switch to it, or the switch re-validates the slot's liveness after `pick_next` under the
   same CAS discipline the Running->Ready transition already uses ([[feedback_cas_not_store]]).
3. That is NEUTRAL kernel code, so it lands on every port. If it is a genuine neutral race, other
   ports have it latent and riscv64 merely EXPOSES it - the same story as the xhci hot-plug fix.

**Do not re-litigate:** the allocator lock (contract honoured, masked correctly), `report_fault`
(stage 12 gone), the drain (`KSTACK_USED`), `uart_rx_drain` (bounded, wakes only on a real byte),
`panic_halt_check`/`apic_send_eoi`/`rearm_tsc_deadline` (stubs on this port).
