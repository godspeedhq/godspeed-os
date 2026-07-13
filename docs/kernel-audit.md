# Kernel Commandment Audit

> **Living document.** Records every audit of the ring-0 kernel against the constitution's
> invariants. Re-run and append with each audit. First audit: 2026-07-11.

## North-star invariant

**Nothing above the kernel may panic or wedge the kernel.** For any userspace action - any syscall
with any arguments, any IPC message, any capability use, any driver MMIO/DMA, any hardware state - the
kernel's only allowed responses are: **perform it**, **return a defined error**, or **kill the offending
task**. Never a kernel panic; never an unbounded hang. (Invariant 12; CLAUDE.md 26.6 bounded, 26.4 no
silent fallback, 3.1 validate-before-act; 6.2 the kernel may panic ONLY on its own already-corrupted state.)

### Triage rule (A/B/C)

Every `panic!`/`unwrap`/`expect`/`assert`, every loop/wait, every silent fallback, every arithmetic/index
on a user value, and every driver-hardware/lifecycle access is classified:

- **(A)** unreachable from userspace - not recorded.
- **(B)** a *correct* loud panic on already-corrupted **kernel** state - recorded so no one "fixes" a defense.
- **(C)** reachable from userspace input/behavior/hardware - a **violation** to fix.

## Audit 1 - 2026-07-11 (full-kernel sweep)

Method: 9 parallel subsystem auditors (syscall, ipc, capability, task, memory, smp, arch-cpu, arch-device,
misc), each triaging its files A/B/C, then an adversarial verify pass on every C to confirm it is genuinely
reachable (default: not-a-bug unless a concrete trigger exists). Result: **3 confirmed violations, 5 investigated-and-cleared, 24 correctly-loud panics documented.**

### Confirmed violations (fix these)

> **Status (2026-07-11): all 3 FIXED on `feat/dell-wyse-5070-goldmont-plus`.** C1+C2: the CPU-exception
> vectors 0-31 now discriminate the saved-CS CPL like `pf_handler` - a ring-3 exception (#GP, #DE, #MF,
> #AC, #XM, ...) calls `kill_current()`; only a ring-0 exception halts (`gpf_stub`/`gpf_handler` +
> `exc_stub_noec`/`exc_stub_ec`/`exc_dispatch`, wired in `init_idt`). C3: the runtime supervisor respawn
> calls the non-panicking spawn and re-arms `PENDING` on a transient error instead of `panic!`.
> Boot-verified no regression; a dedicated adversarial regression test (ring-3 `cli`/`div0` -> task
> killed, kernel alive) is the follow-up validation.

#### C1. [HIGH] `kernel/src/arch/x86_64/boot.rs:1344` - arch-cpu (hardware-death)

**What.** A #GP raised by ring-3 code dispatches to gpf_stub -> gpf_handler, which UNCONDITIONALLY calls halt_all_cores() - a whole-machine kernel wedge triggered by userspace. Unlike pf_handler (which checks the user/supervisor bit and calls kill_current for ring-3 faults), the #GP path never inspects the saved CS CPL and never kills the offending task.

**Trigger.** Any ring-3 service that raises #GP(0): a non-canonical data access (e.g. `mov rax, [0x8000_0000_0000_0000]`), a privileged instruction (`hlt`/`cli`/`wrmsr`/`rdmsr`/`in`/`out`), or a bad segment load. Services need not be Rust (Appendix B.2 admits any freestanding ELF) and fuzz F3 bit-flips ELFs, so this is trivially reachable. The IDT (init_idt, boot.rs:1139) routes vector 13 -> gpf_stub with no CPL discrimination; gpf_handler -> halt_all_cores() halts every core. Not covered by adversarial suite A1-A10 (A10 tests syscall-arg validation, not a direct ring-3 fault).

**Fix.** Make gpf_stub mirror pf_stub: for #GP the CPU pushes an error code, so the saved CS is at [rsp+16] - `test byte ptr [rsp+16], 3`; swapgs when ring-3; pass the CPL to gpf_handler. gpf_handler must call crate::task::kill_current() for CPL==3 (service continues) and only halt_all_cores() for CPL==0 (genuine kernel-state corruption).

#### C2. [HIGH] `kernel/src/arch/x86_64/boot.rs:1435` - arch-cpu (hardware-death)

**What.** The catch-all exception_halt (installed at every IDT vector except 6/13/14 in init_idt) unconditionally halts all cores via exception_halt_handler + the `2: hlt; jmp 2b` loop. CPU exceptions that ring-3 code can raise are therefore fatal kernel wedges instead of killing the faulting task.

**Trigger.** Ring-3-reachable exception vectors land here. Most direct: vector 0 #DE via integer divide-by-zero or INT_MIN/-1 overflow in an adversarial or fuzz-mutated (F3) service binary -> exception_halt -> halt_all_cores(). Also #MF (16), #XM (19) via unmasked FP/SIMD exceptions. Each halts the whole multi-core kernel from a single ring-3 instruction.

**Fix.** exception_halt already reads the frame words and identifies the CS slot (0x08 kernel vs 0x28 user) in exception_halt_handler. Use that CPL determination to branch: for a ring-3 CS, swapgs (if needed) and kill_current() so only the offending service dies; reserve the halt loop for ring-0 (CPL==0) exceptions where kernel state is actually compromised.

#### C3. [MEDIUM] `kernel/src/task/mod.rs:3725` - task (panic)

**What.** Runtime supervisor respawn panics the whole kernel on ANY transient spawn failure, defeating the Phase-6 guarantee that supervisor death never reboots.

**Trigger.** Kill the supervisor (`chaos kill-storm supervisor` / control channel) while task slots / frames / kstack pool are momentarily exhausted (e.g. a shell/chaos with SPAWN authority storming transient pipe services). poll_supervisor_respawn() (3704) calls spawn_supervisor() (3725), which does `Err(e) => panic!("supervisor spawn failed")` (3648). A NoMemory/CapTableFull/MapFailed from resource pressure at that instant becomes a kernel panic + reboot - a userspace-reachable DoS reboot.

**Fix.** Split boot-time (fatal) from runtime respawn. In poll_supervisor_respawn, call the non-panicking spawn_service_with_config directly; on Err, log loudly and re-set SUPERVISOR_RESPAWN_PENDING (and clear IN_PROGRESS) so the next Core-0 tick retries, instead of panicking. Only the boot-path spawn_supervisor should remain fatal (Test 1B).

### Backlog hardening pass - 2026-07-11 (post-A14)

The two genuinely-unbounded fault/hardware spins below were *cleared* in Audit 1 (their wedge trigger
is not userspace-controllable) but each is still a latent silent-freeze on absent/wedged hardware -
an invariant-12 / §26.6 gap, and directly relevant to new-hardware bring-up (Wyse 5070). Both are now
**bounded** (committed on `feat/dell-wyse-5070-goldmont-plus`). Behaviour is unchanged on healthy
hardware: a live RTC clears UIP in ~microseconds and a live UART empties its holding register in
microseconds, so the caps are never reached in practice; they only convert a dead-hardware infinite
hang into a bounded, best-effort read/proceed.

- **`kernel/src/arch/x86_64/rtc.rs:125`** - FIXED. `read_datetime_raw` bare `while update_in_progress()
  {}` (x2) replaced with bounded `wait_update_clear()` (`RTC_UIP_SPIN_CAP = 1_000_000`); the
  two-reads-agree retry loop capped at `RTC_CONSISTENCY_TRIES = 128`. On a dead RTC (reads 0xFF, UIP
  bit stuck) the read now returns garbage that `year_plausible` / `deglitch_epoch` already reject,
  keeping the last known-good time - loud-degrade, not a freeze.
- **`kernel/src/arch/x86_64/boot.rs:723`** - FIXED. `serial_poll_thre` (lock-free fault-path THRE poll)
  bare `loop` capped at `SERIAL_THRE_NOLCK_CAP = 1_000_000`; on timeout it proceeds best-effort exactly
  like the already-bounded `serial_thre_wait` (worst case: one dropped diagnostic byte, never a wedge).
- **`kernel/src/arch/x86_64/boot.rs:1452`** (pf_handler fall-through, cleared-fragile) - CLARIFIED. The
  fall-through to `halt_all_cores()` after a ring-3 kill is intentional and fail-safe (halt is the safe
  outcome should `kill_current` ever return; it does not for a ring-3 fault). Comment aligned to the
  sibling `gpf_handler` / `exc_dispatch` idiom introduced by the C1/C2 fix, so the non-return contract
  is explicit rather than implicit. No behaviour change.

Done in a later pass (Item 2, committed `cb24515`):
- **`kernel/src/task/scheduler.rs` driver-death quiesce** - DONE generically, respecting §4.4. Added
  `nic-driver` to the DMA-quiesce (bus-master-clear) set (it was missing - a passthrough NIC DMAing
  into reused frames on death), and added `interrupt::route::unregister` + an IOAPIC line-mask on
  driver death (before the endpoint id is freed) to close the reused-endpoint-id stale-IRQ-route gap.
  Deliberately NOT kernel-side HC reset: that embeds per-device MMIO maps in ring 0 (a §4.4 violation)
  and is redundant - every driver resets its controller on init, so a respawn re-inerts it. A
  bus-master-disabled controller with its route removed + line masked is provably inert with zero
  device knowledge in the kernel. Identity 24/24.

### Investigated and cleared (not violations, but recorded)

- **`kernel/src/task/scheduler.rs:1761`** (task/hardware-death, claimed medium) - MARQUEE: on driver death the kill path only clears PCI bus-master-enable and (for level IRQs) leaves masking to deliver(); it never HALTS/RESETS the controller and never tears down the IRQ route. A co
  - *Cleared:* Traced the real path. On driver death kill_task clears PCI bus-master-enable (pci.rs:159, a straight-line RMW of the Command reg) before frame reclaim, and releases the IOMMU device. It is true it issues no HCRESET/Run-Stop clear and there is no interrupt::route::unregister. But a controller left ru
- **`kernel/src/task/mod.rs:2982`** (task/silent-fallback, claimed low) - resolve_spawn_core returns a placement_override core id unchecked; a spawn onto a non-ready core produces an unschedulable (silently stuck) task instead of a loud PlacementInvalid (violates §9.2 / inv
  - *Cleared:* Traced the full path. handle_spawn (dispatch.rs:537) validates a SPAWN capability before touching core_override, so an ordinary ring-3 task with no caps cannot reach resolve_spawn_core at all; only supervisor/shell/chaos/probes hold SPAWN. The override is masked to 16 bits (core_raw = (arg0>>16)&0xF
- **`kernel/src/arch/x86_64/boot.rs:1415`** (arch-cpu/panic, claimed medium) - pf_handler kills the task for a user #PF (error_code bit 2 set) but then FALLS THROUGH to halt_all_cores(); it is correct ONLY because kill_current() is assumed to diverge (never return). If kill_curr
  - *Cleared:* The fall-through is real in source (kill_current() is typed -> (), not -> !, and halt_all_cores() follows unconditionally), but it is not reachable by a ring-3 page fault. The kill branch runs only when error_code bit 2 (U/S) is set, which the CPU sets exactly for a CPL=3 fault. A CPL=3 fault implie
- **`kernel/src/arch/x86_64/boot.rs:723`** (arch-cpu/unbounded-loop, claimed low) - serial_poll_thre() spins on COM1 LSR bit 5 (THRE) with NO iteration cap, unlike mod.rs::serial_thre_wait which bounds the same poll (THRE_SPIN_CAP). It is used by the lock-free fault-path serial helpe
  - *Cleared:* serial_poll_thre() (boot.rs:720) is genuinely an unbounded `loop` with no iteration cap, unlike the bounded mod.rs::serial_thre_wait (THRE_SPIN_CAP=1_000_000). The SITE is reachable from ring-3: a task with no caps can page-fault (write to unmapped addr, Test 7.B) → pf_stub → pf_handler → serial_put
- **`kernel/src/arch/x86_64/rtc.rs:125`** (arch-device/unbounded-loop, claimed medium) - read_datetime_raw() spins in an unbounded `while update_in_progress() {}` on CMOS status register A bit 7; an absent or wedged RTC (reads 0xFF, so bit 7 is permanently set) makes this loop never termi
  - *Cleared:* The loop IS unbounded and the syscall path IS ungated, but the WEDGE TRIGGER is not userspace-controllable, so this is not a userspace-reachable wedge.

Path verification (all confirmed): dispatch.rs:1280 whitelists query_id 11 and 17 as ungated (`matches!(query_id, 0|3|9|10|11|12|13|14|15|16|17|18)

### Correctly-loud panics (B - do NOT remove; these are the defense)

- **`kernel/src/syscall/dispatch.rs:793`** (syscall/assert) - handle_kill calls assert_cap_table_consistent() after a userspace-triggered kill, which panics if any cap in the kernel tables carries generation > its resource's current generation. This is a correct loud guard on CORRU
- **`kernel/src/syscall/dispatch.rs:792`** (syscall/assert) - handle_kill calls assert_tcb_alive() after a kill; the function panics if a TCB service is found Dead (§6.2). It is currently INERT because the non-restartable TCB set is empty (const TCB: &[&str] = &[]) following Path C
- **`kernel/src/ipc/mod.rs:56`** (ipc/panic) - alloc_endpoint_id panics when the monotonic endpoint-id counter reaches DELEGATED_BASE (4096). This is a loud backstop guarding kernel id-space integrity: colliding endpoint ids with the delegated/file-cap band (capabili
- **`kernel/src/ipc/routing.rs:157`** (ipc/panic) - routing::register panics when all MAX_ENDPOINTS (96) routing slots are valid AND alive. Loud backstop on routing-table exhaustion. Not userspace-unbounded: register() is only called from the kernel spawn path (task/mod.r
- **`kernel/src/capability/generation.rs:31`** (capability/panic) - Generation::bump() uses checked_add(1).expect("generation overflow") - the deliberate H7 loud backstop: at u32::MAX it panics rather than wrapping to a low value, which would resurrect a stale cap's authority. Userspace 
- **`kernel/src/capability/generation.rs:59`** (capability/panic) - next_generation() panics if the global monotonic AtomicU32 wraps to 0 (which would alias Generation::INITIAL and resurrect authority). Every endpoint creation/spawn increments it; overflow needs ~4.2 billion spawns per b
- **`kernel/src/capability/table.rs:250`** (capability/expect) - mint_cap() does .expect("mint_cap: resource not registered"). All userspace-reachable callers mint only ids that were just registered: spawn endpoints (registered in spawn_service_by_name, and the endpoint id space is gu
- **`kernel/src/capability/table.rs:209`** (capability/assert) - register_at_gen() asserts overflow_len < OVERFLOW_CAP for ids >= DIRECT_CAP (8192). No userspace path can register an id in that range: endpoint ids are guarded to < DELEGATED_BASE=4096 (ipc::alloc_endpoint_id panics fir
- **`kernel/src/task/scheduler.rs:1971`** (task/assert) - block_and_reschedule asserts a running task exists; a kernel-internal invariant (CORE_CURRENT is always a valid running slot inside a syscall), not userspace-steerable.
- **`kernel/src/task/scheduler.rs:901`** (task/assert) - prepare_ring3_switch calls assert_no_mid_execution_migration (panics if TASK_CORE[slot] != running core); enforces static-placement (§9.1). pick_next only returns same-core slots, so a mismatch is a kernel logic bug, not
- **`kernel/src/task/scheduler.rs:1088`** (task/panic) - LIVENESS WEDGE watchdog panics when a core makes no progress for ~3s. This is the intended loud-stop defense (invariant 12 / §26.7); it fires on kernel-internal stall state (skew-guarded, TSC-quantum-gated) and is the co
- **`kernel/src/memory/allocator.rs:41`** (memory/panic) - guard_bugcheck panics if alloc_frame is about to hand out a frame inside the kernel-image range [GUARD_START,GUARD_END). Kernel-image frames are never marked free (init_from_map skips [kstart,kend); protect_kernel_page_t
- **`kernel/src/memory/allocator.rs:389`** (memory/unbounded-loop) - alloc_lock_wedge panics after ALLOC_LOCKED spins >=1e9 iterations. The critical section is a bounded bitmap scan (<=256 KiB) always held under without_interrupts (all four entry points), so the holder cannot be preempted
- **`kernel/src/smp/ipi.rs:226`** (smp/panic) - TLB-shootdown ack-wait watchdog: after SHOOTDOWN_WATCHDOG_SPINS (~5e8) iterations of request_and_wait, panics naming the core that never acked. This is the intended loud-failure defense - a remote core stuck IF=0 that wi
- **`kernel/src/smp/spinlock.rs:25`** (smp/panic) - lock_wedge: SpinLock lock()/lock_irq() panics after LOCK_WATCHDOG_SPINS (~1e9) iterations, naming the deadlocked lock address. Intended loud-failure defense - a holder that never releases (un-reschedulable holder or AB-B
- **`kernel/src/smp/percpu.rs:88`** (smp/index) - PerCore::get / PerCoreMut::as_mut_ptr bound the core index with debug_assert! only (compiled out in release), then do base.add(core) - an out-of-range core id would be an OOB pointer deref. Confirmed NOT userspace-reacha
- **`kernel/src/smp/placement.rs:24`** (smp/other) - placement::resolve on an out-of-range contract/override core id is SAFE: is_ready(n) casts to usize and returns false for any c >= num_cores() (no panic, no OOB), so resolve returns Err(PlacementInvalid) for any u32. rou
- **`kernel/src/arch/x86_64/boot.rs:1050`** (arch-cpu/assert) - init_syscall asserts EFER.NXE read back as 1 after setting it (W^X foundation). A boot-time, per-core assertion on CPU/MSR state; correct loud-failure if the NX bit cannot be enabled.
- **`kernel/src/arch/x86_64/boot.rs:250`** (arch-cpu/assert) - audit_wx asserts no sampled page is both writable and executable (W^X hardening). Boot-time audit over kernel-owned page tables userspace cannot influence; correct loud failure on a hardening regression.
- **`kernel/src/arch/x86_64/boot.rs:646`** (arch-cpu/hardware-death) - limit_package_cstates RDMSRs MSR 0xE2 whenever is_intel_cpu() is true, assuming every GenuineIntel CPU implements MSR_PKG_CST_CONFIG_CONTROL. An Intel chip lacking 0xE2 would #GP -> gpf_handler -> halt at boot. Early-har
- **`kernel/src/invariants/assertions.rs:13`** (misc/panic) - assert_cap_validated panics if handed an Err, but every one of its ~9 call sites (syscall/dispatch.rs) passes the literal &Ok(()) on the post-validation success path. The panic branch is a tautological tripwire that cann
- **`kernel/src/invariants/assertions.rs:22`** (misc/assert) - assert_no_mid_execution_migration asserts original_core==current_core before every ring-3 resume (scheduler.rs:901). Runs on every context switch so it is heavily reached, but v1 uses static placement - a task is pinned 
- **`kernel/src/invariants/assertions.rs:59`** (misc/panic) - assert_tcb_alive panics when a TCB service is Dead. Called from handle_kill success path (dispatch.rs:792), but the TCB slice is now empty (&[]) since the supervisor became restartable (Path C/Phase 6), so the loop body 
- **`kernel/src/invariants/assertions.rs:86`** (misc/panic) - assert_cap_table_consistent panics if any active cap carries generation > its resource's current generation ('future' cap). Called from handle_kill success path (dispatch.rs:793). Caps are unforgeable kernel structures; 


## Audit 2 - 2026-07-11 (cross-cutting-concern sweep)

Method: a fresh Workflow decomposed **by cross-cutting concern** (not by subsystem, as Audit 1 did) -
8 parallel auditors, one each for: integer arithmetic on user values, array/slice/pointer indexing,
loop/wait boundedness, lock discipline / deadlock, error-path resource cleanup, `unsafe` SAFETY-claim
re-verification, TOCTOU / cross-core races, and syscall input-validation completeness. Each finding was
then adversarially refuted (default not-a-bug unless a concrete userspace trigger exists), same bar as
Audit 1. This lens finds what a subsystem-local auditor misses: defects whose two ends live in
different files (a cause in `syscall_entry.rs`, a fatal consequence in `boot.rs`).

Result: **13 findings -> 5 CONFIRMED violations (all HIGH), 4 refuted C, 4 B-notes.** (One verify agent
hit the structured-output retry cap and dropped its finding unverified; not among the confirmed set.)
The confirmed set includes the precise root cause of the long-standing intermittent chaos-storm UAF
that was an open follow-up (`project_kernel_pf_reclaim_guard`).

### Confirmed violations (fix these)

> **Status (2026-07-11): ALL 3 FIXED on `feat/dell-wyse-5070-goldmont-plus`.** V3 scheduler UAF
> (`2c402ec`): CAS-claim + Dekker re-check (all four handshake accesses SeqCst) so a cross-core kill
> can never free a task mid-switch. V2 spawn leak (`e907e43`): `cleanup_partial_spawn` unwinds the
> endpoint registrations on every post-reservation error path. V1 user-copy halt (`6a0cbb9`): a per-core
> `USER_COPY_ACTIVE` guard + a `pf_handler` branch kill the caller on a bad user pointer instead of
> halting. Identity 24/24 after each. V3's race needs real multi-core HW to exercise, so its final
> validation is a Wyse/T630 chaos storm; V1/V2 are QEMU-testable (a dedicated A15 regression is a
> follow-up, like A14).

#### V1. [HIGH] `kernel/src/arch/x86_64/syscall_entry.rs:105` + `:114` - user-copy fault halts the machine (unsafe-reverify)

**What.** `read_user_bytes` / `write_user_bytes` rely on `validate_user_ptr`, which only **range-checks**
(nonzero, `< USER_END`, no wrap) - it never verifies the pages are present/writable. The kernel then
reads/writes the slice at CPL0. A range-valid-but-**unmapped** (or read-only, for writes) user pointer
faults inside the kernel copy; because the fault is CPL0 the `#PF` error-code U/S bit is 0, so
`pf_handler` prints "KERNEL PF" and calls `halt_all_cores()`. There is no copy-to/from-user fault fixup
(no extable, no per-CPU user-access flag), so the fault is unrecoverable.

**Trigger.** Trivially reachable by **any** service: `log`/`send` with `msg_ptr` = an in-range but
unmapped VA (e.g. `0x1000`) reads the unmapped page at CPL0 (read side, :105); `recv`/`task_stat`/
`inspect_kernel` with an unmapped/read-only `out_buf` faults on the write (write side, :114). One bad
pointer from one service halts every core. This is the most reachable finding in either audit.

**Fix.** Give the user-copy helpers a page-fault fixup: a per-CPU user-access-in-progress flag with a
resume point, and in `pf_handler`, on a CPL0 fault at a user VA while the flag is set, clear it and
resume to the fixup returning `EFAULT` (kill the caller) instead of reaching the U/S-only halt triage.
Range validity is not a mapping guarantee.

#### V2. [HIGH] `kernel/src/task/mod.rs:3604` (and the other post-endpoint `?` sites) - partial-spawn resource leak (errpath-cleanup)

**What.** The recv-endpoint block (mod.rs:3222-3264) allocates an endpoint id, registers the resource,
routing entry, name, recv+grant caps, and per-IRQ routes. Every fallible step **after** it - driver
MMIO map (:3474), DMA-arena map (:3536), ctx-frame alloc (:3604), ctx-page map (:3638), kstack alloc
(:3645) - returns `Err` via `?` **without unwinding** those registrations. The leaked routing entry
stays `valid + Alive`, so `routing::register` can never recycle it and panics at `MAX_ENDPOINTS=96`
(~26 leaks); independently the leaked endpoint id never returns to the free list, marching
`alloc_endpoint_id` into its panic at `DELEGATED_BASE=4096`.

**Trigger.** A sustained `chaos max-carnage` + `chaos mem-pressure` storm: a driver/service respawn that
loses the frame-allocator race fails at one of the post-endpoint maps, permanently leaking one Alive
routing entry + endpoint id per failure. ~26 accumulated leaks panic `routing::register`; the kstack
pool (224 slots) gives a tighter deterministic variant.

**Fix.** Unwind the partial spawn on any post-endpoint error: free the endpoint id, unregister the
routing / name / resource entries and IRQ routes, release the task slot, then return the error.

#### V3. [HIGH] `kernel/src/task/scheduler.rs:992` (`run`) + `:1244` (timer ISR) - pick-then-commit cross-core UAF (concurrency-races)

**What.** The scheduler publishes a just-picked task (`STATE=Running`, `CORE_CURRENT[cid]=next`, then
load its CR3/kstack and `switch_context`) with **no re-check that a concurrent cross-core kill set the
slot Dead**, and it publishes `CORE_CURRENT` only **after** `pick_next` read `STATE=Ready`.
`kill_task_by_slot`'s spin-wait breaks the instant `CORE_CURRENT[peer] != slot`, so in the pick->publish
window it frees the victim's PML4 / user frames / kstack; `switch_context` then loads a freed
(possibly re-alloced-and-zeroed) CR3 -> kernel `#PF` / UAF. The handshake is one-sided (kill does
store-Dead-then-load-CORE_CURRENT; the scheduler does store-CORE_CURRENT-then-use-CR3 with no matching
load of STATE - an incomplete Dekker pattern). The `next != prev` timer path (:1244) is worse: it stores
Running/CORE_CURRENT **unconditionally**, unlike the Dead-preserving CAS used for `prev` and the
`next == prev` path.

**Trigger.** Real multi-core hardware only (TCG serializes cores, cannot repro). A userspace cross-core
kill (`chaos max-carnage`, shell `kill`, supervisor `restart`) of a service pinned to another core,
racing that core's `pick_next` / timer ISR. **This is the precise root cause of the known intermittent
chaos-storm UAF** (b9dbc4c only catches the downstream corrupt-PTE walk; it does not close this window).

**Fix.** After `cli` + publishing `CORE_CURRENT`, re-load `TASK_STATE[next]` (and `TASK_VALID`) and
abort the switch (set `CORE_CURRENT=IDLE`, re-pick) if it is Dead - completing the Dekker handshake with
the kill's store-Dead-then-load-CORE_CURRENT spin-wait. Apply to both `run` and the `next != prev` timer
path.

### Refuted (investigated, not violations)

- **scheduler.rs:1814** kill-path CORE_CURRENT spin has no *iteration* cap - REFUTED: covered by the
  cross-core LIVENESS WATCHDOG (~3s loud panic naming the stalled core) on real HW; the mutual-wait ring
  needed to hang it is not constructible from the serialized kill triggers.
- **scheduler.rs:413** `TASK_SLOT_LOCKED` hand-rolled CAS has no watchdog - REFUTED: every critical
  section is a bounded `MAX_TASKS` scan under `without_interrupts`, no holder can fail to release without
  the kernel already being wedged (a B scenario). Consistency-hardening only.
- **capability/delegated.rs:172** `BAND` uses `lock()` not `lock_irq()` - REFUTED: every acquirer runs
  IF=0 today (syscall interrupt-gate, IF=0 kill path), so no preemptible holder exists. Latent
  future-code hazard, not live.
- **interrupt/route.rs:59** `IRQ_TABLE` uses `lock()` not `lock_irq()` - REFUTED: same, all acquirers
  IF=0; single-array critical sections drain in ns. `lock_irq`-convention hygiene, not a live deadlock.

### B-notes (correctly-loud, do NOT remove) + latent hardening

- **generation.rs:31 / :59** - `bump()` `checked_add.expect` and `next_generation()` wrap-to-0 panic:
  correct H7 defenses (a silent wrap resurrects stale authority). ~4.2e9 bumps/spawns to reach; keep.
- **ipc/mod.rs:55** - `alloc_endpoint_id` panic at `DELEGATED_BASE`: correct backstop against an endpoint
  id aliasing the delegated/file-cap band; kept unreachable by id reuse bounding the live range to <=96.
- **allocator.rs:261** - `free_frame` phantom-frame guard checked only `idx >= max_valid_frame` but
  `max_valid_frame` is set from region extents **unclamped**, while the bitmap is sized `MAX_FRAMES`
  (8 GiB / 4 KiB). On a machine with **> 8 GiB RAM**, a corrupt/stale PTE whose index lands in
  `[MAX_FRAMES, max_valid_frame)` passed the guard and OOB-indexed the bitmap. Not userspace-reachable
  (only a pre-corrupted PTE reaches it - a B), and the T630/Wyse test boxes have 8 GiB (band empty), but
  a genuine latent hardening gap. **FIXED (`f276f61`):** the guard is now
  `idx >= max_valid_frame || idx >= MAX_FRAMES`; the alloc path never returns `idx >= MAX_FRAMES`, so no
  legitimate free is rejected.

### Regression tests

- **A14** (`b97c23d`) pins C1/C2: a ring-3 CPU exception (#GP, #DE) kills the task, not the kernel.
- **A15** (`90d520a`) pins V1: a bad user pointer to a syscall (`raw_syscall(log, cap 0, 0x1000, 16)`)
  faults in the kernel copy at CPL0 and the kernel logs `USER-COPY PF (killing caller)` + kills the
  caller instead of `halt_all_cores()`. `osdev test adv` 15/15.


## Audit 3 - 2026-07-13 (post-v0.4.0 re-audit)

Method: 2 parallel auditors (arch layer; core syscall/ipc/cap/task/memory/smp), each triaging A/B/C
against the north-star, then the lead **re-verified every confirmed finding against source** before
recording it (a subagent's "confirmed" is a lead, not a verdict - the "day my own test lied" discipline).
Motivation: a large surface landed since Audit 1/2 (dynamic core count / `MAX_CORES` removal, the
multi-method `hardware_reset`, the auto-repeat calibration, fbcon safe-area) plus the whole v0.4.0
userspace release - the audit's job is to prove the *new* code did not open a north-star gap and that
the Audit 1/2 fixes are still intact.

Result: **1 confirmed violation (MED), 2 latent hardening notes (LOW), all Audit-1/2 fixes verified
present-and-correct.** The core kernel came back clean; the one real finding is in the arch fault path.

### Confirmed violation (fix this)

> **Status (2026-07-13): K1 + K2 + K3 ALL FIXED on `feat/audit-kernel-and-userspace`.** K1: all five
> exception stubs now bound the asm THRE poll with an `ecx` spin counter (~1M, mirroring
> `SERIAL_THRE_NOLCK_CAP`), falling through to the breadcrumb write best-effort on timeout - so a ring-3
> fault on a wedged UART kills the task instead of spinning the core forever. `ecx` is safe scratch there
> (the stubs that need `rcx` reload it from the stack after the poll). K2: the BSP LAPIC id now gets the
> same loud xAPIC-ceiling check the APs have. K3: the APIC spurious vector 0xFF now routes to a dedicated
> `spurious_stub` (bare `iretq`) instead of `exception_halt`, so a spurious IRQ is a no-op not a wedge.
> Kernel + image build clean; identity 24/0, adversarial 15/0 (incl. A11/A12/A13 cap-gating).

#### K1. [MED] `kernel/src/arch/x86_64/boot.rs:1291,1336,1514,1592,1622` - arch-cpu (unbounded-loop / invariant 12)

**What.** The five naked exception stubs (`gpf_stub`, `pf_stub`, `exception_halt`, `exc_stub_noec`,
`exc_stub_ec`) each *open* with a raw-asm COM1 THRE poll as their absolute first instructions -
`mov dx,0x3fd; 88: in al,dx; test al,0x20; jz 88b` - which is **unbounded**. This is the exact scenario
`SERIAL_THRE_NOLCK_CAP` (boot.rs:719-725, "an absent or wedged COM1 must not hang a fault handler
forever") was added for, but that Audit-1 fix bounded only the *Rust* `serial_poll_thre`; the inline
asm polls at the front of each stub escaped it. **Verified** in source: `gpf_stub` (:1288-1296) loops
on `jz 88b` before writing its 'G' breadcrumb, and the sibling stubs match.

**Trigger.** Any ring-3 fault a service can raise at will (`div` by zero -> #DE, `cli`/`hlt` -> #GP, a
null deref -> #PF) on a machine whose COM1 LSR reads with THRE (bit 5) *persistently clear* - a
present-but-clock-gated/wedged UART. (An *absent* port reads 0xFF, bit 5 set, exits immediately, so this
needs present-but-wedged, the same hardware state the existing cap targets.) The faulting core then
spins forever with IF=0 - a silent single-core wedge from a ring-3 instruction, instead of killing the
task. Latent on the T630/Wyse (COM1 healthy), but a genuine invariant-12 gap.

**Fix.** Add a bounded spin counter to the asm poll in all five stubs (mirror `SERIAL_THRE_NOLCK_CAP`),
falling through to the breadcrumb write best-effort on timeout - exactly as the Rust helper already does.

### Latent hardening notes (LOW - real but no current trigger)

- **K2. [LOW] `kernel/src/arch/x86_64/ap_boot.rs:33`.** **FIXED (`feat/audit-kernel-and-userspace`).**
  The BSP was exempt from the loud xAPIC 8-bit LAPIC-id ceiling the APs get: it was stored without a
  range check while APs above 0xFF are excluded *loudly* (ap_boot.rs:46-64). A BSP with LAPIC id > 255
  (x2APIC-scale machine) would silently mis-route AP->BSP IPIs (`lapic_id & 0xFF` in `send_ipi`). The
  fix adds the matching loud check before storing the BSP LAPIC id (`bsp_lapic > XAPIC_MAX_LAPIC_ID ->
  loud "needs x2APIC" warning`). Exotic trigger; now consistent with the AP path and the loud-failure
  discipline (§26.7).
- **K3. [LOW] `kernel/src/arch/x86_64/boot.rs:323` (SVR=0x1FF) + `:1173` (IDT[0xFF]).** **FIXED
  (`feat/audit-kernel-and-userspace`).** The kernel programs LAPIC spurious vector 0xFF but routed that
  vector to the default `exception_halt` (hlt-loops the core). A spurious-vector delivery - which the SDM
  says to ignore-and-return - would wedge the whole machine. The fix gives 0xFF a dedicated `spurious_stub`
  (a bare `iretq`: no EOI, no register save, no swapgs - correct from either ring), wired in `init_idt`.
  A spurious IRQ is now a no-op, not a wedge (north-star: a non-fatal hardware event must never wedge the
  kernel, inv12). Identity 24/0 + adversarial 15/0 after the change.

### Verified present-and-correct (Audit 1/2 fixes + new code)

- **C1/C2** (ring-3 CPU-exception CPL discrimination): PRESENT. `init_idt` routes vectors 0-31 to
  CPL-discriminating stubs; `exc_dispatch`/`gpf_handler`/`pf_handler` kill the ring-3 task, halt only on
  ring-0. All gates DPL=0 except 0x80 (no ring-3 `int N` bypass).
- **V1** (user-copy fault fixup): PRESENT. Per-core `USER_COPY_ACTIVE`, set narrowly around the single
  `copy_nonoverlapping`; `pf_handler` clears it and `kill_current()`s on a CPL0 fault at a user VA.
- **V2** (partial-spawn cleanup): PRESENT. `own_endpoint` set right after registration; every post-endpoint
  error path routes through `cleanup_partial_spawn` (no leak toward the routing / endpoint-id panics).
- **V3** (scheduler Dekker re-check): PRESENT. `run()` and the timer `next!=prev` path CAS-claim `next`
  (Ready->Running, SeqCst), publish CORE_CURRENT, fence, re-load STATE, abort if Dead; kill completes the
  handshake. No mid-switch UAF.
- **C3** (runtime supervisor respawn): PRESENT. `poll_supervisor_respawn` re-arms PENDING on a transient
  Err instead of panicking; only boot-time `spawn_supervisor` is fatal.
- **`hardware_reset`** (new, multi-method): SAFE + TERMINAL. `io_delay` (10k) and 8042 wait (1M) bounded;
  the triple-fault fallback (zero-limit IDT + `int3` -> #DF -> shutdown) is unconditionally terminal; the
  trailing hlt-loop is an unreachable type-level backstop. Reboot is cap-gated (`REBOOT_RESOURCE`, granted
  only to shell/xhci/ehci) - no ambient reset authority.
- **Dynamic core count** (new, `MAX_CORES` removed): OOB-FREE. Arena width == cores started (identical
  `lapic_id <= 0xFF && != bsp` filter in `ap_count`/`start_all_aps`); AP exclusion is loud; every runtime
  per-core index guards `core < num_cores()` or uses a kernel-assigned id. No core-id OOB introduced.
- **New-syscall user-value paths**: all bounded before use - resource_mint/invoke/revoke (delegated-band
  range-checked, rights masked, id released on cap-table-full, badge kernel-set-only/unforgeable),
  LastRecvBadge (no user arg), AcquireSendCap (name len <=64, ACQUIRE_ANY-or-declared-peer gated),
  inspect_kernel core-id (guarded `>= num_cores() -> 0`), task_stat/task_caps slot (`>= MAX_TASKS`).
- **fbcon SAFE_PCT, rtc, serial (Rust helpers), iommu, pci, page_tables**: all arithmetic/loops bounded.

### Notes for record (not bugs)

- `cleanup_partial_spawn` does not `interrupt::route::unregister` a *failed driver spawn*'s IRQ lines
  (only driver spawns register IRQs, only a post-IRQ map failure leaves a stale entry). Bounded and
  self-correcting: `IRQ_TABLE` is a fixed `[Option; 256]`, a stale route delivers to a now-dead endpoint
  (harmless `None`), and a respawned driver overwrites it. No trigger to a fault. (Contrast driver
  *death*, which already unregisters - Audit-1 Item 2.)
- The B-set of correctly-loud panics (generation overflow, endpoint-id/routing exhaustion, liveness/
  shootdown watchdogs, W^X asserts) is unchanged from Audit 1/2 and re-confirmed as the defense.
