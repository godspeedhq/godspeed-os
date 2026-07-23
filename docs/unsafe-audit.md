# Unsafe Audit (§18.4)

`scripts/unsafe_check.py` runs on every CI push. It counts non-comment lines
containing the `unsafe` keyword per file and compares to the baseline table below.

**A PR that increases any file's count, or adds unsafe to a new file, fails CI
unless this file is updated in the same commit with a written SAFETY argument.**

`unsafe_check.py` scans `kernel/src/` (tracked against the inventory below) **and `services/`** (where
it fails on ANY `unsafe` line - §18.2 forbids service `unsafe`). The SDK's permitted-layer `unsafe`
(`syscall`, `mmio`, `dma`, `adversarial` - §18.1) is not inventoried here; each block carries a SAFETY
comment.

---

## 2026-07-23 - DWC2 control transfers via slave/PIO mode (feat/pi2-arm32)

The DWC2's internal DMA master never initiated a transfer on the Pi 2: across a dozen HW tests the channel
armed (ChEna set), the host framed (HFNUM advanced) and every config register read correct, yet
`GRSTCTL.AHBIdle` stayed 1 and `HCDMA` never advanced. Switched enumeration to **slave / PIO mode** - the
mode every working bare-metal Pi USB driver uses: DMA disabled (`GAHBCFG.DmaEn=0`), OUT data pushed
word-by-word into the NP TX FIFO and IN data popped from the RX FIFO after `GRXSTSP`, no bus-mastering.
This **removed** the DMA scratch static, the `flush_dcache` cache-coherency bracket, and the tick-driven
state machine, so `dwc2.rs` unsafe **shrank 8 -> 3** (only the `rd`/`wr` Device-MMIO accessors + the `nop`
`spin` remain; the slave-mode transfer code is all safe `rd`/`wr`). Enumeration is now synchronous (a
one-time bounded boot cost).

| File | Change | Why |
|------|--------|-----|
| `arch/arm/dwc2.rs` | 8 -> 3 (-5) | Slave/PIO rewrite dropped the DMA path: removed `flush_dcache` (DCCIMVAC + `dsb`, -2) and `poll_inner` + the two step handlers' `DMA`-static access (-3). Remaining: `rd`/`wr`/`spin`. |

---

## 2026-07-22 - DWC2 USB host bring-up, increment 1 (feat/pi2-arm32)

The Pi 2's USB is a Synopsys DesignWare USB 2.0 OTG (DWC2) core. `dwc2.rs` brings it up in host mode and
detects the attached device (the first step toward a USB keyboard): read the Synopsys core ID, soft-reset
the core, force host mode, power + reset the root port, report the connected device's speed. All new
unsafe is Device-mapped MMIO (the DWC2 register block is inside the already-Device-mapped peripheral
window) plus a `nop`-spin, both permitted `arch/`. QEMU (`-M raspi2b,usb=on -device usb-kbd`): core
GSNPSID=0x4f54294a, device detected + port enabled at full-speed.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/dwc2.rs` | new, 3 -> 8 | `rd`/`wr` (DWC2 Device MMIO 32-bit accessors) + `spin` (`nop` delay) for bring-up; increment 2 adds `flush_dcache` (DCCIMVAC + `dsb` - the DMA cache-coherency bracket, 2) and, in the tick-driven state machine, `poll_inner` + the two step-completion handlers' access to the `DMA` scratch static (identity-mapped physical buffer, 3). |
| `arch/arm/mod.rs` | 38 -> 39 (+1) | `uart_rx_poll` reads MPIDR to gate the USB `dwc2::poll()` to core 0 (it is the single writer of the DWC2 channel + DMA). |

## 2026-07-22 - ARM serial input works: idle + scheduler-context fixes (feat/pi2-arm32)

The core-0 block-path idle bug (typing did nothing) is fixed. `wait_for_interrupt` was a bare `wfi` that
never re-enabled IRQs, and the scheduler context was seeded with cr3=0 because the timer preempted the
bootstrap before `run(0)` seeded it; masking IRQs before arming the neutral scheduler closes that race.
New unsafe is the `clrex` before the serial-lock acquire (exclusive-monitor hygiene) and the `cpsie i`
added to the idle `wfi`, both permitted `arch/`.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 36 -> 38 (+2) | `clrex` before the `SERIAL_BUSY` compare-exchange (clear a stale ARMv7 exclusive-monitor that wedged the shell's 2nd console echo); GPIO14/15 -> ALT0 mux in `gpio_init_uart` so serial RECEIVE works, not just transmit. |

## 2026-07-16 - SEC-21 security fix (feat/hardening)

| File | Change | Why |
|------|--------|-----|
| `memory/allocator.rs` | 43 → 44 (+1) | **SEC-21:** new safe `zero_frame(phys)` helper (one `unsafe` `write_bytes` block via the HHDM alias) so the AllocMem syscall can zero a frame before it becomes user-readable, closing a cross-task info leak (`alloc_frame` returns un-zeroed frames). Permitted `memory/` layer with a SAFETY comment; keeping the `unsafe` here lets the caller (`syscall/dispatch.rs`, a grandfathered file) stay `unsafe`-free per §18.5. |

SEC-4 (bounds-checking the SDK `Dma`/`Mmio` wrappers) adds **0** to this inventory: the SDK's
permitted-layer `unsafe` is not tracked here (see the intro), and the change adds only safe `assert!`
bounds checks, not new `unsafe`. SEC-5 (fs subtree revoke) is `unsafe`-free service code.

## 2026-07-22 - HDMI framebuffer on the Pi 2, Phase 2: text console (feat/pi2-arm32)

`fbcon.rs` renders the serial stream onto the framebuffer (glyphs via the shared `noto-sans-mono-bitmap`
font), so the boot log + `gsh>` prompt appear on the TV; `pl011_write` mirrors to it under the same
SERIAL_BUSY guard. New unsafe is the framebuffer pixel writes + the single FBCON static, permitted
`arch/`. QEMU screendump: text renders (231 distinct colours in the top-left region = antialiased glyphs).

| File | Change | Why |
|------|--------|-----|
| `arch/arm/fbcon.rs` | new, 3 | `put_pixel` (device-mapped framebuffer store), the FBCON static in `init` + `put_byte` - the glyph renderer + cursor. |

## 2026-07-22 - HDMI framebuffer on the Pi 2, Phase 1 (feat/pi2-arm32)

Toward x86-parity local console. The ARM has no Limine to hand it a framebuffer, so `video.rs` asks the
VideoCore GPU for one via the mailbox property interface, `mmu::map_framebuffer` maps it Device, and a
solid fill proves the pipeline (QEMU screendump: a clean 1024x768 blue). New unsafe is the MMIO/mailbox
and framebuffer writes (video.rs) and the live-L1 mapping + TLB flush (mmu.rs), both permitted `arch/`.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/video.rs` | new, 4 | VideoCore mailbox (Device MMIO), framebuffer writes, MBOX static access - the framebuffer acquisition + fill. |
| `arch/arm/mmu.rs` | 6 -> 8 (+2) | `map_framebuffer`: write the framebuffer's Device sections into the live L1, then clean the D-cache + TLBIALL so the walker sees them. |

## 2026-07-22 - AP bring-up: park a mis-identified core (feat/pi2-arm32)

Real Pi 2: releasing core 3 brought up a core whose MPIDR read back as 0 - it registered as a SECOND
core 0, two cores ran scheduler::run(0), raced, and one crashed the boot (UNDEF halt). `ap_boot_main`
now parks any core that finds its own id ALREADY ready (a confused/duplicate release), so the system
boots reliably on the good cores. +1 unsafe: the `wfi` park loop.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 35 -> 36 (+1) | Park (`wfi`) a released AP whose id is already ready - the mis-identified-core guard. |

## 2026-07-22 - AP bring-up: vectors-first + barrier (feat/pi2-arm32)

On real HW core 3's bring-up intermittently faulted BEFORE it installed its vectors, so with VBAR still 0
it branched into low memory (an UNDEF at 0x618) and halted the boot. `ap_boot_main` now installs the
per-core vectors FIRST (before ACTLR.SMP/MMU) so any bring-up fault is REPORTED through the vectors
instead of wandering, plus a `dsb sy`/`isb` to synchronize with core 0's published boot state (SEC-25/28
weak-ordering hygiene). +1 unsafe: the barrier block.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 34 -> 35 (+1) | `dsb sy`/`isb` barrier at the top of `ap_boot_main` (weak-ordering sync before an AP relies on core 0's tables/arenas). `install_for_core` moved ahead of the MMU enable so bring-up faults are loud, not wild. |

## 2026-07-22 - ARM frame reclaim on task death (feat/pi2-arm32)

The ARM kill path reclaimed nothing (`reclaim_user_frames` was a `{ 0 }` stub) and the neutral kill path
`free_frame`d the page-table root - fine on x86 (root = a general frame) but on ARM the root is an ARENA
L1 slot, so it corrupted the frame bitmap (the `alloc_frame returned kernel-range frame` panic on the
first respawn). Real reclaim: `reclaim_user_frames` walks the dying task's L1/L2, `free_frame`s its USER
pages (AP[1:0] >= 0b10; distinguished from shared kernel hole-fill pages) and returns its own L2s to the
arena; the arenas gained per-slot `used` flags so freed L1/L2 slots are reused (a `free_frame`-of-root
would still corrupt, so the root goes back to the arena via `free_page_table_root`). QEMU-proven: 15
logger kill/restart cycles, 0 panic, 0 leak/exhaustion (`freed 76 frames` each, was `freed 0`).

| File | Change | Why |
|------|--------|-----|
| `arch/arm/page_tables.rs` | 25 -> 27 (+2) | `reclaim_user_frames` (walk L1/L2, free USER pages + L2 slots) and `free_page_table_root` (return the L1 root to the arena) - the two `unsafe fn` bodies that free a dead task's memory. `alloc_l1/l2` gained CAS-on-`used` + `free_l1/l2` (safe: atomic store + `addr_of`). |
| `arch/x86_64/page_tables.rs` | 47 -> 48 (+1) | `free_page_table_root` = `free_frame` (behaviour-identical to the old inline neutral root free), so the neutral kill path can be arch-neutral for both ISAs. |

## 2026-07-22 - Fault-survival on ARM: kill the faulting task, keep the kernel alive (feat/pi2-arm32)

The data/prefetch abort handlers went from report-and-halt to the x86 C2/A14/A15 property: a USER-mode
(PL0) fault kills just that task and reschedules; a kernel fault still reports and halts. `stub_dabt` /
`stub_pabt` now branch on the faulting mode (SPSR & 0x1f == 0x10) - no new `unsafe` (asm inside the
existing naked blocks). The +1 is the `wfi` guard loop in the new `arm_user_fault_kill`, which calls the
neutral `kill_current()` (sets the task Dead, `yield_current` switches to the next task). HW-verifiable;
QEMU-proven: `spawn greet` (rigged to read address 0) -> "user task faulted ... killing it; kernel
continues", and the shell + ping/pong keep running, no panic.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/exceptions.rs` | 23 -> 24 (+1) | `arm_user_fault_kill` (reached from the abort stubs in SVC mode on the faulting task's kernel stack) logs the kill loudly and calls `kill_current()`; the +1 is its `wfi` guard loop (kill_current does not return for a Dead task). |

## 2026-07-22 - SMP: cores 1-3 online on the Pi 2 (feat/pi2-arm32)

Bring the other three Cortex-A7s online. All new `unsafe` is in the permitted `arch/arm/` layer (§18.1),
each block SAFETY-commented. QEMU-verified: `smp: 4 cores ready`, services placed on cores 0+1, cross-core
IPC flowing, 0 faults. (Weak-memory-ordering hardening SEC-25..28 for real HW is a documented follow-up.)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 33 -> 34 (+1) | Core-3 lost-wakeup fix: a periodic re-`sev` in `smp_bringup`'s AP-ready wait re-arms the event line for a core that entered WFE just as the first SEV fired (its mailbox is still set, so it proceeds on the nudge). HW-proven: cores 1-2 came up but core 3 hung on release until this landed; now all 4 A7s come up on the Pi 2. The +1 is the `dsb`/`sev` block. (Diagnostic breadcrumbs used to find this were removed once understood.) |
| `arch/arm/mod.rs` | 26 -> 33 (+7) | `get_lapic_id` reads MPIDR (the core id - the linchpin for `current_core_id`); `ap_entry` (naked AP entry: HYP-drop, VFP, per-core stack); `ap_boot_main` (ACTLR.SMP + `mmu::enable_on_this_core` + vectors + timer, one asm block); `smp_bringup` (D-cache clean before release + per-core mailbox-3 SET write + `dsb`/`sev`). The `arm_ap_park` release loop is `global_asm!` (not counted as a Rust `unsafe` block). |
| `arch/arm/mmu.rs` | 4 -> 6 (+2) | Split `enable` into `build_tables` + `enable_on_this_core` (a `pub unsafe fn`, +1) so each AP loads the SAME L1 into its TTBR0; core 0 calls it too. The register-write blocks are unchanged; the +2 is the new unsafe fn wrapper and core 0's call site. |
| `arch/arm/exceptions.rs` | 21 -> 23 (+2) | `install_for_core(core)` gives each AP its OWN banked ABT/UND/IRQ/FIQ stacks (BSS `AP_MODE_STACKS`) instead of the shared linker-symbol stacks - two cores taking a timer IRQ at once would otherwise corrupt the one IRQ stack. The +2 are the raw-pointer stack-top computation and the VBAR/banked-SP asm block. |
| `arch/arm/irq.rs` | 10 -> 11 (+1) | `this_core()` reads MPIDR so the dispatch reads THIS core's `CORE_IRQ_SOURCE`/`CORE_TIMER_IRQCNTL` (`+4*core`), and `start_tick_ap` routes each AP's own timer. The +1 is the MPIDR read. |

## 2026-07-22 - The interactive shell on ARM (feat/pi2-arm32, increment 5)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mod.rs` | 23 -> 26 (+3) | Real console I/O: `console_write_bytes_gated` -> `pl011_write` (output); a PL011-RX -> input-ring path (`pl011_rx_drain`, `uart_rx_pop`, `uart_rx_poll`, `uart_rx_drain_now`, `console_push_byte`, `set_input_ready`/`input_ready`) so the shell reads serial input via ConsoleRead. The +3 unsafe are the three MMIO/ring blocks (`pl011_rx_drain`, `uart_rx_pop`, `console_push_byte`). |
| `arch/arm/exceptions.rs` | unchanged count | `stub_svc` now saves/restores the caller's USER-banked `SP_usr`/`LR_usr` around the syscall (asm inside the existing naked block, no new `unsafe`). A syscall that blocks (recv/console_read) switches to another USER task, which clobbers the shared USER bank; the shell, woken from `console_read`, resumed on the logger's shallow SP and faulted just above the stack top. Saving on the task's own kernel stack (like `stub_irq`'s trap frame) fixes it. |

**The interactive shell runs on ARM.** `gsh> ` prompt, reads serial input, echoes, and executes
commands: `help` prints the command list, `version` prints `GodspeedOS 0.7.0`. 0 faults. The
committed increments are unregressed (IPC 6600+ messages, supervisor bootstrap - both 0 faults - with
the `stub_svc` USER-bank change). New ARM boot: `arm-shell` (`sched_shell.rs`); the shell is built for
ARM (`arm_built += shell`). x86 unchanged (all changes in `arch/arm/`; the shell-spawn helper is
`#[cfg(target_arch = "arm")]`).

## 2026-07-21 - The NEUTRAL spawn works on ARM (feat/pi2-arm32, increment 4a)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/page_tables.rs` | 23 -> 25 (+2) | `finalize_service_address_space(cr3)` - the arch hook the neutral spawn calls after building a service page table: clones the kernel identity into it + cleans the D-cache (ARM has no shared higher-half kernel). The `unsafe fn` + its block. |
| `arch/x86_64/page_tables.rs` | 46 -> 47 (+1) | The x86 `finalize_service_address_space` is a `pub unsafe fn` no-op (kernel is shared higher-half); the empty `unsafe fn` is the +1. |
| `arch/arm/mod.rs` | 22 -> 23 (+1) | `syscall_slot` now returns a real per-core `PerCoreSyscallData` (an `addr_of_mut` unsafe) instead of null: the neutral spawn commits `is_user=true`, and `prepare_ring3_switch` writes through this pointer for every user task. Also added the safe `note_user_task` hook (`irq::mark_task_user`; no unsafe). |
| `task/mod.rs` | unchanged (7, at the grandfathered floor) | The neutral `spawn_service_with_config` gained ONE line calling `finalize_service_address_space`, and `arm_spawn_logger_neutral` (an ARM-only pub probe). The finalize call was folded into the existing `unsafe { TaskContext::new_user }` block so `task/`'s floor holds (§18.5) - no amendment. `scheduler::commit_task` gained a safe `note_user_task` hook call (no unsafe). |

**The neutral spawn machinery runs unchanged on ARM.** `task::spawn_service_with_config` - the exact path
the supervisor's spawn syscall uses (ELF load, user-stack + ctx-page map, kstack-pool alloc, cap
minting, ServiceContext write) - spawns the `logger` on ARM: `task: 'logger' spawned OK on core 0
(slot 0)` -> `logger: ready`. The two ARM-specific steps are now arch-seam hooks the neutral code calls
itself (both no-ops on x86, so x86 is byte-for-byte unchanged - verified it still compiles). This is the
foundation the supervisor stands on. Gated behind `arm-sched-spawn`.

## 2026-07-21 - Atomic syscalls + CLREX on ARM (feat/pi2-arm32, increment 3b hunt cont'd)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/irq.rs` | 9 -> 10 (+1) | `arm_irq_dispatch` reads the interrupted CPSR from the trap frame (`frame_sp + 68`) to implement **atomic syscalls**: skip timer preemption when a USER task is in SVC (a syscall), since preempting ARM kernel code mid-syscall corrupts (SPSR_svc + SVC-banked sp are shared). Gated on `ARM_TASK_IS_USER[slot]` (set by `mark_task_user`) so a *kernel* task running in SVC stays preemptible. The +1 unsafe is the frame read. |
| `arch/arm/context_switch.rs` | unchanged count | Added `clrex` at the top of `switch_context`: a voluntary switch does not implicitly CLREX like an exception entry, so a task switched out mid-`ldrex`/`strex` could leak the exclusive monitor and wedge a SpinLock. Inside the existing naked block, no new `unsafe`. |
| `arch/arm/mod.rs` | unchanged count | Doc-only: `syscall_slot` stays null (ARM tracks user tasks arch-locally, so the neutral `prepare_ring3_switch` never runs and never derefs it). |

**Status: the mid-syscall preemption FAULT is fixed (verified: no EXCEPTION over 30 s, `sched_demo`/
`sched_user` still rotate/preempt); the IPC still hangs on a residual corruption across the voluntary
syscall-context `switch_context`** (`block_and_reschedule`'s `slot` local, asserted `< MAX_TASKS` at
entry, reads back garbage at the tail). Full diagnosis in `sched_ipc.rs`. The SPSR-window fix (`stub_svc`
`cpsid i`) from the prior commit stays.

## 2026-07-21 - Cross-service IPC wiring (feat/pi2-arm32, increment 3b - WIP, blocked on a diagnosed bug)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/spawn.rs` | unchanged count | Refactored to expose `load_service_raw(elf, extra_caps)` + `map_stack_and_ctx` (shared with `sched_ipc`): load any service ELF, map its stack/ctx, reserve a slot, install `LOG_WRITE` + caller-supplied endpoint caps, leaving the ctx write + `fill_kernel_identity` to the caller. `load_logger_into_slot` now rides on it. No net `unsafe` change; `load_service_raw`'s block is the cap inserts. |
| `arch/arm/sched_ipc.rs` | 6 -> 9 (+3) | Rewritten from the 2-logger frame proof to a real `ping`->`pong` IPC attempt: create pong's endpoint (the `spawn_service_with_config` sequence - `alloc_endpoint_id` + register resource/routing/name), mint a RECV cap for pong and a SEND cap for ping, hand-build both `ServiceContext`s (`write_ipc_ctx`), and commit both as scheduled USER tasks. The +3 `unsafe` is `write_ipc_ctx` (raw ctx writes), `commit_user`, and the `halt` WFI. `build.rs` now builds `ping`/`pong` for `armv7a-none-eabi`. Gated behind `arm-sched-ipc`. |

**Status: the wiring is correct, the runtime is blocked on a diagnosed kernel bug.** Verified: the ctx
is wired correctly (a dump confirmed `send_peer_count=1, peer0.slot=1, name="pong"`), both services reach
PL0 and log (`ping: starting`, `pong: ready on core 0`). But once `ping` loops issuing syscalls
alongside a second running user task, a task's registers are corrupted and it jumps to a wild PC (garbage
syscall numbers, then a DATA ABORT to `0xfffffeae` from a PC in a data page). Bisected: `pong`'s blocking
`recv` alone is fine; `ping` ALONE (self-scheduling) loops forever clean; `ping`+`pong` together corrupts.
So the fault is in a **real cross-task `switch_context` reached from a *syscall* context** (yield/block) -
a path #1/#2/#3a never exercised (they switch only via the timer IRQ; #3a's two user tasks busy-loop on a
non-blocking `recv`, never yielding). The USER-banked `SP_usr`/`LR_usr` were ruled out (saving them in both
`stub_svc` and `switch_context` left the corruption unchanged, so those attempts were reverted). Next
leads: the AAPCS callee-saved contract across a syscall-context switch between two different user address
spaces (TTBR0 change), or SVC-stack nesting when the timer preempts a task mid-syscall. The 2-logger
banked-frame proof it replaced is preserved in commit `3e6cb3f`; the banked frame (`stub_irq`) stays, and
ping+pong both reaching PL0 still exercises it. The committed increments (#1/#2/#3a) are unregressed
(default `preempt selftest PASS 9/9/9`, sched-user `logger: ready`).

## 2026-07-21 - Two USER services at once: the banked-register trap frame (feat/pi2-arm32, increment 3a)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/exceptions.rs` | unchanged count | `stub_irq` now stacks the interrupted task's USER-banked `SP_usr`/`LR_usr` (`stmdb r0, {sp, lr}^` on save, `ldmia r0, {sp, lr}^` on restore) - the prerequisite for **more than one** user task. With one, nothing else touched the USER bank across a round trip; with two, task B's ring-3 execution would clobber task A's user stack unless it is saved per task. The extra instructions live in the existing naked block, so no new `unsafe`. Frame grew 16 -> 18 words. |
| `arch/arm/context.rs` | unchanged count | `TrapFrame` gains `usr_sp`/`usr_lr` (matching the 18-word layout) and `prepare_task` zeroes them for kernel tasks (their USER bank is unused). Struct/field change only, no new `unsafe`. |
| `arch/arm/page_tables.rs` | unchanged count | L1 arena 2 -> 8, L2 arena 16 -> 64: the boot loader selftest takes one L1 and each live service takes one, so two only left room for a single service. Sized (bounded static, §26.6.1) for the running service set - IPC pair, supervisor, shell. Constants only, no `unsafe`. |
| `arch/arm/sched_ipc.rs` | 0 -> 6 (new file) | Loads **two** logger instances as scheduled USER tasks (each its own address space) plus two kernel spinners, and runs them under `scheduler::run`. The isolation test for the banked frame; grows into real send/recv (increment 3b). The `unsafe` is the static-stack setup and the `new_user`/`new_kernel`/`commit_task` calls. Gated behind `arm-sched-ipc`. |

**What this proves.** Two independent USER services run concurrently in ring 3 under the scheduler and
both reach PL0 and issue their cap-validated syscall (`logger: ready` twice) with no corruption and no
fault. Were the banked frame wrong, the second user task's ring-3 execution would clobber the first's
`SP_usr` and one would fault - so "both ready, no fault, system live" is the proof the per-task
`SP_usr`/`LR_usr` save/restore is correct. This is the trap-frame foundation IPC stands on. Verified in
QEMU (`raspi2b`); the default image is unregressed (`preempt selftest PASS 9/9/9`).

## 2026-07-21 - A USER service runs through the scheduler, preemptively (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/context_switch.rs` | 14 -> 13 (-1) | `new_user` is now **real**: it builds a context whose first `switch_context` drops to PL0 via a new `user_entry_trampoline` (installs the user stack, fabricates a USR-mode SPSR with IRQs on, `movs pc`). The loud `user_mode_unimplemented` stub it replaced is deleted - net one fewer `unsafe`. `switch_context` also gained a `TLBIALL` on the TTBR0-change branch (SEC-26/27: an ARM address-space switch does not implicitly flush), inside the existing naked block (no new `unsafe`). |
| `arch/arm/page_tables.rs` | 21 -> 23 (+2) | `clean_invalidate_dcache_all` (set/way `DCCISW`) moved here from `spawn.rs` as the shared home for cache maintenance: it makes a service's page-table descriptors visible to the non-cacheable walker once at spawn, so `switch_context` needs no per-switch cache work. The `unsafe fn` + its asm block are the +2. |
| `arch/arm/sched_user.rs` | 0 -> 6 (new file) | Loads the logger as a scheduled **USER** task (its own TTBR0), commits it plus two spinning kernel tasks to the neutral `scheduler::run(0)`, cleans the D-cache once, and arms `NEUTRAL_SCHED`. The `unsafe` is the static-stack setup and the `new_user`/`new_kernel`/`commit_task` calls. Gated behind `arm-sched-user`. |
| `arch/arm/spawn.rs` | unchanged count | Refactored to expose `neutral_bootstrap` + `load_logger_into_slot` (shared with `sched_user`) and to call `page_tables::clean_invalidate_dcache_all` rather than a private copy. No net `unsafe` change. |

**What this proves.** A real GodspeedOS service (`logger`) runs *through* the neutral scheduler on ARM,
not entered directly: loaded into its own address space, committed to a task slot, and preempted in
ring 3 by the timer (its trap frame lands on its own kernel stack), while spinning kernel tasks are
round-robined around it. `logger: ready` is its cap-validated syscall, issued from PL0 under its own
TTBR0 - so the per-task page table, the `switch_context` TTBR0-swap + `TLBIALL`, and the one-shot
descriptor D-cache clean all hold end to end. Verified in QEMU (`raspi2b`): `logger: ready` once,
kernel tasks advancing past tick 3, no fault. **Single** user task by design: a second one needs
`stub_irq` to also stack the banked `SP_usr`/`LR_usr` (the next increment); with one, nothing else
touches the USER bank across the round trip. The default image is unregressed (`preempt selftest PASS
9/9/9`, `neutral surface PASS`).

## 2026-07-21 - Timer preemption via the neutral scheduler (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/irq.rs` | 8 -> 9 (+1) | `arm_irq_dispatch` now routes the timer tick to the neutral `timer_tick_from_irq` (the preemptive `switch_context` path) once `NEUTRAL_SCHED` is set, instead of the early `context.rs` demo scheduler. The +1 unsafe is the `timer_tick_from_irq` call. |
| `arch/arm/sched_demo.rs` | unchanged count | The demo tasks now **spin** (no yield) and arm `NEUTRAL_SCHED`, proving the timer preempts a non-cooperating task. |

**The mechanism, and why the same IRQ stub serves both paths.** The stub saves the full interrupted
frame on the task's kernel (SVC) stack and calls `arm_irq_dispatch`, which returns an `sp` the stub
adopts (`mov sp, r0`). The early demo scheduler returns a *different* task's frame to adopt. The
neutral `timer_tick_from_irq` instead does the `switch_context` INTERNALLY - it swaps `sp` to the next
task's kernel stack itself - so `arm_irq_dispatch` returns `frame_sp` unchanged, and after this task is
later resumed (`switch_context` unwinding back into the call), `frame_sp` again names THIS task's
frame, making the `mov sp` a no-op. One stub, two mechanisms.

**Non-yielding tasks are genuinely preempted**, proven by output interleaved mid-print (a tick caught a
task between arbitrary instructions). The boot `preempt_selftest` still uses the demo path
(`NEUTRAL_SCHED` defaults false) and still passes 9/9/9, so the default image is unregressed. This is
the preemption real services need (they block on `recv`, they do not yield).

## 2026-07-21 - Neutral scheduler runs tasks on ARM (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/sched_demo.rs` | 0 -> 6 (new file) | Commits three kernel tasks and enters the neutral `scheduler::run(0)`; it round-robins them (A->B->C->A...) via `pick_next` + `switch_context` + `yield_current`. Proves the neutral scheduler - the foundation the supervisor and every service stand on - runs on ARM. The `unsafe` is the static-stack setup, the `new_kernel`/`commit_task` calls, and the BootInfo construction for the neutral bootstrap. Gated behind `arm-sched-demo`. |

**Cooperative first, deliberately.** The tasks `yield` (a scheduling point), so this exercises the
scheduler's task table + `switch_context` without the timer-preemption rework - running the timer IRQ
on per-task kernel stacks so it can `switch_context` a *non-yielding* task - that real services need.
That is the next increment; this proves the layer beneath it. No per-task page tables (all tasks share
the kernel identity map), so a switch never changes TTBR0 and the D-cache dance from the service spawn
does not arise.

## 2026-07-21 - Minimal service spawn (feat/pi2-arm32)

Increment 6 groundwork: enough to load a real service, set up its task + capability, and run it at
PL0 issuing syscalls. All in the permitted `arch/` layer with SAFETY comments; no grandfathered floor
moves (the one neutral helper, `set_current_task`, is a **safe** fn - the atomic store is not UB - so
`scheduler.rs` stays at its floor).

| File | Change | Why |
|------|--------|-----|
| `arch/arm/spawn.rs` | 0 -> 7 (new file) | The minimal spawn: build a BootInfo, run the neutral `memory`/`percpu`/`capability` init, load the logger ELF, map its user stack + service-context page, reserve a task slot with a `LOG_WRITE` cap, clone the kernel into the service address space, switch TTBR0, and drop to PL0. |
| `arch/arm/page_tables.rs` | 19 -> 21 | `fill_kernel_identity`: clone the live kernel identity map into a service page table (whole-section where the service slot is empty; page-fill the L2 where the service tabled-over a kernel section, so kernel data sharing the ctx/code 1 MiB stays reachable). |
| `arch/arm/mod.rs` | 21 -> 22 | The `interrupts` module's `disable`/`enable`/`local_irq_save`/`restore`/`wfi` are now REAL (`cpsid`/`cpsie`/`wfi`), the ARM `read`/`write_user_bytes`/`validate_user_ptr` are real, `switch_to_boot_stack` sets SP, and ACTLR.SMP is set at boot. |

**MILESTONE REACHED: `logger: ready`.** A real GodspeedOS service, loaded from an ELF, runs
unprivileged (PL0) on 32-bit ARM under its own address space, and logs through a capability-checked
`svc` into the neutral dispatcher. `clean_invalidate_dcache_all` (a set/way `DCCISW` sweep, +1
unsafe) before the TTBR0 switch was the final fix: the kernel maps its memory as 1 MiB **sections**
but the service maps the shared 1 MiB as 4 KiB **pages**, and stale D-cache lines from the section
view made the cap-table spinlock's `LDREX`/`STREX` fail under the page view. Cleaning the D-cache
makes every line coherent before the walker and exclusive monitor see the new mappings, and the lock
acquires. Gated behind `arm-spawn-logger` so the default image still boots to the selftest halt; the
feature build runs the service to `ready`.

## 2026-07-21 - ARMv7 user mode / PL0 (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/usermode.rs` | 0 -> 15 (new file) | **A task runs UNPRIVILEGED for the first time.** Enters USR mode (PL0), runs a stub that cannot touch kernel memory, and has it `svc` back. The `unsafe` is: `enter_user` (the fabricated exception return that drops to PL0), `resume_boot` (restores the kernel context on the magic svc), the user stub, I-cache sync for the copied code, the ATS1CPUR/W unprivileged translation probes, and the frame copy/map in the selftest. |
| `arch/arm/page_tables.rs` | 19 (unchanged) | `l2_small_page` now encodes PL0 access from the `USER` flag: AP=0b11 (PL0 RW), 0b10 (PL0 RO), 0b01 (PL0 none) - the page's whole two-level security model. No new `unsafe`. |
| `arch/arm/exceptions.rs` / `syscall.rs` | unchanged counts | The SVC entry publishes the caller's SPSR (so a syscall can see its privilege), and the magic test syscall routes to `on_magic_svc`. |

**Entering USR mode is a fabricated exception return.** No `iret`: set SPSR to USR (IRQs enabled), set
LR to the entry PC, arrange the USR banked SP (via a brief system-mode switch), and `movs pc, lr` -
which copies SPSR->CPSR and LR->PC atomically, dropping privilege. The ARM analogue of x86's IRETQ.

**The proof of PL0 is the SPSR at the svc, not that the code ran.** The CPU records the caller's mode
in SPSR_svc; `SPSR.mode == 0x10 (USR)` is unforgeable evidence the stub executed unprivileged. The
selftest checks exactly that (observed 0x10), and separately probes the permission model with the
*unprivileged*-access translation ops (ATS1CPUR/W): user code is user-readable, user stack
user-writable, and a KERNEL page is NOT user-accessible - isolation, proven non-faulting. Getting back
out with no scheduler: `enter_user` saves the kernel context first; the magic svc restores it.

## 2026-07-21 - ARMv7 SVC syscall entry (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/syscall.rs` | 0 -> 5 (new file) | **The SVC syscall entry** - `svc #0` traps into the neutral `syscall_handler`. The `unsafe` is `arm_svc_dispatch` (forwards to the `unsafe` neutral handler) and the `issue_svc`/selftest asm. |
| `arch/arm/exceptions.rs` | 21 (unchanged) | The SVC vector went from report-and-halt to a real entry: save `LR_svc`/`SPSR_svc`, call the dispatcher, `movs pc, lr` to return restoring CPSR. No new `unsafe` block - the naked stub was already one. |

**Two ARM-specific things had to be right, and one was a bug.** (1) SVC targets SVC mode and the
kernel already runs in SVC, so `LR_svc`/`SPSR_svc` are saved *first thing* like a nested exception, or
the next `bl` clobbers the return address; done that way the entry works from a USR caller (real
tasks) and an SVC caller (the selftest) alike. (2) **The 32-bit ABI bug:** `syscall_handler` takes
`u64` parameters, and on 32-bit ARM each `u64` is a *register pair* (number in r0:r1, arg0 in r2:r3,
rest on the stack). Passing the four `r0-r3` values to a `u64`-parameter function read the arguments
shifted - it showed up as a wrong echo (7400 vs 7345). `arm_svc_dispatch` takes `u32`s (one register
each, matching r0-r3) and widens to `u64` for the neutral call; every syscall argument on this arch
(pointer, handle, length) fits in 32 bits, so the widening is loss-free. That widening is the seam the
SDK port will use.

**No user tasks yet (increment 3)**, and the real handlers touch per-task state, so the selftest
proves the *entry mechanism* through a test dispatch (a mix of all four args, so a correct result
proves each survived the mode switch) and leaves `syscall_handler` wired for when tasks arrive. A
second trap confirms the path is re-entrant.

## 2026-07-21 - Neutral frame allocator live on ARM (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/meminit.rs` | 0 -> 4 (new file) | **Wires the neutral `memory::init` on ARM** - the first shared (non-arch-layer) subsystem running on 32-bit ARM, and the prerequisite for per-task page tables and service spawn. Builds a `BootInfo` from the DTB memory map + linker kernel bounds, reserves the kernel image as a low region, and runs the neutral bitmap allocator. The `unsafe` is the `static mut MEM_REGIONS`/`BootInfo` construction, the `__fiq_stack_top` linker-symbol read, and reconstructing a `Frame` to free in the selftest. |
| `memory/allocator.rs` + 7 arch `page_tables` | guard relaxed + `PHYS_IS_IDENTITY` const | The allocator panicked on `hhdm == 0` ("HHDM offset not set"). That is true on x86/Limine but WRONG on ARM: the kernel runs identity-mapped, so hhdm=0 is the correct value (`hhdm + phys == phys` already addresses the frame). Fixed the boundary-correct way (`arch/CLAUDE.md`): each arch declares `page_tables::PHYS_IS_IDENTITY` (true on ARM, false elsewhere), and the guard only fires where a zero offset genuinely means "unset". x86 identity 24/24 confirms the shipping arch is unaffected. |

**`memory::init` fit the neutral allocator unchanged** because two ARM facts line up with what it wants:
`hhdm=0` works (identity map), and `protect_kernel_page_table_frames` - the one Limine-table-specific
step - already returns early when `hhdm == 0`, a clean no-op rather than a special case. Result on
hardware-shaped input: `frame allocator ready (946 MiB free)`, and the selftest allocates 8 distinct
page-aligned frames, checks the free count drops by 8, frees them, and checks it returns to baseline.

## 2026-07-21 - ARMv7 two-level page tables (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/page_tables.rs` | 0 -> 17 (new file) | **Real two-level 4 KiB page tables**, replacing the compile-only stub inline in `mod.rs`. `mmu.rs` gave 1 MiB sections; this gives an L2 table under an L1 entry, so individual pages carry their own permissions. The `unsafe` is: the TTBR0/TLB primitives (`invalidate_tlb_page` = TLBIMVA, `read`/`write_page_table_base`), the descriptor writes into the live and fresh L1/L2 tables, the static-arena table allocators, and the `ATS1CPR`/`ATS1CPW` translation probes the selftest uses. |

**The read-only proof needs no fault.** `ATS1CPW` runs a privileged-*write* address translation and
reports the result in `PAR.F` - so a read-only page returns "denied" for a write while `ATS1CPR`
(read) still returns its address. The selftest maps one page RW and one RO into the live tables and
checks: both translate for read, RW is writable, **RO is not**. The negative is the load-bearing one
(same discipline as the MMU and IOMMU selftests): "RW translates" only shows the L2 was built; "RO
refuses a write" shows the AP/APX permission bits are actually enforced. That is real per-page
protection, the thing 1 MiB sections could not give.

**The frame source is a bounded static arena, deliberately.** x86's `PageTable::new` pulls table
frames from the neutral `alloc_frame`, which needs `memory::init` + a real memory map - and that pulls
in Limine-shaped assumptions (`protect_kernel_page_table_frames`) that are a separate integration
step. So table memory is a fixed static arena here (§26.6.1), with the `alloc_frame` swap called out as
the one remaining seam. The *algorithm* - build an L2, point an L1 entry at it, encode the page with
its permissions - is the real one the neutral path will drive unchanged. `map_in_active_tables` fills a
currently-unmapped L1 slot (a VA in the gap between RAM end and the peripherals) rather than converting
a live section, so running code is never momentarily unmapped.

## 2026-07-21 - Neutral context-switch surface, real (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/context_switch.rs` | 0 -> 14 (new file) | **The real `arch::imp::context_switch` surface** the neutral scheduler imports: `TaskContext`, `new_kernel`/`new_user`, and the naked `switch_context`, plus a selftest driving a kernel task through that exact neutral API. The `unsafe` is the two naked asm fns (switch + first-entry trampoline), the constructors, `user_mode_unimplemented`, and the selftest's static/ptr manipulation. Replaces the compile-only stub that was inline in `mod.rs`. |
| `arch/x86_64/context_switch.rs` + 5 arch stubs | +1 line each (`TaskContext::ZERO`) | A neutral leak fixed by an arch primitive, not a special case (`arch/CLAUDE.md`). The scheduler built a zero context with a literal `TaskContext { rbx: 0, ... }`, naming x86 registers in neutral code - which does not compile once ARM's `TaskContext` is ARM-shaped. Each arch now exposes `const ZERO: Self`; the scheduler uses `TaskContext::ZERO`, naming no register. |

**Kernel-only integration, honestly scoped.** The neutral scheduler spawns **only** ring-3 tasks
(`new_user`, from ELF binaries) - it has no path that calls `new_kernel` - so `scheduler::run()`
end-to-end genuinely needs userspace, which ARM does not have (0-byte service placeholder). What *is*
provable kernel-only is the scheduler's core primitive: `TaskContext::new_kernel` + `switch_context`
driving an ARM kernel task, which the selftest exercises through the neutral types.

**The switch mirrors x86 semantics exactly, and a bug proved it.** Like x86, the ARM switch saves
callee-saved + `sp` + `lr` but **not** `cr3` (it only *loads* TTBR0). The first selftest faulted in a
loop with TTBR0=0 - because `SCHED_CTX.cr3` stayed zero from its `ZERO` init, and switching back
loaded it. That is the *exact* gotcha x86 documents at `scheduler.rs` ("seed the scheduler context's
CR3 ... switch_context never saves CR3, only loads it"); reproducing it confirms the semantics match.
The fix seeds `SCHED_CTX.cr3` with the live TTBR0, as the neutral `run()` does. `new_user` builds a
context that halts loudly if entered - ring-3 needs an SPSR return, per-task page tables, and SVC
syscalls, none of which exist yet, so a premature user spawn fails visibly rather than running undefined.

## 2026-07-20 - Device tree parsing: learn the memory map (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/dtb.rs` | 0 -> 6 (new file) | **Flattened Device Tree parsing.** Six SAFETY-commented blocks, all bounds-checked reads of the firmware-supplied blob: big-endian u32 reads, node-name comparison, and reading `DTB_PTR` itself. Every offset is checked against the blob's own declared `totalsize` before being walked - a corrupt header pointing outside the blob is exactly how a parser wanders into unmapped memory. |
| `arch/arm/mod.rs` | +1 asm site (no new unsafe block) | `_start` now stashes `r2` (the DTB pointer) into `r10` before the mode check clobbers `r0-r2`, and publishes it into `DTB_PTR` **after** the BSS zero - which would otherwise wipe it. |

**Why this stops being optional here.** Every layer so far tolerated a hardcoded `RAM_END`, copied
from what the firmware told Linux, with a comment admitting that was not how a real port should learn
it. That is fine while nothing depends on it, and stops being fine the moment the neutral kernel's
frame allocator does: a wrong constant hands out frames backed by memory that does not exist. The
firmware already knows the answer and passes it in `r2`.

**FDT is big-endian on a little-endian CPU**, so every u32 needs swapping - the most common way to get
nonsense from this format, hence a single `be32` rather than byte-swapping at each site. The parser is
deliberately minimal: find `/memory`, read `reg`, stop. It does not pretend to general
`#address-cells` handling it has not implemented.

A missing or unparsable blob falls back to the old constant but **announces it** (invariant 12),
because a silently wrong memory size becomes allocator corruption much later, far from its cause.
Note QEMU cannot exercise the real path here: `-device loader` sets the PC without emulating the
firmware's r0/r1/r2 handoff, so `DTB_PTR` is 0 there and only hardware tests the parse.

## 2026-07-20 - ARMv7 PREEMPTIVE switch (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/context.rs` | 4 -> 6 (+2) | **Preemptive switching.** Two further SAFETY-commented blocks: fabricating a trap frame on a fresh task's stack, and the WFI halt for a task that returns from its entry function. |
| `arch/arm/exceptions.rs` | 21 (unchanged) | `stub_irq` grew from a five-register handler into a full trap-frame entry: `srsdb` + `cps` + `push {r0-r12, lr}`, dispatch, `mov sp, r0`, `pop`, `rfeia sp!`. No new `unsafe` - the naked stub was already one block. |

**Cooperative and preemptive switching are genuinely different problems**, and the difference is
AAPCS. A cooperative switch happens inside a function call, so the compiler has already spilled the
caller-saved half and ten registers suffice. A preemptive switch is *forced* between two arbitrary
instructions with anything live, so the **entire** register file plus the resume PC and `SPSR` must be
captured.

**The ARMv7 obstacle is register banking**: on IRQ entry the CPU is in IRQ mode, where the interrupted
mode's `sp` and `lr` are banked away and unreachable. `srsdb sp!, #0x13` reaches across that by
pushing `LR_irq`/`SPSR_irq` onto the *SVC* stack; `cps #0x13` then stands on the interrupted task's
own stack to save the rest. The frame therefore lives on **the task's own stack**, which is what makes
a switch cheap - the state is already parked where it belongs, so switching tasks is switching `sp`
and nothing else. The dispatcher returns the frame to resume; returning a *different* pointer is the
entire mechanism of preemption. `rfeia sp!` restores PC and CPSR atomically.

`TrapFrame`'s field order mirrors the push order and is as load-bearing as `Context`'s; a mismatch
would resume tasks with scrambled registers, looking like random corruption far from the cause. A
fresh task is started by fabricating its frame rather than special-casing "never run" in the switch -
the same trick as `Context::prepare`, one layer down. `SPSR` is set with **IRQs enabled**: a task
started with them masked would run to completion and never yield, silently killing preemption with no
error anywhere.

The selftest runs three tasks that never cooperate and checks **all three** were scheduled - a switch
that always picked one task, or worked once then wedged, would still show *a* task running.

## 2026-07-20 - ARMv7 kernel context switch (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/context.rs` | 0 -> 4 (new file) | **Cooperative kernel-mode context switch.** The switch itself is a three-instruction naked fn (`stmia`/`ldmia`/`bx lr`); the remaining blocks are the ping-pong selftest driving it. Only ten registers are saved and that is not a shortcut: AAPCS makes `r0-r3`/`r12` caller-saved, so the compiler has already spilled anything live at the call site, leaving the switch responsible for `r4-r11`, `sp`, `lr` - the same division as the x86 side. |

`Context`'s field order is **load-bearing**: `stmia`/`ldmia` transfer in increasing register number
regardless of how the list is written, so the struct must read `r4..r11`, `sp`, `lr` under `repr(C)`.
Reordering the fields would silently restore registers into the wrong slots.

A fresh context is started by *fabricating* its `lr` as the entry point, so the ordinary restore path
starts it - no special case in the switch. The selftest checks the round trip rather than mere
arrival: the counter is incremented by the *other* context and read back, which only works if state
survives in both directions (a half-working switch that transfers control but corrupts registers is
the dangerous case).

**This is cooperative - called, not forced.** A preemptive switch from the timer IRQ must save the
*full* register file, because an interrupt can land between any two instructions with anything live.
That is the next increment. No address-space switch either: all contexts share the identity mapping,
and per-task `TTBR0` writes bring the SEC-26/27 TLB obligations with them.

## 2026-07-20 - BCM2836 interrupt controller / timer tick (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/irq.rs` | 0 -> 8 (new file) | **Routing the timer IRQ so the counter becomes a tick** - the prerequisite for preemption. Eight SAFETY-commented blocks: volatile read/write of the Device-mapped BCM2836 core-local block (routing + pending), `CNTP_TVAL` and `CNTP_CTL` writes to arm the timer, `CNTP_CTL` and `CPSR` reads for diagnostics, and `cpsie i` / `cpsid i` to unmask and mask IRQs. |
| `arch/arm/exceptions.rs` | 21 (unchanged) | The IRQ stub changed shape without changing its count: it now saves `r0-r3, r12, lr`, calls the dispatcher and **returns** via `ldm sp!, {r0-r3, r12, pc}^` (the trailing `^` restores CPSR from SPSR atomically). It is the only exception in the port that returns rather than halting. |

**The tick selftest caught a real routing bug, and the diagnostics located it precisely.** The first
version counted **zero** interrupts. The follow-up print made the cause unambiguous: `CNTP_CTL` read
`0x5` (ENABLE set, IMASK clear, **ISTATUS set** - the timer was firing), `CPSR` showed SVC mode with
IRQs unmasked, yet the core-local pending register read `0x0`. Timer firing + interrupts enabled +
nothing pending means the timer was raising a source nobody was listening to.

**Cause: `CNTP_*` addresses the secure OR the non-secure physical timer depending on the CPU's
security state, and those are two different interrupt sources** - `CNTPSIRQ` (bit 0) and `CNTPNSIRQ`
(bit 1). The Pi firmware enters an ARMv7 kernel in HYP (non-secure), so hardware raises bit 1; QEMU's
`raspi2b` stub passes through the secure monitor into *secure* SVC and raises bit 0. Routing only the
non-secure bit therefore worked on neither in the same image. The fix routes and accepts **both**,
exactly as `_start` accepts either HYP or SVC entry: one image, either security state, no assumption
left to be wrong about.

## 2026-07-20 - ARMv7 generic timer (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/timer.rs` | 0 -> 4 (new file) | **ARM generic timer + BCM2835 System Timer.** Four SAFETY-commented blocks, all side-effect-free reads: `CNTFRQ` (the firmware-programmed frequency), `CNTPCT` via `mrrc` into a register pair (the 64-bit counter, with an ISB so the read is not reordered), a volatile read of the System Timer's counter-low register, and `local_reg` reading the BCM2836 core-local block (timer control + prescaler). The last two are in ranges `mmu.rs` maps as Device memory. |

**ARM needs no timer calibration** - `CNTFRQ` reports the frequency architecturally, so the whole
x86 PIT-calibration apparatus (and the ~1 second-quantum bug it existed to fix on the T630) has no
counterpart here.

**But `CNTFRQ` is still cross-checked, because it is firmware-programmed rather than
hardware-discovered.** It is an ordinary read/write register that firmware is *supposed* to set;
firmware that forgets leaves it 0 or wrong, and every duration derived from it is then silently
wrong - surfacing much later as mysterious timing bugs. The Pi carries a second, independent clock
(the BCM2835 System Timer, fixed at 1 MHz by hardware), so the selftest measures one against the
other over 100 ms and compares the result with what `CNTFRQ` claims. That turns "the register says
19.2 MHz" into "two independent clocks agree on how long a second is". A zero `CNTFRQ` is reported
loudly and degrades to the System Timer rather than computing nonsense (invariant 12).

**The cross-check immediately paid for itself: on the Raspberry Pi 2, `CNTFRQ` is wrong by 19.2x.**
Hardware reports `CNTFRQ = 19200000` while the counter measurably advances at 1 MHz. The BCM2836
feeds the generic timer through a core timer prescaler (`0x4000_0008`) at `source * prescaler / 2^31`;
firmware programs `0x06AAAAAB`, which divides the 19.2 MHz crystal to **exactly** 1 MHz, and then
never updates `CNTFRQ` - so the register still advertises the undivided crystal. Trusting it would
have made every delay and every scheduler quantum wrong by 19.2x, with the symptom appearing far from
the cause. **QEMU cannot reproduce this**: it does not model the prescaler (both registers read 0) and
its `CNTFRQ` is truthful, so only hardware could have caught it. `timer_hz()` therefore returns the
**measured** rate, never `CNTFRQ`, and the selftest distinguishes a deviation *explained* by the
prescaler (a known board quirk, reported and continued) from an unexplained one (a real failure).

## 2026-07-20 - ARMv7 MMU (feat/pi2-arm32)

| File | Change | Why |
|------|--------|-----|
| `arch/arm/mmu.rs` | 0 -> 4 (new file) | **ARMv7 short-descriptor translation, 1 MiB sections.** Four SAFETY-commented blocks: filling the L1 table (`static mut`, boot-only, secondaries parked and MMU off so nothing is walking it); the enable sequence (TLB/BP/I-cache invalidate, DACR=client, TTBCR=0, TTBR0, then SCTLR.M, with the DSB/ISB pairs the ARM ARM requires); enabling caches afterwards; and `translate()`, which runs the CPU's own table walker via ATS1CPR and reads PAR. `translate` is deliberately safe to call on an address expected to be UNMAPPED - a failed walk sets PAR.F rather than raising an exception, which is what lets the selftest prove the table bounds anything. |

The MMU is the gate on task isolation, and it comes *after* the vectors on purpose: a bad mapping is a
translation fault, and without a vector table that fault is a silent hang rather than a printed
`translation fault (section) - NOT MAPPED`.

**The selftest checks a negative, not just a positive** (same reasoning as the x86 IOMMU selftest,
§22 Test 12): confirming that mapped addresses translate only shows the table is non-empty, so it also
confirms that an address outside every mapped range does **not** translate. The three checks are
mutually validating - a broken `translate()` that always failed would break checks 1-2, and a blanket
identity map would break check 3.

## 2026-07-20 - ARMv7 exception vectors (feat/pi2-arm32)

The 32-bit ARM port gains its vector table. All additions are in the permitted `arch/` layer with
`// SAFETY:` comments, so no §18.5 amendment is needed and no grandfathered floor moves.

| File | Change | Why |
|------|--------|-----|
| `arch/arm/exceptions.rs` | 0 -> 21 (new file) | **ARMv7 exception vectors.** Until this existed, ANY fault on ARMv7 was a silent lockup - no vector table means the CPU jumps to whatever sits at address 0 and wanders off, which is exactly the silent failure invariant 12 forbids. The count is dominated by the eight one-instruction vector entries plus their `naked` stubs (each loads the exception kind, the LR-adjusted faulting PC, and DFSR/DFAR or IFSR/IFAR, then branches to a common reporter). `install()` holds one block that programs VBAR and primes the ABT/UND/IRQ/FIQ banked stacks; `trigger_test_fault()` holds one deliberately-unsound read behind the `arm-fault-test` feature, which is the ARM twin of the x86 A14/A15/C2 adversarial fault tests - a fault path never observed firing is not evidence that it works. |

**ARMv7 trap worth recording: FIQ mode banks r8-r12.** The first version of `install()` stashed the
caller's CPSR in `r12`, walked through FIQ mode to set its banked stack, then restored CPSR from
`r12` - but inside FIQ that register name refers to a different physical register holding garbage, so
the restore loaded a nonsense mode and reset the CPU. The symptom was oblique (the boot banner
printing twice, and VBAR reading back as `0x00000000` instead of the table address). The fix carries
nothing across a mode switch: VBAR is programmed first while still in SVC, and the walk ends by
naming SVC explicitly rather than restoring a saved value.

## 2026-07-16 - SEC-1 / SEC-18 security fixes (feat/hardening)

Two HIGH findings from the security audit (`docs/security-audit.md`), both fixed with `// SAFETY:`-
commented blocks in the permitted `arch/` layer (no §18.5 amendment needed):

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/boot.rs` | 104 -> 107 (+3) | **APIC-timer calibration:** `read_apic` (a volatile MMIO read, the counterpart of the existing `write_apic`) and `pit_calibrate_apic_ticks_per_10ms` (an `unsafe fn` measuring the LAPIC timer against a PIT-gated 50 ms window, mirroring the proven TSC calibration). Needed because the periodic period is `init_count * divisor / f_apic` and `f_apic` is machine-dependent: the old hardcoded count gave ~100 ms on QEMU but ~1 s on the T630, 100x the intended 10 ms quantum. Same PIT ports and stuck-hardware bail-out as the TSC path; the timer LVT is masked during the measurement so it cannot deliver an interrupt. |
| `arch/x86_64/boot.rs` | 100 -> 104 (+4) | **Phase 2a (tickless idle, `docs/power.md` §14):** two safe wrappers, `rearm_idle_timer` (arm the TSC-Deadline at `IDLE_QUANTUM_MULT` quanta, ~1 s, so an idle AP wakes ~100x less often) and `rearm_quantum_timer` (restore the ~10 ms preemption quantum on an idle wake). Each wraps one `arm_tsc_deadline_now` / `rearm_tsc_deadline` call in a SAFETY-commented block, guarded by `TSC_DEADLINE_MODE`. Deliberately **safe `fn`s in the arch layer** so the neutral `scheduler.rs` calls them without `unsafe` - §18.5's rule that new `unsafe` lives in a permitted layer rather than growing a grandfathered file (scheduler.rs stays at its floor of 37). Each helper handles BOTH timer modes, so each has two SAFETY-commented blocks: the TSC-Deadline arm, and a LAPIC `APIC_TIMER_INIT` write for the periodic path (the T630 runs periodic, where the hardware auto-reloads and the initial count is the only way to slow the tick). |
| `arch/x86_64/boot.rs` | 98 → 100 (+2) | **SEC-18:** new `broadcast_nmi_all_but_self` (a `pub unsafe fn` + one `unsafe` ICR-write block) so the panic path stops every core, not just the caller. Models the sibling `broadcast_ipi_all_but_self`; NMI delivery mode (ICR bits 10:8 = 0b100) reaches a core even while it spins IF=0 on a lock. `idt[2]` is also repointed to `exception_halt` (a same-file IDT re-wire, no new `unsafe`). |
| `arch/x86_64/mod.rs` | 35 → 36 (+1) | **SEC-18:** `halt_all_cores` now calls `boot::broadcast_nmi_all_but_self()` before its `cli`+`hlt`, so a panic on one core halts the whole machine (§6.2 / §19). The +1 is that `unsafe { boot::... }` call block. |

**SEC-1** (the freed-CR3 UAF fix in `task/scheduler.rs`) adds **0** here: its Dekker-handshake edits to
`yield_current` / `block_and_reschedule` live inside those functions' pre-existing `unsafe` blocks.

## 2026-07-12 - userspace audit M8: probe made `unsafe`-free; `unsafe_check.py` now scans `services/`

`probe` (the §22 adversarial/fuzz/chaos test harness) held raw-SYSCALL `asm!` plus deliberate ring-3
faults (null read, non-canonical read, divide-by-zero) - `unsafe` in a userspace service, forbidden by
§18.2, and INVISIBLE because `unsafe_check.py` scanned only `kernel/src/`. Both gaps closed:

- The `unsafe` moved to a new **audited SDK module `sdk/rust/src/adversarial.rs`** (§18.1 amendment):
  safe `fuzz_syscall` (wraps the ABI `raw_syscall`; the kernel validates every fuzzed call) and safe
  `fault_null_read` / `fault_noncanonical_read` / `fault_divide_by_zero` (the deliberate faults, each
  SAFETY-commented). `probe` calls these safe wrappers and is now `unsafe`-free.
- `scripts/unsafe_check.py` now also scans **`services/`** and FAILS on any service `unsafe` line -
  mechanically enforcing §18.2 and catching any future regression (the M8 blind spot). As a bonus,
  `fuzz_syscall` uses the SDK's `ud2` trap, removing probe's old raw `syscall` instruction (a latent
  AMD-GX-420GI stall hazard).

Verified: `osdev test adv` 15/0 (A10 fuzz, A14 faults, A15 bad-ptr all pass through the SDK wrappers);
`unsafe_check.py` passes with `services/` scanned. Kernel inventory unchanged (471 lines).

## 2026-07-11 - core count fully dynamic (MAX_CORES ceiling removed)

Every remaining fixed `[_; MAX_CORES]` per-core structure became a boot arena sized to the machine's
real core count, and the `MAX_CORES` sanity ceiling was deleted. All changes are in the permitted
`arch/`/`smp/` layers, each block carrying a `// SAFETY:` comment.

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/mod.rs` | 35 → 33 (-2) | The fixed `AP_ID_BUF: [u32; MAX_CORES-1]` staging buffer (2 single-threaded-boot `unsafe` writes) is gone: `start_all_aps` already walks Limine's live `cpus()` slice directly, and the new `ap_count()` counts it on demand, so no AP list is staged. Net -2. |
| `arch/x86_64/syscall_entry.rs` | 14 → 15 (+1) | `PER_CORE_SYSCALL` moved from a `[_; MAX_CORES]` `.data` array to a `PerCoreMut` boot arena, with a `BSP_SYSCALL` bootstrap slot for the pre-arena window (the BSP sets its syscall GS in `init_bsp`, before the allocator). The +1 is `syscall_slot`'s `addr_of_mut!(BSP_SYSCALL)` fallback; the arena's own `unsafe` lives in `smp/percpu.rs`. |
| `smp/ipi.rs` | 23 → 25 (+2) | The TLB-shootdown ack bitmask was a fixed `PerCore<[AtomicU64; MAX_WORDS]>` (`MAX_WORDS = ceil(MAX_CORES/64)`); it is now two FLAT `num_cores * ceil(num_cores/64)` `AtomicU64` arenas (ACK + EXPECTED), so the per-initiator mask WIDTH scales with the real core count. The +2 is the `ack_word`/`exp_word` accessors (`&*base.add(initiator*wpc + word)`); the arenas are carved by `percpu::alloc_atomic_u64_slice`. |
| `smp/percpu.rs` | 6 → 8 (+2) | New `alloc_atomic_u64_slice(n, init)` - carves a flat `[AtomicU64; n]` from the frame allocator (2 blocks: `ptr::write` init loop + `from_raw_parts`) for the dynamic-width shootdown masks, which no fixed `PerCore<[_; K]>` can size. Plus `PerCoreMut::initialised()` (safe). |

Net across the four: +3. `MAX_CORES` is deleted entirely - nothing is a fixed per-core array. The only
ceiling left is a genuine hardware one: the xAPIC IPI destination field is 8-bit, so a core with LAPIC
id > 255 is excluded LOUDLY (§26.7) until the APIC layer gains x2APIC. Validated: identity 24/24, adv
15/15, QEMU boot 1..128 cores + a 72-core 2-word-shootdown restart, arenas carve for 260 cores.

---

## 2026-07-11 - per-core user-copy arenas (RAM-sized, not [_; MAX_CORES])

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/syscall_entry.rs` | 15 → 14 (-1) | The V1 per-core user-copy state (`USER_COPY_ACTIVE`, and the 1 MiB `USER_READ_SCRATCH` = `[[u8; 4096]; MAX_CORES]`) moved from fixed `[_; MAX_CORES]` statics to boot-sized `PerCore`/`PerCoreMut` arenas (sized to the cores Limine reported, §26.6.1) - so per-core memory scales to the machine, not a 256-core ceiling. The count DROPS by one: `read_user_bytes`'s `addr_of_mut!` on the static scratch became the safe `PerCoreMut::as_mut_ptr` accessor (all the arena's `unsafe` lives in `smp/percpu.rs`). The two `copy_nonoverlapping` + one `from_raw_parts` blocks are unchanged. Removes ~1 MiB of fixed `.bss`. Boot-validated across QEMU -smp 1..16 + identity 24/24 + adv 15/15. |

---

## 2026-07-11 - dynamic (RAM-sized) frame bitmap

| File | Change | Why |
|------|--------|-----|
| `memory/allocator.rs` | 37 → 44 (+7) | The frame bitmaps are no longer fixed static `[u8; N]` arrays; they are sized to the machine's actual RAM at boot and carved from RAM, reached via the HHDM. The +7 is the raw-pointer machinery that replaces the (safe-indexed) static arrays: the `bitmap()` / `kpt()` slice accessors (`slice::from_raw_parts_mut` over `BITMAP_PTR`/`KPT_PTR`, 2 blocks), and in `init_from_map` the pointer publish + `KPT_PTR = BITMAP_PTR.add(bitmap_len)` + `write_bytes` zeroing of the carved region + the `hhdm==0` guard. Each carries a `// SAFETY:` note; the region is HHDM-mapped RAM reserved before any alloc and only ever touched under `ALLOC_LOCKED`. Permitted memory layer. Net effect: 64 MiB of fixed `.bss` (at the 1 TiB static cap) replaced by a bitmap of `RAM / 16 KiB` (e.g. 14 KiB on a 256 MiB box, 4 MiB on 64 GiB), sized dynamically - validated across 256 MiB..64 GiB in QEMU + identity 24/24. |

---

## 2026-07-14 - aarch64 Phase 0: isolate arch asm behind `arch::imp` primitives

The arch-boundary seal (docs/aarch64.md) moved every inline-asm operation out of the arch-NEUTRAL layers
and behind `arch::imp` primitives, so `unsafe` asm consolidated INTO the permitted arch layer and OUT of
the neutral files. New primitives (each one `// SAFETY:`-commented): `page_tables::{read_page_table_base,
write_page_table_base, invalidate_tlb_page}`, `interrupts::{local_irq_save, local_irq_restore}`,
`mod::switch_to_boot_stack`.

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/page_tables.rs` | 41 → 46 (+5) | CR3 read/write + `invlpg` primitives (the MMU-base + TLB seam). |
| `arch/x86_64/mod.rs` | 33 → 35 (+2) | `switch_to_boot_stack` (the boot stack-pointer seam; `#[inline(always)]`). |
| `arch/x86_64/interrupts.rs` | 21 → 22 (+1) | `local_irq_save` (the irq-save half; `local_irq_restore` calls `enable_interrupts`). |
| `memory/allocator.rs` | 44 → 43 (-1) | CR3-read asm replaced by `arch::imp::read_page_table_base`. |
| `smp/ipi.rs` | 25 → 23 (-2) | CR3 reload + `invlpg` + rflags save/restore replaced by `arch::imp` primitives. |
| `smp/spinlock.rs` | 9 → 5 (-4) | irq-save/restore asm replaced by `arch::imp::local_irq_save/restore` (no-op stub in the host lib). |

Follow-on same day - **IPI-send extraction**: `smp/ipi.rs` held the last APIC MMIO in a permitted-but-not-arch layer (the ICR programming for `send_ipi` + the shootdown broadcast). Moved to `arch/x86_64/boot.rs` as `send_ipi_to_lapic(lapic_id, vector)` + `broadcast_ipi_all_but_self(vector)` (+ `apic_wait_icr_idle`); `smp/ipi.rs` now resolves core->LAPIC and holds the neutral shootdown PROTOCOL only, calling the arch seam for the send. `smp/ipi.rs` 23 -> 17 (-6, incl. the removed `read/write_apic_reg` helpers + ICR consts); `arch/x86_64/boot.rs` 92 -> 98 (+6). `smp/ipi.rs` is now APIC-MMIO-free; arch owns ALL hardware MMIO. Identity 24/0 (9A cross-core IPC + shootdown exercise the moved paths).

Net: neutral-layer asm now ZERO (enforced by `scripts/arch_boundary_check.py`, CI-wired); the arch
layer is the sole home of `unsafe` asm, as §18.1 intends. `task/scheduler.rs` + `main.rs` asm was
removed too but their `unsafe` blocks (other ops) stayed, so their counts are unchanged. Identity 24/0.

---

## 2026-07-13 - kernel-audit-3 fix: spurious-interrupt stub (K3)

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/boot.rs` | 90 → 92 (+2) | **K3 (kernel-audit-3): dedicated APIC spurious-interrupt handler.** The LAPIC spurious vector 0xFF was routed to the default `exception_halt`, so a spurious IRQ (a normal, rare hardware event the SDM says to ignore-and-return) would wedge the whole machine. New `spurious_stub` (`#[unsafe(naked)] unsafe extern "C" fn` + `naked_asm!("iretq")`) gives 0xFF a return-without-EOI handler - the exact naked-stub pattern of the sibling `ipi_wake_stub`, no register save / no swapgs needed because it touches nothing. The +2 is the `#[unsafe(naked)]` attribute + the `unsafe extern "C" fn`. Permitted arch layer; carries a doc comment explaining soundness. |

---

## 2026-07-11 - kernel-audit fixes: user-copy fault guard (V1) + exception-handler backfill

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/syscall_entry.rs` | 13 → 15 (+2) | **V1 (kernel-audit-2): user-copy fault guard.** `read_user_bytes` no longer returns a borrowed raw-user slice; it copies the user bytes into a per-core kernel scratch under a `USER_COPY_ACTIVE` guard and returns a slice into the SCRATCH (so no caller ever dereferences raw user memory). The net +2 is: `read_user_bytes` now has 3 blocks (`addr_of_mut!` on the per-core scratch, `copy_nonoverlapping` for the guarded copy, `from_raw_parts` for the returned slice) vs 1 before; `write_user_bytes` keeps its 1 `copy_nonoverlapping` block. Each has a `// SAFETY:` comment. The guard makes a fault on a range-valid-but-unmapped user pointer recoverable (pf_handler kills the caller) instead of a whole-machine halt. Permitted arch layer. |
| `arch/x86_64/boot.rs` | 84 → 90 (+6) | **Backfill (not this change):** reconciles the C1/C2 exception-kill handlers added earlier this session by `af74086` (`gpf_stub` / `gpf_handler` / `exc_stub_noec` / `exc_stub_ec` / `exc_dispatch` - the ring-3 CPU-exception discriminator), which did not bump this table. All in the permitted arch layer, each block carries a `// SAFETY:` comment. **V1's own pf_handler change adds 0 here** - its user-copy-fault branch is safe code (calls to `current_core_id` / `user_copy_active` / `clear_user_copy_active`) inside the existing print `unsafe` block. |

---

## 2026-07-10 - fast fbcon blits for the 4K Wyse 5070 + drift reconcile

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/fb.rs` | 3 → 5 (+2) | Fast blit path for a dense (4K) panel. `fill_rect` writes a solid rectangle as contiguous per-row runs; `draw_glyph`'s fast 32bpp path writes each glyph output row as one contiguous run of aligned `u32` stores. The old per-pixel byte loop crawled repainting ~6.6M pixels/scroll on the Wyse's 3840x2160 panel. Both bounds-check the whole rect/cell ONCE against the reported geometry (cols/rows are sized so cells fit) then write the run unchecked, so write-combining coalesces the stores - the same raw-framebuffer-write pattern as the existing `put_pixel`/`clear`, in the permitted arch layer, each with a `// SAFETY:` comment. There is no safe route: writing Limine's linear framebuffer is a raw-pointer store, and a bounds-checked `&mut [u32]` would defeat the purpose (a compare per pixel is the very overhead removed). |
| `arch/x86_64/boot.rs` | 80 → 84 (+4) | **Reconcile only:** pre-existing drift accumulated since 2026-06-08 by the feat/networking bring-up (H1 IOMMU / NIC / PCI / AHCI, merged to main) that did not update this audit. All in the permitted arch layer; count corrected here, per-block detail is a backfill owed by that work. |
| `arch/x86_64/mod.rs` | 34 → 35 (+1) | **Reconcile only:** pre-existing drift from feat/networking (merged to main); permitted arch layer, count corrected, per-block detail backfill pending. |

## 2026-06-08 - fbcon scroll without VRAM read-back

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/fb.rs` | 4 → 3 (−1) | `scroll` no longer `core::ptr::copy`s the framebuffer up in place (which *read* uncached/WC VRAM - ~130 ms/line on the T630, the fbcon perf trap behind the "40× respawn"). It now shifts a RAM char-grid shadow and repaints from it - write-only via `draw_glyph`/`put_pixel` - so the block is gone. |

Reduction only; locks in the lower count. The three remaining blocks (`clear`,
`put_pixel`, `wc_flush`) keep their `// SAFETY:` comments. Hardware-verified
(T630): pixel-correct after thousands of scrolls; spawn 0.906 s → 9.9 ms.

> **Note.** This same day also reconciled 3 files the earlier H4b/H4 hardening
> merges left unaccounted (`page_tables.rs 25→35` permitted; `main.rs` and
> `task/mod.rs` held at their floors 2 and 7 by the clip) - see the entry below.

---

## 2026-06-08 - H4 hardening reconcile, **grandfathered floors held (no amendment)**

The W^X-remap (H4a/H4b) and kstack-guard (H4) work that merged earlier this session
added `unsafe` (all `// SAFETY:`-commented in source) without updating this audit. It
*briefly* raised two grandfathered floors; that was then **clipped back** so the
grandfathered counts return to their long-standing floors and **no §18 amendment is
needed**. The hardening's page-table `unsafe` now lives in the permitted arch layer,
where §18.1 says page-table manipulation belongs.

| File | Net | Layer | What |
|------|-----|-------|------|
| `arch/x86_64/page_tables.rs` | 25 → 35 (+10) | permitted | `entry_for_va`/`walk` PTE-walk + `unmap_active_4k` + `harden_hhdm_nx` (now a safe `fn`) + new `unmap_4k_strided` (the kstack guard-unmap loop, moved here from `task/`). Permitted-layer growth, allowed with SAFETY comments + this entry. |
| `main.rs` | 2 → 2 (no change) | grandfathered | `install_kstack_guards` / `harden_hhdm_nx` are now **safe `fn`s** (their preconditions are boot-ordering, not UB - same shape as `memory::init`/`smp::init`), so the call sites need no `unsafe`. |
| `task/mod.rs` | 7 → 7 (no change) | grandfathered | `install_kstack_guards` is now a safe `fn` whose guard-unmap delegates to `page_tables::unmap_4k_strided` (arch); the static-pool-address `unsafe` is centralised in `kstack_pool_base()` and reused by `free_kstack`, so the net count is unchanged. |

**Why this is better than amending (the clip).** `unsafe fn` is for memory-safety
preconditions whose violation is *UB*; `harden_hhdm_nx` / `install_kstack_guards` have
only *boot-ordering* preconditions (calling them out of order wedges boot - a liveness
bug, not UB), exactly like the already-safe `memory::init` / `smp::init`. Marking them
safe is both more honest and removes the call-site `unsafe`. The genuinely-unsafe work
(CR3 reads, PTE writes, the page unmap) stays in `unsafe {}` blocks **inside the
permitted arch layer** (§18.1). Net: the security hardening landed with **zero**
grandfathered growth. Hardware-verified on the T630 (guard pages install; W^X holds).

---

## 2026-06-04 - idle-halt (cool when idle) + introspection holds-check reconcile

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/interrupts.rs` | 12 → 13 (+1) | `wait_for_interrupt` gains a `sti; hlt` branch so ARAT-capable cores halt (run cool) instead of spinning; the no-ARAT branch keeps the legacy `sti`-only spin. |
| `arch/x86_64/boot.rs` | 79 → 81 (+2) | `cpuid_arat_supported` (`unsafe fn` + `__cpuid(6)`) - detects whether the LAPIC timer survives a C-state, gating the halt. |
| `task/scheduler.rs` | 36 → 37 (+1, grandfathered) | reconciles `current_task_holds_resource` - the §3.1 introspection holds-check (mirrors the existing grandfathered `current_task_lookup_cap`: reads `TASK_CAP[cur].assume_init_ref()`). Added with the introspection gate; the audit count was not bumped then - corrected here. A single read-only line for a security gate, same pattern as the lines already grandfathered in this file. |

All blocks carry `// SAFETY:` comments. The `hlt` is ARAT/TSC-Deadline-gated, so on
hardware without an always-running timer it never executes (no regression).

---

## 2026-06-03 - USB/xHCI stack (boot-verified, T630)

Branch `feat/usb-keyboard`. The userspace USB keyboard stack (§12) added unsafe
in the permitted arch + memory layers (the driver *service* itself is unsafe-free
behind the SDK's audited `Mmio`/`Dma` wrappers - §18.1).

| File | Change | Why |
|------|--------|-----|
| `arch/x86_64/pci.rs` | **new, 5 lines** | PCI config mechanism #1 port I/O (`outl`/`inl` + `config_read32`) to locate the xHCI controller. |
| `arch/x86_64/mod.rs` | 33 → 34 (+1) | `console_push_byte` pushes a USB-decoded key into the COM1 RX ring (`uart_rx_push`) so keystrokes reach the shell's `ConsoleRead`. |
| `memory/allocator.rs` | 29 → 32 (+3) | `alloc_contiguous(n)` - bitmap scan for a physically-contiguous run, for the driver's DMA arena. |

All blocks carry `// SAFETY:` comments in source. SDK `mmio.rs`/`dma.rs` unsafe
lives outside `kernel/src/` (the §18.1-amended SDK hardware/ABI layer) and is not
counted by `scripts/unsafe_check.py`, which scans `kernel/src/` only.

---

## 2026-05-31 - static-analysis + unsafe-audit pass (boot-verified, T630)

Full write-up: `milestones/testing/static-analysis-audit.md`. Branch
`verify/static-analysis-unsafe-audit`, commit `d276566`.

| Area | Result |
|------|--------|
| Policy violation | **Fixed** - `unsafe` removed from `ipc/` (§18.1); moved to `SpinLock::ZEROED` in `smp/spinlock.rs`. |
| Safety / correctness lints | **0** - 11 unnecessary `unsafe`, 11 `static mut` refs (→ `addr_of!`), 14 fn-item→int casts, 6 no-op `mem::forget`. |
| Cruft removed | orphaned `page_fault_handler` + `INTERRUPTED_*` statics. |
| Inventory | reconciled below - 302 lines / 23 files, passes clean; `task/scheduler.rs` 37 → 36 (under floor). |
| Kernel warnings | 104 → 57 (rest intentional unwired architecture). |
| Hardware | boots clean on T630, cross-core ping/pong to 83k+ msgs, zero `#PF`/panic. |

---

## Policy (§18)

`unsafe` is permitted only in:

| Permitted layer | Path |
|---|---|
| Architecture | `kernel/src/arch/` |
| Memory | `kernel/src/memory/` |
| Capability table | `kernel/src/capability/` |
| SMP | `kernel/src/smp/` |

**All other locations are outside policy.** The files marked `grandfathered` in
the table below contain unsafe that pre-dates this audit. Their counts are frozen:
they may decrease (fix welcome) but may not increase. New unsafe in those files
requires a policy amendment in `CLAUDE.md §18` before CI will accept it.

When you add an `unsafe` block anywhere:

1. Add `// SAFETY: <argument>` on the line immediately above it in the source.
2. Increase the count for that file in the inventory table below.
3. Add a SAFETY argument entry under that file in the **Entries** section.
4. Both changes must land in the same commit; CI checks the count.

---

## Inventory

Counts are non-comment lines containing the `unsafe` keyword.
CI script: `scripts/unsafe_check.py` - parses the table between the markers.

<!-- unsafe-inventory-start -->
| File (kernel/src/) | Count | Layer |
|---|---|---|
| arch/aarch64/mod.rs | 23 | permitted |
| arch/arm/exceptions.rs | 24 | permitted |
| arch/arm/context.rs | 6 | permitted |
| arch/arm/context_switch.rs | 13 | permitted |
| arch/arm/dtb.rs | 6 | permitted |
| arch/arm/irq.rs | 11 | permitted |
| arch/arm/meminit.rs | 4 | permitted |
| arch/arm/mmu.rs | 8 | permitted |
| arch/arm/video.rs | 6 | permitted |
| arch/arm/fbcon.rs | 4 | permitted |
| arch/arm/dwc2.rs | 3 | permitted |
| arch/arm/page_tables.rs | 27 | permitted |
| arch/arm/sched_demo.rs | 6 | permitted |
| arch/arm/sched_user.rs | 6 | permitted |
| arch/arm/sched_ipc.rs | 9 | permitted |
| arch/arm/spawn.rs | 8 | permitted |
| arch/arm/syscall.rs | 5 | permitted |
| arch/arm/usermode.rs | 15 | permitted |
| arch/arm/timer.rs | 4 | permitted |
| arch/arm/mod.rs | 39 | permitted |
| arch/loongarch64/mod.rs | 23 | permitted |
| arch/riscv32/mod.rs | 23 | permitted |
| arch/riscv64/mod.rs | 23 | permitted |
| arch/s390x/mod.rs | 18 | permitted |
| arch/x86_64/ap_boot.rs | 2 | permitted |
| arch/x86_64/boot.rs | 107 | permitted |
| arch/x86_64/context_switch.rs | 11 | permitted |
| arch/x86_64/fb.rs | 5 | permitted |
| arch/x86_64/interrupts.rs | 22 | permitted |
| arch/x86_64/ioapic.rs | 8 | permitted |
| arch/x86_64/iommu.rs | 74 | permitted |
| arch/x86_64/mod.rs | 36 | permitted |
| arch/x86_64/page_tables.rs | 48 | permitted |
| arch/x86_64/pci.rs | 19 | permitted |
| arch/x86_64/rtc.rs | 1 | permitted |
| arch/x86_64/syscall_entry.rs | 15 | permitted |
| capability/table.rs | 7 | permitted |
| memory/allocator.rs | 44 | permitted |
| memory/frame.rs | 1 | permitted |
| memory/mod.rs | 1 | permitted |
| memory/page.rs | 1 | permitted |
| smp/ipi.rs | 17 | permitted |
| smp/mod.rs | 1 | permitted |
| smp/percpu.rs | 8 | permitted |
| smp/placement.rs | 1 | permitted |
| smp/spinlock.rs | 5 | permitted |
| interrupt/route.rs | 1 | grandfathered |
| loader.rs | 2 | grandfathered |
| main.rs | 2 | grandfathered |
| syscall/dispatch.rs | 2 | grandfathered |
| task/mod.rs | 7 | grandfathered |
| task/scheduler.rs | 37 | grandfathered |
<!-- unsafe-inventory-end -->

**Permitted total:** 394 lines across 22 files  
**Grandfathered total:** 53 lines across 6 files  
**Grand total:** 447 lines across 28 files

> **2026-06-28** (branch `hardening/dma-reserve-pool`). **Audit reconciliation** - three permitted-layer
> files drifted (each line already carrying a `// SAFETY:` comment; the counts just weren't bumped as the
> work landed). `arch/x86_64/pci.rs` 17 → 19: `clear_bus_master` + `set_bus_master`, the PCI bus-master
> quiesce on DMA-driver kill/spawn that cures the max-carnage DMA-after-free (commit `ffe1a0f`).
> `memory/allocator.rs` 32 → 37: one from the page-table reclaim guard (`phys_in_ram`, commit `b9dbc4c`)
> and four from the DMA permanent-reserve net (§12) added on this branch - `alloc_dma_arena` (the reserving
> allocator + its public wrapper + the table-full undo) so a driver's DMA arena is never recycled into a
> page table. `smp/spinlock.rs` 7 → 9: a second `without_interrupts` guard from the per-core shootdown work.
> New grand total: 447 / 28 files (also corrects the prose totals, which had drifted ~5 low vs the inventory sum).

> **2026-06-22** (branch `fix/unsafe-audit-reconcile`). **Audit reconciliation** - caught up four
> drifted files and shrank one back under its floor. Permitted-layer count catch-ups (all `arch/`,
> each line already carrying a SAFETY comment - no policy issue, the counts just weren't bumped as the
> work landed): `arch/x86_64/interrupts.rs` 13 → 21 (USB MSI ISR plumbing), `arch/x86_64/pci.rs`
> 15 → 17 (MSI-X table mapping), and the previously-unlisted file `arch/x86_64/ioapic.rs` (+8, IOAPIC
> MMIO register reads/writes for legacy-INTx routing). `smp/spinlock.rs` 5 → 7 (the `without_interrupts`
> cli/sti added for the kstack-lock irqsafe fix). **`task/scheduler.rs` 40 → 37 - back at floor, NO
> §18.5 amendment:** the 3 file-as-capability (§7.10) accessors that had drifted it over -
> `current_task_endpoint`, `set_last_recv_badge`, `take_last_recv_badge` - were converted from
> `static mut` (`TASK_ENDPOINT`, `TASK_LAST_BADGE`) to `AtomicU64`, making them `unsafe`-free. The
> grandfathered floor stays 37 and there are still **no** grandfathered-floor amendments. New grand
> total: 433 / 28 files.

> **2026-06-13** (branch `feat/persistence`). **ATA PIO / `hw_pio` retired** - the
> AHCI (MMIO+DMA) backend replaced ATA PIO (the T630's SSD is AHCI-only). Reverts the
> 2026-06-12 addition below: `arch/x86_64/mod.rs` 38 → 34 (the `port_in8/16`,
> `port_out8/16` wrappers removed; `inb`/`outb` stay - used by serial + reboot), and
> `capability/hw_pio.rs` deleted (−3). Back to 413/27. The `PortRead`/`PortWrite`
> syscalls and the SDK `pio.rs` (not kernel-audited) are gone too.

> **2026-06-12** (branch `feat/persistence`). Persistence Phase 1 (ATA PIO block
> driver, docs/persistence.md §5). `arch/x86_64/mod.rs` +4 (permitted): safe
> public port-I/O wrappers `port_in8/16` + `port_out8/16` (the `in`/`out` asm,
> isolated in the arch layer; callers validate the port first). New file
> `capability/hw_pio.rs` +3 (permitted): the per-task `hw_pio` grant store
> (`set`/`clear`/`allowed`) - placed in the capability layer **on purpose**, so
> the per-task port-range state does not grow the grandfathered `unsafe` floor in
> `task/` (§18.5). `task/scheduler.rs` and `syscall/dispatch.rs` gained **no**
> `unsafe` (they call the safe wrappers / the capability-layer functions).

> **2026-06-10** (branch `feat/iommu-dma-confinement`). New file `arch/x86_64/iommu.rs`
> (+60, permitted): the H1 AMD-Vi IOMMU work. Phase 0 (+18) is ACPI-table reads
> (RSDP → RSDT/XSDT → IVRS) through the HHDM. Phase 1 (+42) is the IOMMU control
> interface and translation setup: uncached MMIO register read/write, device-table
> /command-buffer/event-log allocation and DTE writes, the 4-level I/O page-table
> builder/translator/free, and command-buffer invalidation. Every block carries a
> `// SAFETY:` argument that the target is a kernel-mapped IOMMU structure (MMIO
> window, device table, command buffer, or I/O page table) and the access is in
> bounds. All hardware `unsafe` is contained here behind the safe wrapper
> `confine_device()`; `task/mod.rs` calls it without any new `unsafe` (its
> grandfathered floor of 7 is unchanged). See the `arch/x86_64/iommu.rs` entry below.

> **Reconciled 2026-05-31** (branch `verify/static-analysis-unsafe-audit`). The
> permitted-layer growth since the prior baseline is from the AMD GX-420GI ring-3 /
> TSC-Deadline-APIC / COM1 work that landed on `main` (boot.rs, mod.rs, interrupts.rs,
> ipi.rs, allocator.rs). `smp/spinlock.rs` +1 is the new `ZEROED` const (below).
> Reductions: the static-analysis pass removed unnecessary `unsafe` blocks
> (ap_boot, boot, mod, scheduler) and the orphaned `page_fault_handler` /
> `INTERRUPTED_*` diagnostics (interrupts.rs net still up from the AMD work).
> **`task/scheduler.rs` is back to 36** - under its grandfathered floor again.

---

## Entries

Each entry documents WHY an unsafe block is sound. Entries are grouped by file.
Files with thorough existing `// SAFETY:` comments in source reference them here.
Files lacking source comments are noted with `(SAFETY comment missing in source)`.

New entries must be added in the same commit as the unsafe block they cover.

---

### arch/x86_64/ap_boot.rs

Unsafe in this file: AP trampoline entry, AP boot identity mapping, and calling
`ap_main` after the long-mode switch. All three are sound because the trampoline
runs before any Rust invariants apply; the stack is valid; identity mapping holds
for the trampoline duration and is torn down by the kernel immediately after.

---

### arch/x86_64/boot.rs

Largest unsafe surface in the kernel. Covers: BSP init (GDT/IDT/TSS per core),
APIC MMIO mapping and register writes, serial I/O port access, TSS RSP0 reads
and writes, paging init, and IPI delivery. All operations are sound because
they target fixed hardware addresses verified against the Limine memory map, or
operate on per-core structures indexed by a valid `core_id` bounded by
`MAX_CORES`. APIC MMIO is mapped once before any AP comes up.

One additional `unsafe {}` block (count +1): `write_apic(apic_virt, APIC_TPR, 0x00)`
in `init_local_apic` - zeroes the Task Priority Register so all interrupt
vector classes (including `WAKE_RECEIVER` at 0xF0) are accepted. Sound because
`apic_virt` is established by the preceding `map_in_active_tables` call within
the same function; `APIC_TPR` offset is within the mapped 4 KiB MMIO page.
`// SAFETY:` comment present in source.

---

### arch/x86_64/context_switch.rs

Stack construction for new kernel and user tasks. `new_kernel` and `new_user`
write a synthetic initial register frame to a freshly allocated kernel stack
pointer. Sound because the stack buffer is owned exclusively by the new task
and not yet visible to the scheduler.

---

### arch/x86_64/fb.rs

Framebuffer text console (Phase 1 boot output, §11.4). Five blocks; four write
to Limine's linear framebuffer at `base + y*pitch + x*bpp`:
- `clear`: `write_bytes(base, 0, height*pitch)` - fills the whole buffer.
- `put_pixel`: writes `bpp` bytes (one aligned `u32` store on the 32bpp fast path) at a
  bounds-checked offset (`x<width`, `y<height`).
- `fill_rect`: fills a `w x h` rectangle clamped to `width`/`height`, writing each row as a
  contiguous run (aligned `u32` stores on the 32bpp path). Sound: the clamped rect stays inside the
  mapped `height*pitch` region; `x*bpp`/`pitch` are 4-aligned on the 32bpp path.
- `draw_glyph` (fast 32bpp path): writes each glyph output row as one contiguous run of `cw` aligned
  `u32` stores. Sound: it first checks the whole cell `[x0,x0+cw) x [y0,y0+chh)` lies inside the
  framebuffer (cols/rows are sized so cells fit; otherwise it falls back to the checked `fill_rect`),
  so the unchecked run stays within `height*pitch`, and `x0*4`/`pitch` are 4-aligned.

Sound because the framebuffer is the region Limine mapped and sized
(`height*pitch` bytes), it lives in the higher half (PML4 256-511) that every
address space inherits via `PageTable::new`, so it is valid for writes for the
system lifetime; every offset is bounds-checked against the reported geometry.

`scroll` previously held a fourth block - an in-place `copy`/`write_bytes` that
shifted the framebuffer up one glyph row. That `copy` *read the framebuffer back*
(uncached/WC VRAM, ~130 ms/line on the T630); it was replaced by a RAM char-grid
shadow that scroll shifts and repaints from, leaving `scroll` entirely safe
(write-only via `draw_glyph` → `put_pixel`). Net **4 → 3** (2026-06-08).

The remaining `wc_flush` block is a single `SFENCE` instruction. The framebuffer
is mapped write-combining (Limine HHDM default), so the FB lock's atomic release
does not order the WC store buffer - a scroll's pixel stores on one core could
flush after the next line's first glyph drawn on another core, erasing it. Each
`put_byte`/`put_bytes` issues `SFENCE` before releasing the lock so its WC stores
are globally visible in order. Sound because `SFENCE` only orders stores and has
no memory or privilege effects.

---

### arch/x86_64/interrupts.rs

IRQ dispatch and CR2 read on page fault. Sound because the IRQ handler runs at
known IDT vector; CR2 is only read inside the page-fault handler where it is
valid.

Three additional `unsafe {}` blocks (count +3): `enable_interrupts` (STI),
`disable_interrupts` (CLI), and `wait_for_interrupt` (STI+HLT). All three are
ring-0 privileged instructions with no memory effects; the callers are
responsible for the context invariants (e.g., interrupts were disabled before
calling `wait_for_interrupt`). `// SAFETY:` comments present in source.

One additional `unsafe {}` block (count +1): `send_eoi` - writes the local APIC
EOI register via `boot::apic_send_eoi`. Sound because the APIC is mapped before
any IRQ fires and EOI register writes are idempotent with no memory-safety
implications. Exposes APIC EOI as a safe call site in `interrupt/route.rs` (§12)
without increasing the grandfathered count there.

One additional `unsafe {}` block (count +1): `fire_test_irq` - calls
`interrupt::route::deliver(irq)` after disabling interrupts and before
re-enabling them. Sound because IF=0 satisfies `deliver`'s calling convention;
the surrounding `disable_interrupts()` / `enable_interrupts()` calls are safe
arch functions; EOI inside `deliver` is idempotent outside a real hardware
interrupt. Used only by the `FIRE_IRQ` COM2 control command (§22 Tests IR1A/IR1B).

---

### arch/x86_64/ioapic.rs

IOAPIC programming for legacy-INTx interrupt routing. 8 unsafe lines, each with a SAFETY comment -
uncached MMIO access to the IOAPIC index/data window (write the register selector, then read/write the
32-bit data register) to read the IOAPIC id/version and program redirection-table entries that route a
legacy IRQ line to a CPU vector. Permitted `arch/` layer (direct hardware access, §18.1). The file was
not previously in the inventory; its count was correct in source, just unaudited - added 2026-06-22.

---

### arch/x86_64/iommu.rs

AMD-Vi IOMMU detection (H1 Phase 0). All 18 unsafe lines are raw reads of
firmware ACPI tables - the RSDP, the RSDT/XSDT, and the IVRS - through the HHDM.
The helpers `read_bytes`, `read32`, `read64` are `unsafe fn`; `detect` calls them
at every step. Each block is sound because:

- The RSDP virtual address comes from Limine's `RsdpRequest`, which points at a
  table Limine keeps mapped in the HHDM; the signature is checked before any
  further read.
- Every subsequent table is reached only through a physical pointer that lives
  inside an already-validated parent table, converted to a virtual address via
  the HHDM (`hhdm + phys`), which Limine maps for all usable + ACPI memory.
- Each read stays within the table's own length field (`sdt_len`, `ivrs_len`),
  and the IVHD walk advances by the block's self-reported length and stops on a
  zero length, so it cannot run off the end or loop forever.

Detection only - no behaviour change, no writes, no device programming. The
results are published in two atomics (`IOMMU_PRESENT`, `IOMMU_MMIO_BASE`).

**Phase 1 (translation setup), +42.** The remaining unsafe in this file programs
the IOMMU and builds translation structures. Grouped:

- `mmio_read64` / `mmio_write64` - volatile access to the IOMMU MMIO control
  registers, which `bringup` maps uncached (PCD|PWT) at their HHDM alias before
  any access. Offsets are compile-time constants within the mapped 0x4000 window.
- `setup_structures` / `write_dte` - allocate the device table (2 MiB contiguous),
  command buffer, and event log from the frame allocator, zero them through the
  HHDM, and write DTEs. All writes target the freshly-allocated, HHDM-mapped
  structures; the DTE index is a 16-bit BDF, in bounds of the 64K-entry table.
- `io_walk_or_alloc` / `io_map_page` / `io_translate` / `free_io_table` - the
  4-level AMD-Vi I/O page-table builder, read-only translator, and frame reclaim.
  Each level VA is the HHDM alias of a present/just-allocated table; indices are
  masked to 9 bits (< 512), so every read/write is in bounds of a 4 KiB table.
  `free_io_table` frees only the page-table frames (reached top-down from a root
  that `release_device` has already detached from the device), never the leaf
  arena pages.
- `invalidate_device` - writes 16-byte commands into the mapped command-buffer
  ring at the hardware tail offset (masked to the 4 KiB ring) and rings the tail
  register; serialised by `CMD_LOCK`.
- `drain_event_log` - reads decoded fault events from the mapped 4 KiB event-log
  ring (head < 0x1000) and advances the head register; bounded per call so it is
  safe to invoke from the timer-tick path (`control::process_pending`). Also
  recovers from event-log overflow (disable EvtLogEn, RW1C the status bit, reset
  head/tail, re-enable) - all writes to valid IOMMU control/status/pointer regs.
- `confine_device` / `confinement_selftest` / `release_device` - orchestrate the
  above; the raw work they do directly is zeroing a freshly-allocated page table,
  an `sfence` (no memory-safety effect, orders prior stores), and (on release)
  reverting a DTE before freeing the now-unreachable I/O page table.

`confine_device`, `release_device`, `event_log_state`, and `bringup` are the safe
entry points;
all callers outside the arch layer (e.g. `task/mod.rs`) use them without `unsafe`.
`// SAFETY:` comments present on every block.

---

### arch/x86_64/mod.rs

Serial port init and COM2 init via `outb`/`inb`, `cli`/`hlt` in the halt loop,
and the `init` call chain into `boot.rs`. Sound because serial ports are
exclusively owned by the kernel at these call sites; `cli`/`hlt` is the correct
halt sequence; all callers are within the single-threaded BSP init path.

One additional `unsafe {}` block (count +1): `console_push_byte` calls
`uart_rx_push(b)` to enqueue a USB-keyboard-decoded byte into the COM1 RX ring,
then wakes any task blocked in `ConsoleRead`. Sound because the RX ring is a
single-logical-producer buffer (the timer-ISR UART drain and the xHCI driver's
`ConsolePush` syscall both run on Core 0's serial path); the push is a bounded
ring write with head/tail wrap. `// SAFETY:` comment present in source.

---

### arch/x86_64/page_tables.rs

HHDM offset reads and writes, PTE reads and writes via `read_volatile`/
`write_volatile`, `map_in_active_tables` (reads CR3, walks and modifies the
active page table), and `reclaim_user_frames` (walks a dead task's page table
after the TLB shootdown has completed). All are sound because: HHDM offset is
written once before any AP starts; PTE access goes through the HHDM which is
valid after `set_hhdm_offset`; `map_in_active_tables` holds the frame allocator
lock for the duration; `reclaim_user_frames` is called only after TLB shootdown
acknowledgment from all cores.

Ten additional unsafe lines (count 25 → 35) from the W^X / guard-page hardening
(H4a/H4b, 2026-06-07/08):
- `entry_for_va` / `walk` / `read_entry` chain - read-only PTE walk used to probe
  a VA's mapping (PTE/large-page) for the W^X audit and the kstack-guard install.
- `unmap_active_4k(virt)` (`unsafe fn` + CR3 read + present-entry walk + clear PTE
  + `invlpg`) - marks a 4 KiB page non-present; no-ops on a large page (fails safe).
- `unmap_4k_strided(base, stride, count)` - a **safe `fn`** that unmaps the low page
  of each kstack slot via `unmap_active_4k`; the guard-unmap loop moved here from
  `task/` (§18.1 - page-table work belongs in arch) so it adds no grandfathered
  unsafe. Boot-ordering contract (BSP, before APs).
- `harden_hhdm_nx()` - a **safe `fn`** (CR3 read + HHDM subtree walk OR-ing NX into
  every present PDPT/PD/PT, then CR3 reload) that flips the HHDM `NO_EXEC`, closing the
  Limine-mapped RWX direct map (§3.12). Boot-ordering precondition (after `smp::init`),
  not UB - hence safe; the CR3/PTE work inside stays `unsafe {}`.

Six further unsafe lines (count 35 -> 41) from the `alloc_mem` reclaim-leak fix
(2026-06-23, surfaced by `chaos mem-pressure`):
- `free_phys_frame(phys)` (an `unsafe fn` + one `unsafe { free_frame }`) - frees one
  physical frame by address during task-death teardown.
- `reclaim_user_frames` now frees each leaf / PDPT / PD / PT frame INLINE via
  `free_phys_frame` (four call sites) instead of collecting into the fixed 512-entry
  `ReclaimBuffer`, whose `push` silently DROPPED - i.e. LEAKED - every frame past 512 (a
  32 MiB `alloc_mem` task leaked ~30 MiB on every kill, violating §10.5 / §26.7). The walk
  itself is unchanged; only "collect into a buffer" became "free inline". Sound for the same
  reason as the original: called only after the TLB shootdown has been acknowledged by all
  cores, so no core's page-walker can reach a freed frame.

All sound for the same reason as the rest of the file: HHDM is live, the tables are
reached via present entries rooted at the live CR3-referenced PML4, and these run
BSP-only at boot before APs execute from the affected region. `// SAFETY:` comments
and `# Safety` docs present in source for every block.

---

### arch/x86_64/pci.rs

PCI configuration-space access via legacy mechanism #1 (port `0xCF8` address /
`0xCFC` data), used once at boot to locate the xHCI USB host controller and
record its MMIO base + IRQ (§12). Five unsafe lines:
- `unsafe fn outl` / its inner `unsafe {}` block - 32-bit `out dx, eax` port write.
- `unsafe fn inl` / its inner `unsafe {}` block - 32-bit `in eax, dx` port read.
- `unsafe {}` in `config_read32` - pairs an `outl(address)` then `inl(data)`.

Sound because port I/O is ring-0 and these ports are the architecturally fixed
PCI config registers, owned exclusively by the kernel during single-threaded BSP
boot (the scan runs before any AP or task exists); the address dword is
constructed from bounded bus/dev/func/offset values with the enable bit set per
the mechanism-#1 spec. `// SAFETY:` comments present in source.

Three additional unsafe lines (+3) for the EHCI BIOS→OS handoff
(`ehci_bios_handoff`): the `unsafe {}` in `config_write32` (paired `outl(address)`
+ `outl(data)`, same discipline as `config_read32`), the `map_in_active_tables`
call mapping the EHCI MMIO page to read HCCPARAMS, and the `read_volatile` of
HCCPARAMS. Sound for the same reason - ring-0 BSP boot, architecturally fixed
ports, the MMIO page mapped uncached before the single aligned read.

Seven more (+7) for the xHCI BIOS→OS handoff (`xhci_bios_handoff`): xHCI's legacy
support lives in MMIO (not PCI config), so this maps the xHCI MMIO (16 pages,
uncached), reads HCCPARAMS1 for the xECP, then walks the MMIO extended-capability
list - `read_volatile`/`write_volatile` of USBLEGSUP (claim OS ownership, poll
for BIOS release) and USBLEGCTLSTS (disable firmware SMIs). Each access is within
the just-mapped 64 KiB MMIO window at a bounded offset (< 0x10000), during
single-threaded BSP boot. All carry `// SAFETY:` comments.

---

### arch/x86_64/rtc.rs

MC146818 CMOS real-time-clock read via the legacy index/data ports (`0x70` /
`0x71`), used to answer `InspectKernel` query 11 (wall-clock date/time) for the
shell's `date`/`time` commands (§12). One unsafe line:
- `unsafe {}` in `cmos_read` - wraps an `out dx, al` (select register) followed by
  an `in al, dx` (read its value); the two asm blocks are not `pure`, so their
  order is preserved.

Sound because port I/O is ring-0 and these are the architecturally fixed CMOS
ports; only a register number (`0x00..0x3F`) is written, and the read is
side-effect-free. The driver is read-only - it never writes CMOS - so it cannot
disturb other clock/NMI state. `// SAFETY:` comment present in source.

---

### arch/x86_64/syscall_entry.rs

Serial output helpers (`ser_putc`, `ser_puts`, `ser_hex64`) and per-core SYSCALL
MSR setup. Sound because serial helpers are guarded by the kernel's serial
spinlock; SYSCALL MSR setup runs once during per-core init before the core
enters the scheduler.

Three additional `unsafe {}` blocks (count +3): `read_user_bytes`
(`from_raw_parts`), `write_user_bytes` (`copy_nonoverlapping`), and
`read_cycle_counter` (`_rdtsc`). All three are sound because the pointer/length
pair is validated by `validate_user_ptr` before the unsafe call, ensuring the
range lies in user-space (below `USER_END`) and cannot overlap kernel memory;
`_rdtsc` is a read-only counter with no side effects. `// SAFETY:` comments
present in source.

---

### capability/table.rs

Access to `GLOBAL_RESOURCES` - a static `ResourceTable` protected by an
internal `SpinLock`. All seven unsafe calls go through the lock; the lock
ensures mutual exclusion across cores. `// SAFETY:` comments present in source.

---

### memory/allocator.rs

Frame allocator internals: bitmap manipulation, guard-page checks, allocator
init from the Limine memory map. Sound because the allocator is protected by a
`SpinLock`; bitmap indices are bounds-checked before access; guard-page ranges
are set once during init. `// SAFETY:` comments present in source for most
blocks; a small number need back-fill (see grandfathered note in §18).

Three additional `unsafe` lines (count 29 → 32): the `alloc_contiguous(n)` path
for driver DMA arenas (§12) - the `unsafe fn alloc_contiguous` method, its inner
`&mut *addr_of_mut!(BITMAP)` access, and the public `alloc_contiguous` wrapper's
`(*addr_of_mut!(ALLOCATOR)).alloc_contiguous(n)` call. Sound for the same reason
as the rest of the allocator: every access holds `ALLOC_LOCKED` (single writer
across all cores), and the bitmap scan is bounds-checked against
`max_valid_frame`. `// SAFETY:` comments present in source for all three.

Five further `unsafe` lines (count 32 → 37): one from the page-table reclaim guard
(`phys_in_ram`'s `ALLOCATOR.max_valid_frame` read, commit `b9dbc4c`) and four from the
DMA permanent-reserve net (§12, the DMA-safety net): `unsafe fn alloc_dma_arena`, its
inner `self.alloc_contiguous(n)` call, the table-full `bitmap_set_free` undo, and the
public `alloc_dma_arena` wrapper's `(*addr_of_mut!(ALLOCATOR)).alloc_dma_arena(n)` call.
`alloc_dma_arena` records the run in `dma_reserves` so `free` skips it - the arena is
never returned to the general pool to be recycled as a page table (a stray DMA then hits
DMA-reserved memory, not a PTE). Sound for the same reason as the rest of the allocator:
every access holds `ALLOC_LOCKED`. `// SAFETY:` comments present in source for all five.

---

### memory/frame.rs

`Frame::from_phys` - constructs a `Frame` from a raw physical address. Sound
because all callers are in the frame allocator or page-table walker, both of
which obtain addresses from the validated Limine memory map.
*(SAFETY comment missing in source - needs back-fill.)*

---

### memory/mod.rs

Calls `set_hhdm_offset` with the Limine-supplied HHDM offset during early init.
Sound because this runs exactly once, on the BSP, before any AP or task sees
virtual memory. `// SAFETY:` comment present in source.

---

### memory/page.rs

`Page::from_virt` - constructs a `Page` from a raw virtual address. Used only
by the page-table walker with addresses derived from the HHDM. Sound for the
same reason as `Frame::from_phys`.

---

### smp/core.rs

Per-core ready-flag manipulation via static arrays indexed by `core_id`.
`core_id` is bounded by `MAX_CORES` at all call sites. `// SAFETY:` comments
present in source.

---

### smp/ipi.rs

APIC IPI delivery: reads `APIC_VIRT_BASE`, writes to APIC ICR register, and
dispatches IPI handler. Sound because `APIC_VIRT_BASE` is set during BSP init
before any IPI is issued; ICR writes follow the APIC specification (write high
word first, then low word to trigger). `// SAFETY:` comments present in source
for most blocks; a small number need back-fill.

---

### smp/mod.rs

AP startup via `start_all_aps`. Delegates to `arch/x86_64/ap_boot.rs`.
`// SAFETY:` comment present in source.

---

### smp/placement.rs

Round-robin core assignment reads the `READY_CORES` count set by `smp/core.rs`.
Sound because the count is written before placement is ever called (BSP marks
core 0 ready before spawning init). `// SAFETY:` comment present in source.

---

### smp/spinlock.rs

`SpinLock<T>` interior-mutable spinlock. Seven unsafe constructs:
- `without_interrupts(f)` - two blocks: `unsafe { pushfq; pop; cli }` to capture
  RFLAGS.IF and mask interrupts on the local core, and `unsafe { sti }` to restore
  the prior enabled state. Local-core, no memory effects, IF restored exactly (nests
  correctly). REQUIRED for locks taken in both syscall and interrupt context
  (`KSTACK_USED`): without it a timer firing mid-critical-section re-enters the lock
  in the ISR on that core and self-deadlocks (the `chaos max-carnage` freeze).
- `unsafe impl Send for SpinLock<T>`: sound because the atomic spinlock
  serialises all access to `T`; `T: Send` is required.
- `unsafe impl Sync for SpinLock<T>`: same reasoning - mutual exclusion is
  enforced by the atomic before any shared reference is handed out.
- `unsafe { &*self.lock.data.get() }` in `Deref`: sound because the lock is
  held (we have a `SpinLockGuard`); no other reference to the inner data can
  exist simultaneously.
- `unsafe { &mut *self.lock.data.get() }` in `DerefMut`: same reasoning for
  mutable access.
- `pub const ZEROED: Self = unsafe { core::mem::zeroed() }`: all-zeroes
  initializer for placing a large `SpinLock<T>` in `.bss` without the undef
  padding bytes that LLD rejects there. Sound only when the all-zeroes bit
  pattern is a valid `T` - the caller's responsibility via the `T` instantiated.
  Replaces a `core::mem::zeroed()` that previously sat in `ipc/routing.rs`
  (outside the permitted layers); moving it here keeps `ipc/` unsafe-free (§18.1).

`// SAFETY:` comments present in source for all five.

---

### interrupt/route.rs *(grandfathered)*

`pub unsafe fn deliver(irq: u8)` - called from the IDT stub with IF=0.
One unsafe line remaining (the `unsafe fn` declaration).
`IRQ_TABLE` is now protected by `SpinLock`; registration and delivery are safe
with respect to the lock. The `unsafe` on `deliver` reflects the interrupt-context
calling convention (must only be called from the IDT with IF=0).
`// SAFETY:` comment present in source.

---

### loader.rs *(grandfathered)*

ELF loader: two private helpers (`read_ehdr`, `read_phdr`) each contain one
`read_unaligned` call that copies the entire packed ELF struct into a local
value; all field accesses in `load()` then go through safe local copies with no
unsafe at the call site. The remaining two unsafe blocks are `write_bytes` (BSS
zeroing) and `copy_nonoverlapping` (segment copy); both are bounded by bounds
checks performed above them. `// SAFETY:` comments present in source for all
four blocks.

---

### main.rs *(grandfathered)*

Two unsafe blocks: (1) BSP stack switch via inline ASM - sound because
`BSP_BOOT_STACK` is a 512 KiB static buffer and the pointer arithmetic is
bounded; (2) deref of `boot_info_ptr` - sound because the Limine bootloader
guarantees alignment and validity. (The earlier COM2-init block was removed when
`com2_init` was made a safe function.) `// SAFETY:` comments present in source.

The H4 hardening calls (`install_kstack_guards`, `harden_hhdm_nx`) are **safe `fn`s**
(boot-ordering preconditions, not UB), so they add no `unsafe` here - see the
2026-06-08 reconcile.

---

### syscall/dispatch.rs *(grandfathered)*

2 unsafe lines remaining (reduced from 26):
- `pub unsafe extern "C" fn syscall_handler`: the raw ring-3 → ring-0 entry
  point installed as the LSTAR target; must remain `unsafe` because it
  processes untrusted register values from user space.
- `unsafe { map_in_active_tables(va, phys, flags) }` inside `handle_alloc_mem`:
  sound because `va` is in the task heap region (above `0x1_0000_0000`);
  `phys` is a freshly allocated frame from the bitmap allocator; the active
  page table is the calling task's own CR3. `// SAFETY:` comment present in source.

All other handlers were converted from `unsafe fn` to `fn` - their user-pointer
accesses moved to `arch/x86_64::read_user_bytes` / `write_user_bytes` which
encapsulate the unsafe in the permitted arch layer.

---

### task/mod.rs *(grandfathered)*

Seven unsafe blocks: two in the kstack pool - `kstack_pool_base` (`addr_of!` of the
`static mut KSTACK_STORAGE`, the single encapsulated pool-address read, reused by
`free_kstack`) and `alloc_kstack` (`(addr_of_mut!(...) as *mut u8).add(...)` slot-top
arithmetic) - and five in the spawn path (`write_bytes` for stack zeroing,
`task_cap_init_empty`, `write_bytes` + `*mut ServiceContextData` cast for the ctx
page, `TaskContext::new_user`, and `commit_task`). All bounded by prior bounds checks
or scheduler-layer invariants. `// SAFETY:` comments present in source.

The H4 kstack-guard install (`install_kstack_guards`) is a **safe `fn`**: it reads the
pool base via `kstack_pool_base()` and delegates the per-slot page unmap to
`page_tables::unmap_4k_strided` (the arch layer, §18.1), so it adds no `unsafe` here -
see the 2026-06-08 reconcile. (Centralising the pool-address read in `kstack_pool_base`
also let `free_kstack` drop its own `addr_of!` block, holding the net count at 7.)

The previous magic-word liveness scheme (`KSTACK_MAGIC_USED` volatile
reads/writes at slot offset 0) was replaced by `SpinLock<[bool; TASK_KSTACK_MAX]>`,
removing 5 unsafe lines.

---

### task/scheduler.rs *(grandfathered)*

36 unsafe lines. Five formerly-`static mut` arrays converted to atomic types,
removing eight standalone `unsafe {}` blocks (previous count was 42, then 38,
now back to the original 36 floor after `TASK_VALID` was also converted):

- `CORE_CURRENT` → `[AtomicUsize; MAX_CORES]`: removed standalone `unsafe` in
  `current_task_slot()`; accesses updated to `.load()`/`.store()`.
- `CORE_RR_SLOT` → `[AtomicUsize; MAX_CORES]`: removed both standalone `unsafe`
  blocks in `pick_next()`.
- `CORE_PENDING_KSTACK_LEN` → `[AtomicUsize; MAX_CORES]`: removed both
  standalone `unsafe` blocks in `drain_pending_kstack()`.
- `TASK_KERNEL_STACK_TOP` → `[AtomicU64; MAX_TASKS]`: removed the standalone
  `unsafe` in `prepare_ring3_switch()`.
- `TASK_VALID` → `[AtomicBool; MAX_TASKS]`: removed the standalone
  `unsafe { TASK_VALID[slot] = false; }` in `release_task_slot()` and the
  inline `if !unsafe { TASK_VALID[slot] }` in `for_each_active_cap()`. All
  stores use `Release` ordering; the lock-free `for_each_active_cap` read uses
  `Acquire` to pair with `Release` stores and ensure cap table visibility; all
  reads inside lock-protected regions use `Relaxed`.
- `CORE_PENDING_PML4` is `AtomicU64` so its load/store sites are safe - only
  the `Frame::from_phys` + `free_frame` pair and the `send_ipi` call needed
  `unsafe` blocks.

One remaining line in `for_each_active_cap` is still `unsafe`:
- `unsafe { TASK_CAP[slot].assume_init_ref() }.for_each_slot(&mut f)` - reads
  a `MaybeUninit<CapTable>` after `TASK_VALID[slot].load(Acquire)` returned
  `true`. Sound because the `Acquire` load pairs with the `Release` store in
  `reserve_task_slot`/`enqueue`, establishing that the `CapTable` write
  happened-before this read. `CapTable` cannot be const-constructed so
  `MaybeUninit` is necessary; `assume_init_ref` is the unavoidable unsafe
  assertion that the slot is initialised.

One additional `unsafe {}` block (count +1, net): `TASK_CORE` reads in
`pick_next` - the wake-hint fast path (`TASK_CORE[hint]`) and the RR scan loop
(`TASK_CORE[idx]`) both read this `static mut [u32; MAX_TASKS]` array. Sound
because `TASK_CORE[slot]` is written exactly once at spawn and never modified
thereafter (§9.1 static-placement invariant); all indices are bounded by
`MAX_TASKS`; reads are unsynchronised but safe because the value is immutable
after task spawn. Two new `unsafe` lines were added; one previously-unsafe
access to a now-atomic variable was removed, yielding net +1.
`// SAFETY:` comments present in source for both new blocks.

Sound in aggregate: all arrays are indexed by slot or core_id with bounds
checked at their call sites; ring3 switch is called only from the scheduler
with interrupts disabled; cap init runs before the task is visible to other
cores; deferred PML4 free runs only after CR3 switch.
`// SAFETY:` comments present in source for all blocks.

## 2026-07-15 - multi-arch stubs: aarch64 / arm / loongarch64 / riscv32 / riscv64 / s390x

Six per-arch scaffolds under `arch/<isa>/mod.rs`, added while proving the demarcation
(docs/multi-arch.md). Each is the arch layer (a permitted §18.1 layer, exactly like
`arch/x86_64/`) for a non-x86 target: the `_start` naked entry, a minimal boot bring-up,
and a UART poke. All `unsafe` in them is the same class as x86's arch layer - inline
`asm!` for the boot sequence and raw MMIO writes to a fixed UART register - and each block
carries a `// SAFETY:` comment. They exist only to compile (all six) and boot (four:
aarch64/riscv64/loongarch64 to a UART print, x86 to the full shell) the arch-neutral kernel;
no neutral file gained any `unsafe`. `arch/arm/mod.rs` and `arch/riscv32/mod.rs` are the
32-bit word-size proof (docs/multi-arch.md, "Word size"); `arm` needs no atomics shim
(ARMv7 LDREXD), `riscv32` uses `portable-atomic` (RV32A has no 64-bit atomic). Counts are
the current stub sizes; they may grow as a real port fills the arch surface, each increase
carrying its own `// SAFETY:` and an audit bump.
