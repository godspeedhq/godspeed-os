# Bug 2 — Intermittent KERNEL PF (rip=0) on core 0 after a benchmark/test completes

**Date:** 2026-05-31 · **Hardware:** HP T630 (AMD GX-420GI) · **Branch:** `stress/cross-core-t630`
**Status:** OPEN — surfaced while clearing S9; deferred until S3 is cleared, then to be chased.

## Symptom

During the isolated S9 IPI-storm run (`osdev image --mode iso-s9`), across 6 captured boots
S9 passed every time (`stress: S9 pass (100/100)`, 6/6), but in **1 of the 6 boots** a kernel
page fault fired **right after** the S9 pass, during the post-test idle. The serial tail was
flooded with the `#PF` canary byte `P` (the PF stub writes `P` as its first instruction),
interleaved with one detail line. De-interleaved (P-stripped):

```
KERNEL PF: fault_addr=0x0000000000000000  error_code=0x...00000000  rip=0x0000000000000000
           hw_user_rsp=0x0000000000000000  per_core_ursp=0x000000007fffef88
           GS.base=0xffffffff8013eff0      KERNEL_GS=0x0000000000000000
```

## Reading

- **`rip=0x0`** — the CPU was executing at address 0 and faulted fetching the instruction
  (`fault_addr=0`, error_code low 32 = 0 → not-present, read, supervisor). A task was resumed
  with a **zeroed/corrupted instruction pointer** → wild jump to null.
- **`GS.base=0xffffffff8013eff0`** = core 0's kernel GS → the fault is on **core 0**
  (where `stress-s9-send-a` lives; after S9 passes it `idle()`s).
- **`hw_user_rsp=0`** but `per_core_ursp=0x7fffef88` — user-RSP handling is in the frame; worth
  checking whether the resumed context's saved RSP/RIP were both clobbered.
- The `P` flood = the fault **recurs** (fault → canary writes `P` → iretq → same fault), i.e. a
  fault loop, not a one-shot.

## Notes / hypotheses

- **Intermittent (~1/6)** and **post-test** (after the workload finished, on the idle path).
- NOT the parked restart-storm kstack-UAF (Bug B): iso-s9 does no kills/respawns.
- Candidate: a task whose `TaskContext.rip` is 0 (uninitialised or clobbered) gets scheduled —
  e.g. a probe that returned/idled leaving a bad saved context, or a context-switch save/restore
  race on the idle transition. Core-0-specific (BSP) is a hint.

## Update — also reproduces in iso-s3 (2026-05-31)

iso-s3 run (3 boots): S3 passed (50/50) on the boot that completed, but the PF appeared twice:
- **post-S3-pass** (`P`-flood of ~6014, empty payload) — same post-test idle crash as iso-s9.
- **early in boot 1** (`P`-flood of ~11329 interleaved with `init_syscall: core=0 LSTAR=…`) —
  boots 1 and 2 never reached the S3 pass, suggesting an early crash there too.

So it is **not** post-test-specific; it bites during/after boot as well, and it **loops** (the PF
stub re-fires forever flooding `P`) instead of halting — which drowns the detail line. First fix
to chase it: make a kernel `#PF` **halt after one dump** (no iretq back into the fault), and add
the faulting task slot + saved `TaskContext` (rip/rsp/cr3) to the dump so the `P`-flood stops and
the fault is legible.

## Update 2 (2026-05-31) — ROOT LOCALIZED: resumed with rip INTO the kstack pool (≈ Bug B)

Reproduced reliably on iso-s9 (31 boots: S9 passes 31/31; fault in ~half, right after the pass).
Two boots gave readable de-interleaved detail:

```
PF-task: core=2  cur_slot=4  IDLE=224
KERNEL PF: fault_addr=0xffffffff80295200  error_code=0x11  rip=0xffffffff80295200
           per_core_ursp=0x7fffefa8  GS.base=0xffffffff8013f010 (core 2)
```

- **cur_slot=4 on core 2 = `stress-s9-recv`** — the receiver, after it logs the pass and enters
  `idle()` (`loop { yield_cpu() }`).
- **rip = 0xffffffff80295200, error_code=0x11 (Present + Instruction-fetch = NX violation).**
  `KSTACK_STORAGE` is at `0xffffffff80245220`; `0x80295200 - 0x80245220 = 0x4FFE0` ≈ the **top of
  kstack slot 4** (slot 4 = stress-s9-recv). So the task was **resumed with `rip` pointing into
  its own kernel stack** — `switch_context`'s final `ret` popped a stack value as the return
  address and jumped into non-executable stack. (Other instances showed `rip=0` — i.e. the popped
  garbage varies.)

**This is the same symptom as the parked Bug B** ("wild jump into KSTACK_STORAGE" at `0x80303070`,
another slot in the same pool, under restart-storm churn). Bug 2 and Bug B are very likely **one
bug**: a context save/restore corruption that leaves a task with a bad saved `rsp`/`rip`, so the
context-switch `ret` lands inside the kstack. Different triggers (heavy cross-core recv + idle vs
restart churn), same fault. Intermittent → an SMP race (cf. [[feedback_cas_not_store]] — known
scheduler SMP state races).

## Update 3 (2026-05-31) — fault is at K0T-32; PF-time dump obscures the clobber

Captured a clean dump (PF-ctx + PF-stk) on core 2 / slot 4 (stress-s9-recv):
```
KERNEL PF: rip=0xffffffff80295200  error_code=0x11 (NX, instruction-fetch)
PF-ctx: saved_rsp=0xffffffff80293060  saved_rip=0x408298(user entry)  cr3=0xa10000
PF-stk[rsp-16..+40]: 8013ef50 0 0 0 ffffffff00000000 0 ffffffff8010bed5 ffffffff80101288
```
- **fault rip 0x80295200 = K0T-32 for slot 4** (slot-4 kstack top = 0x80295220). That is *exactly*
  the ring-3 interrupt-frame CS slot the `context_switch.rs::new_user` comment (lines 166–187)
  documents as hazardous. So a `ret` jumped to that address.
- The two valid return addresses in the dump map to **`pf_handler` (0x8010bed5)** and **`pf_stub`
  (0x80101288)** — i.e. `TASK_CTX[4].rsp` (suspend-time) sits just below the **current fault
  handler's own frames**. The PF-time dump therefore shows the fault handler's stack, not the
  write that clobbered the ret-target. The fault destroys its own evidence.

So the PF-time approach is tapped out. To find the clobbering write we need to catch it **before**
the fault — e.g. a guard in the scheduler that validates `[TASK_CTX[next].rsp]` (the value
`switch_context`'s `ret` will pop) is a kernel-code address *before* switching, logging slot +
bad target + halting cleanly. (Caveat: if the corruption happens *after* restore, during the
task's kernel execution, the restore-time guard won't see it — but it cleanly settles the
restore-vs-execution question.)

## Update 4 (2026-05-31) — NOT restore-time: corruption is during kernel EXECUTION

Added `dbg_guard_ctx(next)` at all 4 `switch_context` resume sites: it reads the value the
`ret` will pop (`[TASK_CTX[next].rsp]`) and halts with `BADCTX` if it is 0 or a kstack pointer.
Over **21 boots** (S9 passed 21/21, faults still occurred): **`BADCTX` fired 0 times.** So the
ret-target is **always valid at restore** — `switch_context` restores a good context every time.

**Conclusion: the corruption happens *after* restore, during the task's own kernel execution.**
stress-s9-recv resumes cleanly, runs, and a `ret` *later in its run* lands on a garbage value
(0 or a kstack address ≈ slot-4 top / K0T-32). Consistent new datum: the suspend-time
`saved_rsp = 0xffffffff80294000` on every clean dump (same stack depth each time).

This shifts the suspect from the context-switch save/restore to **a write that clobbers a return
address on the kstack mid-execution.** Prime candidate (matches the K0T-32 fault address and the
`context_switch.rs::new_user` comment): a **timer interrupt taken from this ring-3 task** (or a
nested interrupt while it is in the recv syscall) whose frame / ISR handling overwrites a
stack-resident return address. Heavy cross-core recv (100× block/wake via IPI) maximises the
interrupt+context-switch interleavings on this stack.

Next options: (a) reason through the ring-3 timer-ISR / nested-interrupt path on the kstack
(no hardware); (b) stack-canary instrumentation around the recv/idle path to catch the clobbering
write (more involved than the simple guards so far).

## Update 5 (2026-05-31) — code analysis: refutations + leading suspect

Dug through the ring-3 kstack/interrupt management (no hardware):

- **REFUTED — stale TSS.rsp0 / kernel_rsp.** `prepare_ring3_switch` (scheduler.rs:653) re-points
  both `PER_CORE_SYSCALL[core].kernel_rsp` (→ K0T-512) and `TSS.rsp0` (→ K0T) to the incoming
  task's kstack before **every** ring-3 resume. Not a stale-pointer omission.
- **The bug class is documented in `prepare_ring3_switch` (lines 656–667):** a timer-ISR return
  address saved at ~K0T-200 gets clobbered by an overlapping frame → rip=0. Mitigation: SYSCALL
  frames start at K0T-512 so they never reach K0T-200. **But the comment's depth bound (K0T-260)
  is for the timer ISR's _early-return_ path only.** The timer-ISR _context-switch_ path
  (timer → timer_tick_from_irq → pick_next → prepare_ring3_switch → switch_context) has **no
  documented depth bound**; if it descends past K0T-512 it overlaps the SYSCALL region and can
  clobber a suspended syscall call-chain's return address. **Leading remaining suspect.**
- **CORRECTION — Bug 2 ≠ Bug B.** Bug B is a kstack use-after-free (needs a kill/free under
  restart churn). iso-s9 does **no kills**, so nothing is freed — Bug 2's corruption is not a
  free-then-reuse. Same symptom (rip → kstack), different mechanism. The earlier "one bug"
  unification (Update 2) is withdrawn.

To confirm the leading suspect needs either disassembly-level stack-depth accounting of the
timer-ISR-switch path vs the K0T-512 boundary, or **stack-canary instrumentation** (place a
sentinel at K0T-512±, on each ring-3 resume / timer tick check it's intact) to catch the
overlapping write directly. Both are heavier than the simple guards so far.

## Update 6 (2026-05-31) — ROOT CAUSE CONFIRMED: timer-switch over-descends the kstack

Stack canary (0xCAFEBABE at K0T-{280,344,440,504}, checked each timer tick) fired:
```
CANARY: slot=4 k0t=0xffffffff80296220 off=280 got=0x0   (stress-s9-recv,   core 2)
CANARY: slot=5 k0t=0xffffffff802a6220 off=280 got=0x0   (stress-s9-send-a, core 0)
CANARY: slot=6 k0t=0xffffffff802b6220 off=280 got=0x0   (stress-s9-send-b, core 1)
```

**Confirmed mechanism.** The timer-ISR *context-switch* path (TSS.rsp0=K0T → timer_isr_stub →
timer_tick_from_irq → pick_next/prepare_ring3_switch/switch_context) descends **past the
documented K0T-260 bound** to at least **K0T-280** and **zero-writes** the gap (`got=0x0` —
a Rust frame's zero-init local). It is **systematic**: every ring-3 task on every core has its
K0T-280 canary zeroed. The `new_user` mitigation (SYSCALL frames at K0T-512, assuming the timer
ISR only reaches K0T-260) is therefore **unsound for the switch path**.

The **crash** is intermittent only because it requires the descent to reach far enough down (to
~K0T-512, the top of the SYSCALL region) to zero a *suspended* syscall's return address — which
then `ret`s to 0 (rip=0) or a stale stack value (rip ≈ kstack addr). The over-descent itself is
not intermittent.

**Open for the fix:** how deep does the switch path actually reach? The check halts on the first
clobbered canary (K0T-280); to size the fix we need the *max* depth (does it cross K0T-512?).
Either report all clobbered canaries (deepest-first) in one more run, or just adopt a fix that
removes the shared-stack overlap entirely:
- give hardware interrupts a dedicated per-core IST stack (timer ISR never touches the task
  kstack), reconciling it with switch-from-ISR; **or**
- lower the SYSCALL region far enough below the measured max timer-switch depth; **or**
- bound/trim the timer-switch path's stack use.

## Update 7 (2026-05-31) — ROOT CAUSE FULLY UNDERSTOOD + fix direction

Depth measurement: for the heavy-recv tasks (slots 4/5/6) the canary reports
`deepest_off=504 got=0x0 clobbered=8/8` — the timer-switch path zero-writes the **entire**
gap down to **K0T-504**, i.e. it reaches the K0T-512 SYSCALL boundary. (logger, slot 3:
`deepest_off=400 got=0x400000 clobbered=1/8` — a shallower, different write.)

**Why the K0T-512 mitigation is ineffective on this hardware (the real root):** on the
GX-420GI the live syscall mechanism is **`ud2`/#UD** (IDT[6]), a CPU exception. Exceptions use
**`TSS.rsp0 = K0T`** (IDT[6] has `ist=0`), and `ud2_syscall_entry` is verified to **NOT** re-base
RSP to `kernel_rsp` — it processes the exception frame in place on the `K0T-40`-down stack. The
`PER_CORE_SYSCALL.kernel_rsp = K0T-512` separation only ever protected the **LSTAR `SYSCALL`**
path, which is dead here (§ milestones/v2 issue #2). So **the syscall call-chain and the timer
interrupt + context-switch path both grow down from K0T-40 with no separation**, and they
overlap: a deep recv syscall's return addresses sit in [K0T-280, K0T-512], exactly where the
timer-switch zero-writes → the task later `ret`s into zeroed/garbage → wild jump into the kstack.
This is why it's cross-core-recv-specific (max interrupt-driven switches) and intermittent (depends
on the timer landing while a syscall chain occupies that region).

**Fix options:**
1. **Re-base #UD syscalls** in `ud2_syscall_entry` to a separate stack (the original K0T-512
   intent, but for the #UD path) so the syscall chain lives below the timer-switch's reach.
   Must copy/relocate the CPU exception frame — fiddly but localized.
2. **Dedicated IST stack** for the timer (and other) interrupts so they never touch the task's
   kstack; reconcile with switch-from-ISR (saving an IST rsp into a per-task ctx is wrong).
3. Give the syscall path more headroom AND verify the timer-switch max stays above it (the
   canary maxes at K0T-504; the real max may be deeper — needs a separate-stack probe to size).

The constitutional-clean fix is (2) but it is the most invasive. (1) most directly addresses the
identified root (no syscall/interrupt separation for #UD).

## Update 8 (2026-05-31) — evidence gap before the fix; do NOT implement blind

Reviewed the fix readiness. Two cautions recorded so the fix isn't attempted on shaky ground:

- **Canary placement caveat.** For the *recv* probe (slot 4), the canary gap [K0T-280, K0T-504]
  overlaps its 4 KiB `Message` stack buffer, so part of that probe's canary hit is its own
  message zero-init — NOT necessarily the timer-switch. The **senders** (slots 5/6, no 4 KiB
  message) firing 8/8 still implicates the timer-switch path, so the structural overlap is real,
  but the recv-probe canary is partly a false positive.
- **The precise clobber is not yet pinned.** We have the structural root (no #UD-syscall /
  interrupt stack separation) and that the timer-switch zero-writes the gap, but NOT the exact
  return-address slot that gets overwritten nor the exact interleaving. Implementing a stack
  re-base (boot-critical naked asm) without that risks separating the stacks yet not killing the
  fault, or fixing the wrong thing.

**Resume plan:** (1) pin the exact clobber — canaries placed *around the real return-address
slots* (not inside the 4 KiB message buffer), or a debug-register (DR0) watchpoint on the
faulting slot's clobbered word, to capture which write does it; (2) only then implement the
structural fix (re-base #UD syscalls in `ud2_syscall_entry` to a separate/lower stack), and
verify the fault is gone on hardware.

## Next steps (when chased)

1. Make it reproduce reliably (1/6 is too rare) — e.g. loop the isolated probe, or add a probe
   that returns from `service_main` to exercise the post-return path deterministically.
2. Capture the faulting task slot + its `TaskContext` (rip/rsp/cr3) at fault time.
3. Check the idle/return path: what runs on core 0 after a probe calls `idle()` / returns.
