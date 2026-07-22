// SPDX-License-Identifier: GPL-2.0-only
//! ARM (armv7, 32-bit) arch layer - STUB scaffold for the 32-bit word-size PROOF (compile-only).
//!
//! Same `arch::imp` surface; proves the neutral kernel compiles for 32-bit ARM. ARMv7 HAS 64-bit
//! atomics (LDREXD/STREXD), so `portable_atomic::AtomicU64` is native here (no shim) - unlike RV32.

#![allow(unused_variables, dead_code)]

use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};

/// Physical address of the flattened device tree, as handed to us in r2 by the firmware.
///
/// Captured in `_start` (into r10 before the mode check clobbers r0-r2) and published here only
/// *after* the BSS zero, which would otherwise wipe it. Zero means the firmware passed nothing.
#[no_mangle]
pub static mut DTB_PTR: u32 = 0;

pub mod exceptions;
pub mod dtb;
pub mod mmu;
pub mod timer;
pub mod irq;
pub mod context;
pub mod context_switch;
pub mod page_tables;
pub mod meminit;
pub mod syscall;
pub mod video;
pub mod fbcon;
pub mod dwc2;
pub mod usermode;
pub mod loadtest;
pub mod spawn;
pub mod sched_demo;
pub mod sched_user;
pub mod sched_ipc;
pub mod sched_spawn;
pub mod sched_supervisor;
pub mod sched_shell;

// ============================ Boot bring-up (Raspberry Pi 2 Model B) ============================
// BCM2836 peripheral base is 0x3F00_0000 (the BCM2835/Pi 1 was 0x2000_0000; the BCM2711/Pi 4 is
// 0xFE00_0000 - this constant is the single thing that moves between Broadcom generations).
//
// PL011 UART0 sits at +0x201000. On the Pi 2 it is wired to the GPIO header (pins 8/10) and is the
// default console; unlike the Pi 3/4 there is no Bluetooth to steal it, so no dtoverlay is needed.
// Confirmed on the board: Linux boots here with `console=ttyAMA0,115200`.
const PERIPHERAL_BASE: usize = 0x3F00_0000;
const PL011_BASE:      usize = PERIPHERAL_BASE + 0x20_1000;
const PL011_DR:        *mut u32 = PL011_BASE as *mut u32;              // +0x00 data
const PL011_FR:        *const u32 = (PL011_BASE + 0x18) as *const u32; // +0x18 flags
const PL011_LCRH:      *mut u32 = (PL011_BASE + 0x2C) as *mut u32;     // +0x2C line control
const PL011_CR:        *mut u32 = (PL011_BASE + 0x30) as *mut u32;     // +0x30 control
const PL011_FR_TXFF:   u32 = 1 << 5;                                   // transmit FIFO full
const PL011_FR_BUSY:   u32 = 1 << 3;                                   // transmitting
const PL011_LCRH_8N1:  u32 = (3 << 5) | (1 << 4);                      // WLEN=8 bits, FIFOs on
const PL011_CR_ON:     u32 = (1 << 0) | (1 << 8) | (1 << 9);           // UARTEN | TXE | RXE

// GPIO controller (BCM2836) sits at +0x200000. UART0 uses GPIO14 (TXD0) + GPIO15 (RXD0), both ALT0.
const GPIO_BASE:  usize = PERIPHERAL_BASE + 0x20_0000;
const GPFSEL1:    *mut u32 = (GPIO_BASE + 0x04) as *mut u32; // function select for GPIO10-19
const GPPUD:      *mut u32 = (GPIO_BASE + 0x94) as *mut u32; // pull up/down enable
const GPPUDCLK0:  *mut u32 = (GPIO_BASE + 0x98) as *mut u32; // pull up/down clock (GPIO0-31)

/// Route GPIO14/GPIO15 to the UART (ALT0) so BOTH transmit AND receive reach header pins 8/10. The
/// firmware often muxes only GPIO14 (TX) for console *output*, leaving GPIO15 (RX) as an input - so
/// output works but typing does nothing. Setting both to ALT0 here makes receive work regardless of how
/// the firmware left the header. Runs with the MMU off; the GPIO block is identity-mapped MMIO.
fn gpio_init_uart() {
    // SAFETY: BCM2836 GPIO registers, identity-mapped MMIO, single-threaded boot. Read-modify-write of
    // GPFSEL1 touches only GPIO14/15's function bits; the pull sequence is the BCM2835-spec dance.
    unsafe {
        let mut sel = GPFSEL1.read_volatile();
        sel &= !((0b111 << 12) | (0b111 << 15)); // clear GPIO14, GPIO15 function fields
        sel |= (0b100 << 12) | (0b100 << 15);    // ALT0 = UART0 TXD0 / RXD0
        GPFSEL1.write_volatile(sel);
        // Disable pull-up/down on both UART pins (they are externally driven). BCM2835 pull sequence:
        // write GPPUD, wait, write the pin clock, wait, clear both.
        let spin = |n: u32| { for _ in 0..n { core::arch::asm!("nop", options(nomem, nostack)); } };
        GPPUD.write_volatile(0);
        spin(150);
        GPPUDCLK0.write_volatile((1 << 14) | (1 << 15));
        spin(150);
        GPPUD.write_volatile(0);
        GPPUDCLK0.write_volatile(0);
    }
}

/// Bring the PL011 up for output, **preserving whatever baud divisors are already programmed**.
///
/// Do not assume the firmware did this. On real hardware it has (Linux runs a console here at
/// 115200), but under `qemu-system-arm -M raspi2b -kernel` there is no firmware at all: the UART
/// comes up disabled, every write to DR is silently swallowed, and FR.TXFF reads 0 so the poll below
/// never even blocks. Output just vanishes - which is exactly the failure seen the first time this
/// booted. Explicit init makes the same image work in both worlds.
///
/// IBRD/FBRD are deliberately NOT touched. The Pi's UART reference clock depends on firmware
/// (`init_uart_clock`, commonly 48 MHz) and differs under emulation, so recomputing divisors here
/// would risk a wrong baud on one of the two targets. QEMU ignores baud for a chardev, and hardware
/// firmware has already set it correctly for 115200 - so keeping the existing divisors is right on
/// both. Sequence per the PL011 spec: disable, drain, set the line format, re-enable.
fn pl011_init() {
    // SAFETY: BCM2836 UART0 registers, identity-mapped with the MMU off. Volatile MMIO writes in the
    // order the PL011 spec requires; no memory is aliased and no other core is running yet.
    gpio_init_uart(); // mux GPIO14/15 to the UART so RECEIVE works, not just transmit
    unsafe {
        PL011_CR.write_volatile(0);
        while PL011_FR.read_volatile() & PL011_FR_BUSY != 0 {}
        PL011_LCRH.write_volatile(PL011_LCRH_8N1);
        PL011_CR.write_volatile(PL011_CR_ON);
    }
}

/// Image entry - the firmware loads `kernel7.img` flat at 0x8000 and branches to byte 0, so this
/// must be physically first (`.text.boot`, KEEPed by the linker script).
///
/// Four things have to happen before any Rust runs, and three of them are ARMv7 traps that do not
/// exist on AArch64:
///
/// 1. **Drop out of HYP mode.** Cortex-A7 has the virtualization extensions and the Pi firmware
///    enters an ARMv7 kernel in HYP (mode 0x1A) so a hypervisor *could* install itself. Ordinary
///    kernel code expects SVC. We check CPSR and `eret` down to SVC only if we are actually in HYP,
///    so the same image works whichever mode the firmware hands us. This is the ARMv7 counterpart of
///    the AArch64 CPACR_EL1.FPEN trap: skip it and the failure is baffling and far from the cause.
/// 2. **Park the secondary cores.** All four A7s start executing here. Read MPIDR and send anything
///    that is not core 0 to a WFE loop. (Later SMP work takes them off the firmware mailboxes at
///    0x4000_008C + 0x10*core instead.)
/// 3. **Enable VFP/NEON.** Both are trapped at reset via CPACR cp10/cp11 and FPEXC.EN. The target is
///    soft-float so this *should* be unnecessary, but LLVM may still emit NEON for bulk copies - the
///    exact bug that cost a debugging session on AArch64. Enabling it costs four instructions.
/// 4. **Stack, then zeroed BSS**, before calling into Rust.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.boot"]
pub unsafe extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        // ---- 1. If we booted in HYP (mode 0x1A), eret down to SVC. Otherwise fall through. ----
        // `armv7a-none-eabi` does not enable the virtualization extensions, so the assembler rejects
        // spsr_hyp/elr_hyp/eret without this. The Cortex-A7 HAS them; only the default target
        // description is conservative.
        ".arch_extension virt",
        // The firmware hands us r0 = 0, r1 = machine type, r2 = **DTB address**, and that pointer is
        // the only way to learn the machine's real memory map. Stash it in r10 before anything else:
        // the mode check below clobbers r0/r1 immediately, and the BSS-zero loop would take r2. r10 is
        // callee-saved and untouched by everything between here and the store into DTB_PTR.
        "mov  r10, r2",
        "mrs  r0, cpsr",
        "and  r1, r0, #0x1f",
        "cmp  r1, #0x1a",
        "bne  2f",
        "bic  r0, r0, #0x1f",
        "orr  r0, r0, #0xd3",            // SVC (0x13) + I/F masked (0xC0)
        "msr  spsr_hyp, r0",
        "adr  r1, 2f",
        "msr  elr_hyp, r1",
        "eret",
        "2:",
        "cpsid if",                      // interrupts off until the IDT-equivalent exists

        // ---- 2. Only core 0 continues; the other three A7s park. ----
        "mrc  p15, 0, r1, c0, c0, 5",    // MPIDR
        "and  r1, r1, #3",
        "cmp  r1, #0",
        "bne  4f",

        // ---- 3. Full access to CP10/CP11 (VFP/NEON), then FPEXC.EN. ----
        "mrc  p15, 0, r0, c1, c0, 2",    // CPACR
        "orr  r0, r0, #(0xf << 20)",
        "mcr  p15, 0, r0, c1, c0, 2",
        "isb",
        ".fpu vfpv3-d16",
        "mov  r0, #0x40000000",          // FPEXC.EN
        "vmsr fpexc, r0",

        // ---- 4. Stack, then zero [__bss_start, __bss_end). ----
        "ldr  sp, =__stack_top",
        "ldr  r1, =__bss_start",
        "ldr  r2, =__bss_end",
        "mov  r3, #0",
        "3:",
        "cmp  r1, r2",
        "bhs  5f",
        "str  r3, [r1], #4",
        "b    3b",
        "5:",
        "ldr  r0, ={dtb}",               // publish the DTB pointer AFTER the BSS zero, or it would
        "str  r10, [r0]",                //   be wiped along with everything else in BSS
        "bl   {main}",                   // -> arm_boot_main (never returns)
        "4:",
        "b    arm_ap_park",              // secondary cores: watch the mailbox, jump to ap_entry on release
        main = sym arm_boot_main,
        dtb = sym DTB_PTR,
    )
}

/// Secondary-core park + release loop, reached from `_start` when MPIDR says we are not core 0.
///
/// `r1` holds this core's id (1-3). We watch this core's BCM2836 mailbox-3 read/clear register
/// (`0x400000CC + 0x10*core`): core 0 writes `ap_entry`'s physical address to the matching set
/// register (`0x4000008C + 0x10*core`) in `smp_bringup`, we read it, clear it, and jump. This mirrors
/// the firmware spin-table exactly, so it works whether QEMU/firmware started this core here in
/// `_start` (we park and are released) or held it in its own spin-table (core 0's write releases it
/// straight to `ap_entry`, bypassing us). Either way the AP arrives at `ap_entry` with the MMU off.
core::arch::global_asm!(
    ".section .text.boot",
    ".globl arm_ap_park",
    "arm_ap_park:",
    "mov  r2, #0x40000000",
    "orr  r2, r2, #0xCC",            // 0x400000CC = core 0 mailbox-3 read/clear
    "add  r2, r2, r1, lsl #4",       // + 0x10*core -> this core's mailbox-3
    "1:",
    "wfe",
    "ldr  r3, [r2]",                 // read this core's mailbox
    "cmp  r3, #0",
    "beq  1b",                       // nothing yet -> keep waiting
    "str  r3, [r2]",                 // write the value back = clear the mailbox
    "bx   r3",                       // jump to ap_entry (physical address; MMU off)
);

/// Secondary cores that came online (set by `smp_bringup`); `ap_count()` returns this.
static AP_ONLINE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Per-core kernel stacks for the secondary cores (core 0 uses the linker `__stack_top`). 64 KiB each
/// (= 1 << 16, so the AP entry asm can shift rather than multiply). Slot `core` is that AP's stack;
/// slot 0 is unused. BSS.
const AP_KSTACK_SIZE: usize = 64 * 1024;
#[repr(C, align(16))]
struct ApKStacks([u8; AP_KSTACK_SIZE * 4]);
static mut AP_KSTACK_REGION: ApKStacks = ApKStacks([0; AP_KSTACK_SIZE * 4]);

/// AP entry: reached from `arm_ap_park` (or the firmware spin-table) with the MMU OFF and this core's
/// id in MPIDR. Drop HYP defensively, enable VFP/NEON, set this core's stack top
/// (`AP_KSTACK_REGION + (core+1)*64 KiB`), then call `ap_boot_main(core)`. Never returns.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.boot"]
pub unsafe extern "C" fn ap_entry() -> ! {
    core::arch::naked_asm!(
        ".arch_extension virt",
        // Drop HYP if firmware left us there (idempotent: skip if already SVC/secure).
        "mrs  r0, cpsr",
        "and  r1, r0, #0x1f",
        "cmp  r1, #0x1a",
        "bne  2f",
        "bic  r0, r0, #0x1f",
        "orr  r0, r0, #0xd3",            // SVC + I/F masked
        "msr  spsr_hyp, r0",
        "adr  r1, 2f",
        "msr  elr_hyp, r1",
        "eret",
        "2:",
        "cpsid if",
        // VFP/NEON on (CPACR cp10/11 then FPEXC.EN) - same reason as core 0's _start.
        "mrc  p15, 0, r0, c1, c0, 2",
        "orr  r0, r0, #(0xf << 20)",
        "mcr  p15, 0, r0, c1, c0, 2",
        "isb",
        ".fpu vfpv3-d16",
        "mov  r0, #0x40000000",
        "vmsr fpexc, r0",
        // core id -> r4
        "mrc  p15, 0, r4, c0, c0, 5",
        "and  r4, r4, #3",
        // stack top = AP_KSTACK_REGION + (core+1) * 64 KiB  (64 KiB = 1 << 16)
        "ldr  r0, ={kstacks}",
        "add  r5, r4, #1",
        "lsl  r5, r5, #16",
        "add  sp, r0, r5",
        "mov  r0, r4",                   // ap_boot_main(core)
        "bl   {apmain}",
        "3:", "wfe", "b 3b",             // ap_boot_main never returns; guard anyway
        kstacks = sym AP_KSTACK_REGION,
        apmain  = sym ap_boot_main,
    )
}

/// Rust side of a secondary core's bring-up. Runs with the MMU OFF on `core`'s own stack, then brings
/// this core into the SAME kernel address space and the neutral per-core scheduler. Never returns.
extern "C" fn ap_boot_main(core_id: u32) -> ! {
    // Vectors FIRST (before the MMU), so a fault ANYWHERE in the rest of this core's bring-up is
    // REPORTED through the vectors instead of wandering into garbage. On real HW core 3's bring-up
    // intermittently faulted before vectors were installed and, with VBAR still 0, branched into low
    // memory (an UNDEF at 0x618) and halted the boot. arm_vectors is a kernel .text symbol at its
    // identity address, valid with the MMU off or on, so this is safe here. Fail loud, never wild.
    exceptions::install_for_core(core_id);
    // Synchronize with core 0's published boot state (page tables, arenas) before relying on it - the
    // weak-ordering hygiene a released AP owes on the Cortex-A7 (SEC-25/28 class). Core 0 flushed its
    // D-cache and `dsb`+`sev`'d before release; match it with a barrier on this side.
    // SAFETY: `dsb`/`isb` are PL1 barriers with no memory effects.
    unsafe { core::arch::asm!("dsb sy", "isb", options(nomem, nostack)); }
    // Coherency + exclusives for shareable memory (LDREX/STREX, every spinlock) - before caches/MMU.
    // SAFETY: ACTLR is a PL1 control register; SMP before caches is the documented Cortex-A7 order.
    unsafe {
        core::arch::asm!(
            "mrc p15, 0, {t}, c1, c0, 1", "orr {t}, {t}, #(1 << 6)", "mcr p15, 0, {t}, c1, c0, 1", "isb",
            t = out(reg) _, options(nomem, nostack),
        );
    }
    // Load the SAME L1 core 0 built: this core now sees the whole kernel address space.
    // SAFETY: core 0 finished build_tables and released us; every mapping is identity.
    unsafe { mmu::enable_on_this_core(); }
    // Duplicate/mis-identified core guard. On the real Pi 2, releasing core 3 brought up a core whose
    // MPIDR read back as 0 - it registered as a SECOND core 0, so two cores ran `scheduler::run(0)`,
    // raced on core 0's state, and one crashed the boot. A core that finds its own id ALREADY ready is
    // such a confused release: park it safely (it never double-registers or runs a second scheduler),
    // and the system continues on the cores that came up cleanly. Vectors are installed, so it still
    // reports a later fault loudly rather than wandering.
    if crate::smp::core::is_ready(core_id) {
        crate::kprintln!(
            "smp: a released core reports id {} which is ALREADY ready - mis-identified, parking it", core_id);
        loop {
            // SAFETY: WFI is always valid; park this confused core instead of running it.
            unsafe { core::arch::asm!("wfi") }
        }
    }
    // Register our id so the neutral current_core_id() resolves us, then start our own timer tick.
    crate::smp::core::set_core_lapic_id(core_id, core_id);
    irq::start_tick_ap(core_id);
    // Announce ready (logs "smp: core N ready") and enter the neutral per-core scheduler. The run
    // queue is empty until the supervisor places a service on this core, so we idle until then.
    crate::smp::core::mark_ready(core_id);
    crate::task::scheduler::run(core_id)
}

/// Bring the secondary cores online (SMP). Core 0 calls this from a sched path AFTER the machine is up
/// (MMU, per-core scheduler arenas, NEUTRAL_SCHED). Releases cores 1-3 via the BCM2836 mailbox
/// spin-table and waits (bounded) for each to mark itself ready - as x86's `start_all_aps` waits on
/// `mark_ready`. A core that never answers is left not-ready (§11.3 "continue with available cores");
/// placement to it then fails gracefully and services fall back to core 0.
pub fn smp_bringup() {
    // From here more than one core writes the UART, and core 0's MMU + caches + ACTLR.SMP are all on
    // (arm_boot_main ran long ago), so the serial guard's exclusive access is now sound. Enable it
    // BEFORE releasing any AP, so the very first concurrent write is already serialized.
    SERIAL_SMP.store(true, core::sync::atomic::Ordering::Release);

    // Publish everything core 0 wrote (L1 tables, scheduler arenas) before the APs - which start with
    // caches OFF, reading physical memory directly - can observe it.
    // SAFETY: a set/way clean+invalidate of core 0's D-cache; valid at PL1, no operands.
    unsafe { page_tables::clean_invalidate_dcache_all(); }

    let entry = ap_entry as *const () as u32; // identity-mapped, so physical == virtual
    for core in 1u32..=3 {
        crate::kprintln!("smp: releasing core {}...", core);
        // Write ap_entry to this core's mailbox-3 SET register (0x4000008C + 0x10*core), then SEV to
        // wake it from WFE.
        // SAFETY: the core-local block is Device-mapped; a volatile write to a fixed mailbox register,
        // followed by the barrier + event that make the write visible and wake the waiter.
        unsafe {
            ((0x4000_008C + 0x10 * core as usize) as *mut u32).write_volatile(entry);
            core::arch::asm!("dsb", "sev", options(nomem, nostack));
        }
        // Wait (bounded, generous) for this core before releasing the next - a wedged core is then
        // distinct from a slow one, and each `smp: core N ready` line stays ordered. ~40M spins is
        // tens of ms, far longer than a healthy AP's MMU+timer bring-up. Re-issue SEV periodically: if
        // the core was still transitioning into WFE when the first event fired (a lost wakeup), the
        // mailbox is still set and a fresh SEV nudges it out of WFE to re-check and proceed.
        let mut online = false;
        for i in 0..40_000_000u32 {
            if crate::smp::core::is_ready(core) { online = true; break; }
            if i & 0x3F_FFFF == 0x3F_FFFF {
                // SAFETY: re-arm the event line; SEV has no memory effects and is always valid at PL1.
                unsafe { core::arch::asm!("dsb", "sev", options(nomem, nostack)); }
            }
            core::hint::spin_loop();
        }
        if online {
            AP_ONLINE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::kprintln!("smp: core {} up", core);
        } else {
            crate::kprintln!("smp: WARNING - core {} did NOT come up; continuing without it", core);
        }
    }
    crate::kprintln!("smp: {} cores ready", AP_ONLINE.load(core::sync::atomic::Ordering::Relaxed) + 1);
}

/// Write one byte to the PL011, waiting for room in the transmit FIFO.
///
/// The firmware has already configured the UART (115200 8N1 - the same line Linux uses), so no
/// baud/line setup is needed for this milestone. We poll TXFF rather than writing blind, or a burst
/// longer than the 16-byte FIFO would silently drop characters.
pub(super) fn pl011_write_byte(b: u8) {
    // SAFETY: PL011_FR/PL011_DR are the BCM2836 UART0 flag and data registers, identity-mapped with
    // the MMU off. Volatile MMIO: poll until the TX FIFO has room, then write one byte to transmit.
    unsafe {
        while PL011_FR.read_volatile() & PL011_FR_TXFF != 0 {}
        PL011_DR.write_volatile(b as u32);
    }
}

/// Best-effort cross-core serialization of the one PL011 UART. Under SMP, cores 0/1/2/... all write the
/// single UART; without this each core's bytes interleave and every log line garbles (seen on the Pi 2:
/// "smp: core 1 ready" mangled into neighbouring lines). This is NOT the neutral `SpinLock` on purpose:
/// a `SpinLock` watchdog-panics on a wedge, and the panic path itself writes serial - a recursion trap.
/// Instead every writer BOUNDED-acquires this flag and writes REGARDLESS: a fault-time / panic dump, or a
/// write that interrupts a holder on the same core, is never lost or deadlocked (it may rarely interleave).
static SERIAL_BUSY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// True once `smp_bringup` is about to run more than one core. The serial guard uses `compare_exchange`
/// (LDREX/STREX), and on ARMv7 an EXCLUSIVE access before the MMU + caches + ACTLR.SMP are enabled is
/// architecturally UNPREDICTABLE (it faults/hangs - the same hazard that once wedged the cap-table
/// spinlock). Every boot message before `smp_bringup` runs on core 0 alone, with the MMU possibly still
/// off, so those writes MUST stay lock-free. Once this is set (inside `smp_bringup`, well after
/// `mmu::enable` and ACTLR.SMP), the exclusive is sound and needed. A plain atomic LOAD of this flag is
/// a bare `LDR` (not LDREX), which is safe pre-MMU.
pub(super) static SERIAL_SMP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Cap on the acquire spin (~one full serial line at 115200 baud). A waiter that exceeds it writes
/// anyway rather than block forever - the only correct choice for a UART also used by the fault handler.
const SERIAL_ACQUIRE_SPINS: u32 = 50_000;

pub(super) fn pl011_write(s: &[u8]) {
    use core::sync::atomic::Ordering;
    // Pre-SMP (or the boot core alone): no contention, and LDREX/STREX are unsafe before the MMU is on.
    // Write lock-free.
    if !SERIAL_SMP.load(Ordering::Relaxed) {
        for &b in s { pl011_write_byte(b); }
        fbcon::put_bytes(s); // mirror to the TV once the console is up (no-op before that)
        return;
    }
    // Clear any stale exclusive-monitor reservation before the compare-exchange below. ARMv7 does NOT
    // guarantee the local monitor is cleared on exception entry/return, so a task that took an
    // interrupt (or an SVC) mid-`ldrex`/`strex` sequence can leave the monitor "exclusive" to a foreign
    // address - making EVERY subsequent `strex` here fail spuriously and forever (the shell's second
    // console echo wedged exactly this way). An explicit `clrex` resets it so the acquire can succeed.
    // SAFETY: `clrex` clears the local exclusive monitor; no memory effect.
    unsafe { core::arch::asm!("clrex", options(nomem, nostack)); }
    // SMP is live: serialize across cores. Try to claim the UART (bounded), write, then release only if
    // we claimed it - a fault-time / heavily-contended write is never lost or deadlocked.
    let mut held = false;
    let mut spins: u32 = 0;
    while SERIAL_BUSY.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        spins += 1;
        if spins >= SERIAL_ACQUIRE_SPINS { break; } // give up waiting; write lock-free (never deadlock)
        core::hint::spin_loop();
    }
    if spins < SERIAL_ACQUIRE_SPINS { held = true; }
    for &b in s {
        pl011_write_byte(b);
    }
    fbcon::put_bytes(s); // mirror to the TV under the same guard (serialized across cores)
    if held {
        SERIAL_BUSY.store(false, Ordering::Release);
    }
}

/// Rust side of boot. Milestone 1: prove the toolchain, the load address, the HYP drop, and the UART
/// on real 32-bit silicon, then halt. The neutral kernel is already linked in; what is still missing
/// before `kernel_main` can run is the ARMv7 MMU (short/long descriptors via CP15), the vector table
/// (VBAR), and the BCM2836 interrupt controller - none of which is shared with AArch64.
extern "C" fn arm_boot_main() -> ! {
    pl011_init();
    pl011_write(b"\r\nGodspeedOS arm32: _start reached SVC, PL011 alive - 32-bit ARM BOOTS.\r\n");
    pl011_write(b"arm32: Raspberry Pi 2 Model B (BCM2836, Cortex-A7), peripherals @ 0x3F000000.\r\n");
    exceptions::install();
    // Set ACTLR.SMP (bit 6) BEFORE enabling caches/MMU. On Cortex-A7, exclusive access (LDREX/STREX -
    // the basis of every spinlock) to cacheable, shareable memory needs the SMP bit; without it, an
    // exclusive store can fail perpetually and a spinlock deadlocks. Firmware often sets it, but not
    // always (nor under QEMU), so set it explicitly. Harmless if already set or if the write is ignored
    // in non-secure state.
    // SAFETY: ACTLR is a PL1 control register; ORing in SMP before caches are on is the documented
    // Cortex-A7 bring-up order. No memory effects.
    unsafe {
        core::arch::asm!(
            "mrc p15, 0, {t}, c1, c0, 1",   // read ACTLR
            "orr {t}, {t}, #(1 << 6)",      // SMP = 1 (coherency + exclusives for shareable memory)
            "mcr p15, 0, {t}, c1, c0, 1",   // write ACTLR
            "isb",
            t = out(reg) _,
            options(nomem, nostack),
        );
    }
    let ram_end = dtb::report_memory(mmu::FALLBACK_RAM_END);
    mmu::set_ram_end(ram_end);
    // Ask the GPU for a framebuffer BEFORE turning the MMU + caches on: the mailbox exchange is only
    // coherent with the GPU while the ARM caches are off (on real silicon the reply comes back through
    // the A7's L2, which an L1 clean does not reach). Request the display's NATIVE resolution so the
    // framebuffer fills the screen (no pillarbox bars); fall back to 1280x720 if the query fails.
    let (fbw, fbh) = video::query_display_size().unwrap_or((1280, 720));
    let fb = video::request(fbw, fbh);
    mmu::enable();
    // Map the framebuffer and bring up the text console over it, so the boot log + shell prompt appear
    // on the TV (mirrored from serial). Everything logged from here on shows on the display.
    if let Some(fb) = fb {
        video::map(&fb);
        fbcon::init(fb.base, fb.pitch, fb.width, fb.height);
        pl011_write(b"arm32: framebuffer console up - this line should appear on the TV\r\n");
    }
    timer::init();
    const TICK_HZ: u32 = 100; // 10 ms quantum, matching CLAUDE.md section 9.1
    if irq::start_tick(TICK_HZ) {
        irq::selftest(TICK_HZ);
    }
    context::selftest();
    context::preempt_selftest();
    context_switch::selftest();
    let reserve_end = meminit::init(ram_end);
    meminit::selftest();
    syscall::selftest();
    usermode::selftest();
    loadtest::selftest();
    // USB host bring-up (DWC2): detect the controller + the attached device. Increment 1 - no transfers
    // yet. Runs before the scheduler dispatch (which never returns).
    dwc2::init();
    #[cfg(feature = "arm-sched-demo")]
    sched_demo::run(ram_end, reserve_end);
    #[cfg(feature = "arm-sched-user")]
    sched_user::run(ram_end, reserve_end);
    #[cfg(feature = "arm-sched-ipc")]
    sched_ipc::run(ram_end, reserve_end);
    #[cfg(feature = "arm-sched-spawn")]
    sched_spawn::run(ram_end, reserve_end);
    #[cfg(feature = "arm-supervisor")]
    sched_supervisor::run(ram_end, reserve_end);
    #[cfg(feature = "arm-shell")]
    sched_shell::run(ram_end, reserve_end);
    #[cfg(feature = "arm-spawn-logger")]
    spawn::boot_service(ram_end, reserve_end);
    let _ = (ram_end, reserve_end);
    page_tables::selftest();
    #[cfg(feature = "arm-fault-test")]
    exceptions::trigger_test_fault();
    pl011_write(b"arm32: machine layer COMPLETE - MMU, vectors, tick, cooperative + preemptive switch.\r\n");
    pl011_write(b"       Neutral kernel linked; scheduler integration + user mode pending. halting.\r\n");
    loop {
        // SAFETY: WFI is always valid; wait for an interrupt that never comes (halt).
        unsafe { core::arch::asm!("wfi"); }
    }
}

// ---- Boot info (shape shared with x86; a real port fills it from the DTB / UEFI) ----
#[repr(C)]
pub struct BootInfo {
    pub memory_map: &'static [MemoryRegion],
    pub kernel_phys_start: u64,
    pub kernel_phys_end: u64,
    pub hhdm_offset: u64,
    pub rsdp_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemoryRegion {
    pub base: u64,
    pub len: u64,
    pub kind: MemoryKind,
}

#[repr(u32)]
#[derive(Clone, Copy)]
pub enum MemoryKind {
    Usable = 1,
    Reserved = 2,
    AcpiReclaimable = 3,
    KernelImage = 4,
    BootloaderReclaimable = 5,
}

// ---- Lifecycle ----
//
// On ARM the machine is brought up in `arm_boot_main` (MMU, vectors, timer, tick, allocator) *before*
// any neutral code runs, rather than by the neutral `kernel_main` calling `arch::imp::init` partway
// through. So these are honest no-ops: the work they name is already done, not skipped. They exist to
// complete the `arch::imp` surface (the boundary the whole port rests on).
/// Number of secondary cores to SIZE the per-core arenas for. The BCM2836 always has 4 A7s, so this
/// is a constant 3 (like x86 sizing to Limine's enumerated count) - `percpu_init` runs this BEFORE
/// `smp_bringup` releases the cores, so it must be the expected count, not the live one. A core that
/// fails to come up is simply never `is_ready` (its arena slot goes unused); `AP_ONLINE` tracks how
/// many actually answered, for the boot log.
pub fn ap_count() -> usize { 3 }

/// Machine init - already performed in `arm_boot_main` before neutral code runs. No-op.
pub fn init(_boot_info: &BootInfo) {}

/// Timer init - the generic timer + BCM2836 tick are already up (`timer::init` / `irq::start_tick`).
pub fn init_timer() {}

/// AP init - the secondary A7s are parked in `_start`; SMP bring-up (firmware mailboxes) is later
/// work. Never reached while `ap_count() == 0`.
pub fn ap_init(_core_id: u32) {}

pub use interrupts::{disable_interrupts, enable_interrupts, wait_for_interrupt, local_irq_save, local_irq_restore};
pub use page_tables::{read_page_table_base, write_page_table_base, invalidate_tlb_page};
pub use syscall_entry::{read_cycle_counter, read_user_bytes, validate_user_ptr, write_user_bytes};

/// Switch to a new stack top - `sp` on ARM. `#[inline(always)]` for the same reason as x86: the
/// caller's frame must not outlive the switch.
/// # Safety: caller guarantees `top` is a valid 8-byte-aligned stack top; nothing live is on the old
/// stack.
#[inline(always)]
pub unsafe fn switch_to_boot_stack(top: u64) {
    // SAFETY: sets SP to the caller-provided stack top. `nostack` because nothing is pushed/popped.
    unsafe { core::arch::asm!("mov sp, {t}", t = in(reg) top as u32, options(nomem, nostack)) }
}

/// The ELF `e_machine` and `EI_CLASS` this arch's service binaries carry (ARM, ELFCLASS32).
/// The neutral loader checks a candidate ELF against these, so it can parse a 32-bit ARM
/// service ELF or a 64-bit one without any arch-specific code in the loader itself.
pub const ELF_MACHINE: u16 = 40;
pub const ELF_CLASS: u8 = 1; // 1 = ELFCLASS32, 2 = ELFCLASS64

pub fn halt_all_cores() -> ! { loop { core::hint::spin_loop(); } }
pub fn hardware_reset() -> ! { loop { core::hint::spin_loop(); } }

// ---- Serial / console (PL011: output = pl011_write; input = the PL011 RX FIFO drained into a ring) ----
pub fn serial_write_byte(b: u8) { pl011_write_byte(b); }
pub fn serial_write_bytes_lockfree(s: &[u8]) { pl011_write(s); }
/// The shell's console output. No framebuffer on this port, so `to_fb` is ignored - everything goes to
/// the PL011 (the serial console). Without this the shell's prompt/output would silently vanish.
pub fn console_write_bytes_gated(s: &[u8], _to_fb: bool) { pl011_write(s); }
pub fn set_console_echo(on: bool) {}
pub fn claim_console_foreground(task_slot: u32) {}
pub fn release_console_foreground() {}
pub fn release_console_foreground_if_owner(task_slot: u32) {}
pub fn console_foreground_allows(task_slot: u32) -> bool { true }
pub fn console_boot_complete() {}

// PL011 receive FIFO -> a single-producer/single-consumer input ring. The producer is `pl011_rx_drain`
// (polled from the timer tick and by a blocked `console_read` itself); the consumer is `uart_rx_pop`
// (the ConsoleRead syscall). PL011 FR bit 4 = RXFE (RX FIFO empty).
const PL011_FR_RXFE: u32 = 1 << 4;
const RX_BUF_SIZE: usize = 256;
static mut RX_BUF: [u8; RX_BUF_SIZE] = [0; RX_BUF_SIZE];
static RX_HEAD: AtomicU32 = AtomicU32::new(0);
static RX_TAIL: AtomicU32 = AtomicU32::new(0);
static INPUT_READY: AtomicBool = AtomicBool::new(false);

/// Drain every byte currently in the PL011 RX FIFO into the input ring. Single producer (guard IRQs at
/// the call site if a poll and a syscall could race; on this single-core port they are serialised by
/// the syscall/IRQ masking already).
fn pl011_rx_drain() {
    // SAFETY: reading the PL011 FR/DR (Device-mapped MMIO) and appending to the ring; the ring indices
    // are atomics and this is the only producer path.
    unsafe {
        loop {
            if PL011_FR.read_volatile() & PL011_FR_RXFE != 0 { break; } // RX FIFO empty
            let b = (PL011_DR.read_volatile() & 0xFF) as u8;
            let tail = RX_TAIL.load(Ordering::Relaxed) as usize;
            let head = RX_HEAD.load(Ordering::Acquire) as usize;
            let next = (tail + 1) % RX_BUF_SIZE;
            if next == head { continue; } // ring full: drop this byte, keep draining the FIFO
            RX_BUF[tail] = b;
            RX_TAIL.store(next as u32, Ordering::Release);
        }
    }
}

/// Pop one byte from the input ring (the ConsoleRead syscall consumer). `None` if empty.
pub fn uart_rx_pop() -> Option<u8> {
    let head = RX_HEAD.load(Ordering::Relaxed) as usize;
    let tail = RX_TAIL.load(Ordering::Acquire) as usize;
    if head == tail { return None; }
    // SAFETY: single consumer; head is in-bounds.
    let b = unsafe { RX_BUF[head] };
    RX_HEAD.store(((head + 1) % RX_BUF_SIZE) as u32, Ordering::Release);
    Some(b)
}

/// Drain the RX FIFO into the ring right now (called by a blocked `console_read` so input capture never
/// hinges on the timer tick, which the atomic-syscall path may skip while a user task is mid-syscall).
pub fn uart_rx_drain_now() { pl011_rx_drain(); }

/// Hands-off chaos demo (`arm-autochaos`). A serial-output-only setup (no keyboard) still can't type a
/// command, so a few seconds after boot - once the supervisor has spawned everything and the shell is
/// at a steady prompt - inject `chaos max-carnage all-services 10` plus its `y` confirm into the input
/// ring. The shell consumes it exactly as if typed (real path, real confirmation), runs the storm, and
/// prints the report to serial. Called from the Core-0 timer tick; a one-shot latch fires it once.
#[cfg(feature = "arm-autochaos")]
pub fn autochaos_tick() {
    use core::sync::atomic::AtomicU32;
    static TICKS: AtomicU32 = AtomicU32::new(0);
    static FIRED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if FIRED.load(Ordering::Relaxed) { return; }
    // ~100 Hz tick; wait ~6 s so boot (supervisor spawns, shell prompt) has settled.
    if TICKS.fetch_add(1, Ordering::Relaxed) < 600 { return; }
    if FIRED.swap(true, Ordering::Relaxed) { return; }
    for &b in b"chaos max-carnage all-services 10\ry\r" {
        console_push_byte(b);
    }
    pl011_write(b"\r\nautochaos: injected 'chaos max-carnage all-services 10' + confirm (hands-off demo)\r\n");
}

/// Timer-tick hook: drain the RX FIFO and wake any task blocked in ConsoleRead. Runs from
/// `timer_tick_from_irq` (core 0).
pub fn uart_rx_poll() {
    pl011_rx_drain();
    // Advance USB enumeration one transaction per tick, on core 0 only (it is the single writer of the
    // DWC2 channel + DMA buffer). Reached both from the Core-0 tick and from the idle loop; the MPIDR
    // gate keeps an AP that idles here from racing core 0 on the controller.
    {
        let mpidr: u32;
        // SAFETY: reading MPIDR (`c0, c0, 5`) is a side-effect-free PL1 register read.
        unsafe { core::arch::asm!("mrc p15, 0, {m}, c0, c0, 5", m = out(reg) mpidr, options(nomem, nostack)); }
        if mpidr & 3 == 0 { dwc2::poll(); }
    }
    if RX_HEAD.load(Ordering::Acquire) != RX_TAIL.load(Ordering::Acquire) {
        let waiter = CONSOLE_READ_WAITER.load(Ordering::Acquire);
        if waiter != u32::MAX {
            crate::task::scheduler::wake_by_slot(waiter as usize, 0);
        }
    }
}

/// Inject a byte into the input ring + wake the reader (kernel-side producer; unused on this port,
/// which drives input straight from the PL011 RX FIFO, but kept for parity with the x86 keyboard path).
pub fn console_push_byte(b: u8) {
    let tail = RX_TAIL.load(Ordering::Relaxed) as usize;
    let head = RX_HEAD.load(Ordering::Acquire) as usize;
    let next = (tail + 1) % RX_BUF_SIZE;
    if next != head {
        // SAFETY: single producer in practice; tail in-bounds.
        unsafe { RX_BUF[tail] = b; }
        RX_TAIL.store(next as u32, Ordering::Release);
    }
    let waiter = CONSOLE_READ_WAITER.load(Ordering::Acquire);
    if waiter != u32::MAX {
        crate::task::scheduler::wake_by_slot(waiter as usize, 0);
    }
}

/// The input path is up (the PL011 RX is always available on this port). The shell waits on this before
/// presenting its prompt (the deterministic end-of-boot signal).
pub fn set_input_ready() { INPUT_READY.store(true, Ordering::Release); }
pub fn input_ready() -> bool { INPUT_READY.load(Ordering::Acquire) }
pub fn com2_init() {}
pub fn com2_try_read_byte() -> Option<u8> { None }

/// Hook called by the neutral `commit_task` when it commits a **user** task. On ARM this records the
/// slot as a ring-3 task so the timer runs its syscalls atomically (see `irq::mark_task_user` and the
/// atomic-syscall check in `irq::arm_irq_dispatch`). A no-op on x86, which tracks ring via `TASK_IS_USER`.
pub fn note_user_task(slot: usize) { irq::mark_task_user(slot); }

pub static CONSOLE_READ_WAITER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
pub mod boot {
    use super::*;
    pub static TSC_DEADLINE_MODE: AtomicBool = AtomicBool::new(false);
    pub fn init_gdt_arenas(n: usize) {}
    /// Idle-tick pacing (v0.7.0 power work, x86 Phase 2a). Neutral `scheduler.rs` calls these around
    /// its idle `wait_for_interrupt`: slow the timer while a core sleeps, restore the quantum on wake.
    /// A no-op here is CORRECT for a stub - the tick simply never slows - and a real port implements
    /// them on its own timer (generic timer on ARM, CLINT/mtimecmp on RISC-V).
    pub fn rearm_idle_timer() {}
    pub fn rearm_quantum_timer() {}
    pub fn audit_wx() {}
    pub fn tsc_ticks_per_quantum() -> u64 { 0 }
    pub unsafe fn rearm_tsc_deadline() {}
    pub unsafe fn apic_send_eoi() {}
    /// The calling core's id (0-3), read from MPIDR. On ARM the "lapic id" IS the core index, so the
    /// neutral `lapic_to_core_id` (which matches this against each ready core's registered id) resolves
    /// it identically. This is what makes `current_core_id()` correct on every core - the linchpin the
    /// whole per-core scheduler rests on. (Was `0` while the port was single-core.)
    pub unsafe fn get_lapic_id() -> u32 {
        let mpidr: u32;
        // SAFETY: reading MPIDR (c0,c0,5) is a side-effect-free PL1 register read.
        unsafe { core::arch::asm!("mrc p15, 0, {m}, c0, c0, 5", m = out(reg) mpidr, options(nomem, nostack)); }
        mpidr & 3
    }
    pub unsafe fn send_ipi_to_lapic(lapic_id: u32, vector: u8) {}
    pub unsafe fn broadcast_ipi_all_but_self(vector: u8) {}
    pub unsafe fn set_tss_rsp0(core_id: usize, rsp: u64) {}
}

// ---------------------------------------------------------------------------
// page_tables is now a real module (page_tables.rs): two-level 4 KiB tables, TTBR0/TLB
// primitives, and the neutral PageTable API - not the compile-only stub that was here.

// ---------------------------------------------------------------------------
pub mod syscall_entry {
    #[repr(C)]
    pub struct PerCoreSyscallData { pub user_rsp: u64, pub kernel_rsp: u64 }

    /// Top of the ARM user address space. 32-bit, so the ceiling is well below 4 GiB: services load
    /// at `0x400000` and their stack tops at `USER_STACK_TOP` (`0x8000_0000`), all under this.
    pub const USER_END: u64 = 0x8000_0000;

    // Real backing storage so `syscall_slot` is non-null. ARM has no SYSCALL/SYSRET fast path (user
    // sp/lr live in banked registers; syscall entry/exit is `svc`/`movs pc`), so `user_rsp`/`kernel_rsp`
    // are never *read* to drive a return. But the neutral spawn commits services with `is_user=true`,
    // and the neutral `prepare_ring3_switch` (+ user-RSP capture) then WRITES through this pointer for
    // every user task - so it must be real memory. The writes land here and are ignored. Sized for the
    // effectively-single-core port; clamp guards any stray index.
    const MAX_SLOTS: usize = 8;
    static mut SYSCALL_SLOTS: [PerCoreSyscallData; MAX_SLOTS] =
        [const { PerCoreSyscallData { user_rsp: 0, kernel_rsp: 0 } }; MAX_SLOTS];

    pub fn syscall_slot(core_id: usize) -> *mut PerCoreSyscallData {
        // SAFETY: index clamped into the fixed static array; single writer per core.
        unsafe { core::ptr::addr_of_mut!(SYSCALL_SLOTS[core_id.min(MAX_SLOTS - 1)]) }
    }
    pub fn init_percore_syscall_arena(_n: usize) {}
    pub fn init_percore_arenas(_n: usize) {}

    /// A user pointer is valid if the whole range lies below `USER_END`. A service runs under its own
    /// page table (kernel cloned in as privileged, service pages USER), and the kernel handles its
    /// `svc` in SVC mode under that same table - so a user VA is directly readable once range-checked.
    /// A genuinely unmapped user address still faults into the abort handler rather than reading junk.
    pub fn validate_user_ptr(ptr: u64, len: usize) -> bool {
        // Refuse a len past isize::MAX and a null base with a non-empty range: BOTH are hard
        // preconditions of `slice::from_raw_parts` / `copy_nonoverlapping`, so a service passing a
        // garbage len (seen under chaos/fuzz) must be REFUSED here, never panic the kernel in
        // read_user_bytes/write_user_bytes (§22 F1: no kernel panic on user-controllable syscall args).
        if len > isize::MAX as usize { return false; }
        if ptr == 0 && len != 0 { return false; }
        let end = match ptr.checked_add(len as u64) { Some(e) => e, None => return false };
        end <= USER_END
    }

    /// Borrow `len` bytes at user VA `ptr` as a slice, after range-checking. Returns `None` if the
    /// range escapes user space.
    pub fn read_user_bytes(ptr: u64, len: usize) -> Option<&'static [u8]> {
        if !validate_user_ptr(ptr, len) { return None; }
        // SAFETY: the range is within user space and mapped in the current (service) page table; the
        // kernel shares that table while handling the syscall, so `ptr` is directly addressable.
        Some(unsafe { core::slice::from_raw_parts(ptr as usize as *const u8, len) })
    }

    /// Write `src` to user VA `dst`, after range-checking. Returns false if the range escapes user space.
    pub fn write_user_bytes(dst: u64, src: &[u8]) -> bool {
        if !validate_user_ptr(dst, src.len()) { return false; }
        // SAFETY: range-checked user VA, mapped writable in the current page table (see read_user_bytes).
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst as usize as *mut u8, src.len()); }
        true
    }
    /// CNTPCT - the ARM generic timer's physical counter, the arm32 analogue of RDTSC.
    pub fn read_cycle_counter() -> u64 { super::timer::cntpct() }
}

// ---------------------------------------------------------------------------
pub mod interrupts {
    pub const XHCI_MSI_VECTOR: u8 = 0x28;
    pub const EHCI_MSI_VECTOR: u8 = 0x29;

    /// Unmask IRQs (`cpsie i`). Real, not a stub: the neutral `SpinLock` masks interrupts while held
    /// (via `local_irq_save`/`restore`), and a no-op here lets the timer ISR fire mid-lock and
    /// deadlock against the interrupted holder - exactly the hang the first service spawn hit.
    pub fn enable_interrupts() {
        // SAFETY: clearing CPSR.I is always valid; the vector table and handlers are installed.
        unsafe { core::arch::asm!("cpsie i", options(nomem, nostack)) }
    }

    /// Mask IRQs (`cpsid i`).
    pub fn disable_interrupts() {
        // SAFETY: setting CPSR.I is always architecturally valid.
        unsafe { core::arch::asm!("cpsid i", options(nomem, nostack)) }
    }

    /// Save the current IRQ-enable state and mask. Returns true if IRQs *were* enabled (so the paired
    /// `restore` knows whether to re-enable), the ARM analogue of x86 saving RFLAGS.IF.
    pub fn local_irq_save() -> bool {
        let cpsr: u32;
        // SAFETY: reading CPSR is side-effect-free; masking IRQs is always valid.
        unsafe {
            core::arch::asm!("mrs {c}, cpsr", c = out(reg) cpsr, options(nomem, nostack));
            core::arch::asm!("cpsid i", options(nomem, nostack));
        }
        cpsr & 0x80 == 0 // I bit (7) clear == IRQs were enabled
    }

    /// Re-enable IRQs only if they were enabled when `local_irq_save` ran (nests correctly).
    pub fn local_irq_restore(was_enabled: bool) {
        if was_enabled {
            // SAFETY: clearing CPSR.I; only done when the saved state had IRQs enabled.
            unsafe { core::arch::asm!("cpsie i", options(nomem, nostack)) }
        }
    }

    /// Wait for an interrupt - the idle primitive. **Enables IRQs, then `wfi`**, the ARM twin of x86's
    /// `sti; hlt`. This is load-bearing: the scheduler reaches here from a task that BLOCKED inside a
    /// syscall (the shell in `console_read`), and syscall entry masked IRQs (`cpsid i`). A bare `wfi`
    /// would idle with IRQs still masked, so the timer ISR could never fire - nothing would drain the
    /// UART RX or reschedule the woken task, and serial input would hang forever (exactly the bug this
    /// fixes). `cpsie i` unmasks so the timer wakes the core and runs the tick; the woken task's own
    /// saved CPSR is restored by `switch_context`, so it resumes with the IRQ state it had.
    pub fn wait_for_interrupt() {
        // Poll serial input from the idle loop and wake a blocked reader. The scheduler reaches here
        // when the shell has blocked in `console_read` with nothing else to run. The timer tick that
        // normally drains the UART does NOT fire while the core idles (WFI quiesces it under QEMU, and
        // even on hardware the tick is the only other drainer), so draining here is what lets a
        // keystroke actually arrive and reschedule the shell. Then `cpsie i; wfi` (the x86 `sti; hlt`
        // twin) unmasks IRQs so a timer/IPI can also wake us instead of busy-spinning forever.
        super::uart_rx_poll();
        // SAFETY: unmasking IRQs is always valid (vectors + handlers installed); WFI then waits for one.
        unsafe { core::arch::asm!("cpsie i", "wfi", options(nomem, nostack)) }
    }

    pub fn idle_can_halt() -> bool { true } // ARM WFI wakes on the generic-timer IRQ; halting is safe
    pub fn send_eoi() {}                    // BCM2836 timer has no separate EOI (TVAL re-arm clears it)
    pub fn fire_test_irq(_irq: u8) {}
}

// ---------------------------------------------------------------------------
// The neutral context-switch surface is now a REAL implementation (`context_switch.rs`), not a stub:
// TaskContext + new_kernel/new_user + switch_context that the arch-neutral scheduler drives directly.

// ---------------------------------------------------------------------------
pub mod rtc {
    pub use crate::clock::epoch_secs;
    pub fn capture_boot_time() {}
    pub fn boot_datetime() -> u64 { 0 }
    pub fn read_datetime() -> u64 { 0 }
    pub fn now_epoch_monotonic() -> i64 { 0 }
}

// ---------------------------------------------------------------------------
pub mod pci {
    use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32};
    use portable_atomic::AtomicU64;
    pub static XHCI_FOUND: AtomicBool = AtomicBool::new(false);
    pub static XHCI_MMIO_BASE: AtomicU64 = AtomicU64::new(0);
    pub static XHCI_BDF: AtomicU32 = AtomicU32::new(0xFFFF);
    pub static EHCI_FOUND: AtomicBool = AtomicBool::new(false);
    pub static EHCI_MMIO_BASE: AtomicU64 = AtomicU64::new(0);
    pub static EHCI_BDF: AtomicU32 = AtomicU32::new(0xFFFF);
    pub static AHCI_FOUND: AtomicBool = AtomicBool::new(false);
    pub static AHCI_ABAR: AtomicU64 = AtomicU64::new(0);
    pub static AHCI_BDF: AtomicU32 = AtomicU32::new(0xFFFF);
    pub static NIC_FOUND: AtomicBool = AtomicBool::new(false);
    pub static NIC_MMIO_BASE: AtomicU64 = AtomicU64::new(0);
    pub static NIC_BDF: AtomicU32 = AtomicU32::new(0xFFFF);
    pub static NIC_VENDOR_DEVICE: AtomicU32 = AtomicU32::new(0);
    pub fn init() {}
    pub fn clear_bus_master(bdf: u32) {}
    pub fn set_bus_master(bdf: u32) {}
    pub fn set_power_d0(bdf: u32) {}
    pub fn xhci_bios_handoff() {}
    pub fn ehci_flr_probe() {}
    pub fn program_xhci_msi() -> bool { false }
    pub fn program_ehci_msi() -> bool { false }
    pub fn route_ehci_intx() {}
}

// ---------------------------------------------------------------------------
pub mod iommu {
    pub fn detect(rsdp_addr: u64, hhdm: u64) {}
    pub fn bringup(hhdm: u64) {}
    pub fn confine_device(bdf: u32, arena_phys: u64, arena_len: u64) -> bool { false }
    pub fn release_device(bdf: u32) {}
    pub fn drain_event_log() {}
}

// ---------------------------------------------------------------------------
pub mod fb {
    pub fn dims_packed() -> u64 { 0 }
}

// ---------------------------------------------------------------------------
pub mod ioapic {
    pub fn init() {}
    pub fn mask_vector(vector: u8) {}
    pub fn unmask_vector(vector: u8) {}
}

// ---------------------------------------------------------------------------
pub mod ap_boot {
    pub unsafe fn start_all_aps(boot_info: &super::BootInfo) -> u32 { 0 }
}
