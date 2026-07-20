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
        "wfe",
        "b    4b",
        main = sym arm_boot_main,
        dtb = sym DTB_PTR,
    )
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

pub(super) fn pl011_write(s: &[u8]) {
    for &b in s {
        pl011_write_byte(b);
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
    let ram_end = dtb::report_memory(mmu::FALLBACK_RAM_END);
    mmu::set_ram_end(ram_end);
    mmu::enable();
    timer::init();
    const TICK_HZ: u32 = 100; // 10 ms quantum, matching CLAUDE.md section 9.1
    if irq::start_tick(TICK_HZ) {
        irq::selftest(TICK_HZ);
    }
    context::selftest();
    context::preempt_selftest();
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
pub fn ap_count() -> usize { 0 }
pub fn init(boot_info: &BootInfo) { unimplemented!("arm::init") }
pub fn init_timer() { unimplemented!("arm::init_timer") }
pub fn ap_init(core_id: u32) { unimplemented!("arm::ap_init") }

pub use interrupts::{disable_interrupts, enable_interrupts, wait_for_interrupt, local_irq_save, local_irq_restore};
pub use page_tables::{read_page_table_base, write_page_table_base, invalidate_tlb_page};
pub use syscall_entry::{read_cycle_counter, read_user_bytes, validate_user_ptr, write_user_bytes};

/// Switch to a new stack top - `sp` on AArch64. `#[inline(always)]` for the same reason as x86.
/// # Safety: caller guarantees `top` is a valid aligned stack top; nothing live is on the old stack.
#[inline(always)]
pub unsafe fn switch_to_boot_stack(top: u64) { unimplemented!("arm::switch_to_boot_stack") }

pub fn halt_all_cores() -> ! { loop { core::hint::spin_loop(); } }
pub fn hardware_reset() -> ! { loop { core::hint::spin_loop(); } }

// ---- Serial / console (board UART; stubbed - 32-bit proof is compile-only) ----
pub fn serial_write_byte(b: u8) { pl011_write_byte(b); }
pub fn serial_write_bytes_lockfree(s: &[u8]) { pl011_write(s); }
pub fn console_write_bytes_gated(s: &[u8], to_fb: bool) {}
pub fn set_console_echo(on: bool) {}
pub fn claim_console_foreground(task_slot: u32) {}
pub fn release_console_foreground() {}
pub fn release_console_foreground_if_owner(task_slot: u32) {}
pub fn console_foreground_allows(task_slot: u32) -> bool { true }
pub fn console_boot_complete() {}
pub fn console_push_byte(b: u8) {}
pub fn set_input_ready() {}
pub fn input_ready() -> bool { false }
pub fn com2_init() {}
pub fn com2_try_read_byte() -> Option<u8> { None }
pub fn uart_rx_pop() -> Option<u8> { None }
pub fn uart_rx_poll() {}
pub fn uart_rx_drain_now() {}

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
    pub unsafe fn get_lapic_id() -> u32 { 0 }
    pub unsafe fn send_ipi_to_lapic(lapic_id: u32, vector: u8) {}
    pub unsafe fn broadcast_ipi_all_but_self(vector: u8) {}
    pub unsafe fn set_tss_rsp0(core_id: usize, rsp: u64) {}
}

// ---------------------------------------------------------------------------
pub mod page_tables {
    use crate::memory::frame::{Frame, PhysAddr};

    pub const PAGE_SIZE: usize = 4096;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub struct VirtAddr(pub u64);

    bitflags::bitflags! {
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub struct PageFlags: u64 {
            const PRESENT  = 1 << 0;
            const WRITABLE = 1 << 1;
            const USER     = 1 << 2;
            const PWT      = 1 << 3;
            const PCD      = 1 << 4;
            const NO_EXEC  = 1 << 63;
        }
    }

    #[derive(Debug)]
    pub enum MapError { FrameAllocFailed, AlreadyMapped, NotMapped }

    pub struct PageTable { root: u64 }
    impl PageTable {
        pub fn new() -> Result<Self, MapError> { unimplemented!() }
        pub fn map(&mut self, virt: VirtAddr, phys: PhysAddr, flags: PageFlags) -> Result<(), MapError> { unimplemented!() }
        pub fn unmap(&mut self, virt: VirtAddr) -> Result<Frame, MapError> { unimplemented!() }
        pub fn cr3_value(&self) -> u64 { self.root }
        pub fn into_cr3(self) -> u64 { self.root }
    }

    pub fn get_hhdm_offset() -> u64 { 0 }
    pub unsafe fn set_hhdm_offset(offset: u64) {}
    pub fn read_page_table_base() -> u64 { 0 }               // TTBR0_EL1
    pub unsafe fn write_page_table_base(base: u64) {}
    pub unsafe fn invalidate_tlb_page(addr: u64) {}          // TLBI VAE1
    pub unsafe fn map_in_active_tables(virt: u64, phys: u64, flags: u64) -> Result<(), MapError> { unimplemented!() }
    pub fn entry_for_va(virt: u64) -> Option<u64> { None }
    pub fn unmap_4k_strided(base: u64, stride: u64, count: usize) {}
    pub fn harden_hhdm_nx() {}
    pub unsafe fn reclaim_user_frames(cr3: u64) -> usize { 0 }
}

// ---------------------------------------------------------------------------
pub mod syscall_entry {
    #[repr(C)]
    pub struct PerCoreSyscallData { pub user_rsp: u64, pub kernel_rsp: u64 }

    pub const USER_END: u64 = 0x0000_8000_0000_0000;
    pub fn syscall_slot(core_id: usize) -> *mut PerCoreSyscallData { core::ptr::null_mut() }
    pub fn init_percore_syscall_arena(n: usize) {}
    pub fn init_percore_arenas(n: usize) {}
    pub fn validate_user_ptr(ptr: u64, len: usize) -> bool { false }
    pub fn read_user_bytes(ptr: u64, len: usize) -> Option<&'static [u8]> { None }
    pub fn write_user_bytes(dst: u64, src: &[u8]) -> bool { false }
    /// CNTPCT - the ARM generic timer's physical counter, the arm32 analogue of RDTSC.
    pub fn read_cycle_counter() -> u64 { super::timer::cntpct() }
}

// ---------------------------------------------------------------------------
pub mod interrupts {
    pub const XHCI_MSI_VECTOR: u8 = 0x28;
    pub const EHCI_MSI_VECTOR: u8 = 0x29;
    pub fn enable_interrupts() {}                            // msr daifclr
    pub fn disable_interrupts() {}                           // msr daifset
    pub fn local_irq_save() -> bool { false }                // mrs DAIF
    pub fn local_irq_restore(was_enabled: bool) {}
    pub fn wait_for_interrupt() {}                           // wfi
    pub fn idle_can_halt() -> bool { false }
    pub fn send_eoi() {}                                     // GIC EOIR
    pub fn fire_test_irq(irq: u8) {}
}

// ---------------------------------------------------------------------------
pub mod context_switch {
    // AArch64: callee-saved x19-x28, fp/lr, sp + the page-table base. Field names kept x86-ish for the
    // stub compile; a real port renames them (and `cr3` in the neutral scheduler is a leak to address).
    #[repr(C)]
    pub struct TaskContext {
        pub rbx: u64, pub rbp: u64, pub r12: u64, pub r13: u64, pub r14: u64, pub r15: u64,
        pub rip: u64, pub rsp: u64, pub cr3: u64,
    }
    impl TaskContext {
        pub unsafe fn new_kernel(entry: unsafe extern "C" fn() -> !, stack_top: *mut u8, cr3: u64) -> Self {
            Self { rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0, rip: entry as u64, rsp: stack_top as u64, cr3 }
        }
        pub unsafe fn new_user(kernel_stack_top: *mut u8, user_entry: u64, user_stack_top: u64, cr3: u64) -> Self {
            Self { rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0, rip: user_entry, rsp: kernel_stack_top as u64, cr3 }
        }
    }
    pub unsafe extern "C" fn switch_context(current: *mut TaskContext, next: *const TaskContext) {}
}

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
