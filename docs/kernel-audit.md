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

