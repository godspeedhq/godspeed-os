// SPDX-License-Identifier: GPL-2.0-only
//! LoongArch64 (la64) arch layer - STUB scaffold that BOOTS in QEMU `virt`. The FOURTH ISA.
//!
//! Same `arch::imp` surface as x86_64/aarch64/riscv64; the neutral kernel compiles for loongarch64 with
//! only this file written. Bodies are stubs; real bodies (LoongArch page tables/DMW, CSR trap vector,
//! extended IRQ controller, stable timer) come later.

#![allow(unused_variables, dead_code)]

use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};

// ============================ Boot bring-up (QEMU `virt`) ============================
// QEMU loongarch `virt` UART is an NS16550 at 0x1fe0_01e0 (the LoongArch legacy UART address). At reset
// the CPU is in DA mode (direct address: VA==PA, paging off), so a direct write reaches the register.
const UART_THR: *mut u8 = 0x1fe001e0 as *mut u8;

/// ELF entry - QEMU `-kernel` jumps here. Set the stack, zero BSS, call Rust. softfloat target, so no
/// FP-enable step. LoongArch register ABI: $sp=r3, $t0-$t1=r12-r13, $zero=r0.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.boot"]
pub unsafe extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "la.pcrel $sp, __stack_top",         // boot stack
        "la.pcrel $t0, __bss_start",         // zero [__bss_start, __bss_end)
        "la.pcrel $t1, __bss_end",
        "1:",
        "bgeu $t0, $t1, 2f",
        "st.d $zero, $t0, 0",
        "addi.d $t0, $t0, 8",
        "b 1b",
        "2:",
        "bl {main}",                         // -> loong_boot_main (never returns)
        "3:",
        "idle 0",
        "b 3b",
        main = sym loong_boot_main,
    )
}

/// Rust side of boot. Milestone: write to the 16550 UART and halt.
extern "C" fn loong_boot_main() -> ! {
    for &b in b"
GodspeedOS loongarch64: _start reached, 16550 UART alive - the demarcation BOOTS on a FOURTH arch.
" {
        // SAFETY: UART_THR is QEMU loongarch virt NS16550 transmit register.
        unsafe { UART_THR.write_volatile(b); }
    }
    for &b in b"loongarch64: neutral kernel linked; arch/loongarch64 stubs pending real bodies. halting.
" {
        unsafe { UART_THR.write_volatile(b); }
    }
    loop {
        unsafe { core::arch::asm!("idle 0"); }
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
pub fn init(boot_info: &BootInfo) { unimplemented!("aarch64::init") }
pub fn init_timer() { unimplemented!("aarch64::init_timer") }
pub fn ap_init(core_id: u32) { unimplemented!("aarch64::ap_init") }

pub use interrupts::{disable_interrupts, enable_interrupts, wait_for_interrupt, local_irq_save, local_irq_restore};
pub use page_tables::{read_page_table_base, write_page_table_base, invalidate_tlb_page};
/// Non-PCI fixed-physical peripheral MMIO grant (ARM Pi path); no fixed windows on this arch stub.
pub fn map_fixed_driver_mmio(_pt: &mut page_tables::PageTable, _name: &str) -> Option<(u64, u64)> { None }

// USB-net bridge stubs: on this arch the NIC is a userspace PCIe driver, not an in-kernel USB device.
pub fn net_frame_tx(_frame: &[u8]) -> bool { false }
// No hardware-RNG backend exposed on this arch yet (x86 RDRAND is a trivial follow-up).
pub fn hw_random() -> Option<u32> { None }

/// The SD/EMMC controller's base clock in Hz, or 0 where the platform does not report one
/// (the block driver then refuses to guess a divider). Only the Pi's ARM port learns this,
/// from the VideoCore mailbox at boot.
pub fn emmc_base_clock_hz() -> u32 { 0 }
/// No board mailbox on this architecture: the driver uses whatever the chip holds. See query 23.
pub fn board_mac_packed() -> Option<u64> { None }

/// USB mass-storage block device. NO port has an in-kernel USB stack any more - `arch/arm/dwc2.rs`
/// and `arch/aarch64/xhci.rs` were both deleted (§6.4, 2026-08-09 and 2026-08-17) - so disks are
/// userspace drivers everywhere and this always answers "no device". The USB_DISK syscalls (46-49)
/// are a dead ABI; nothing holds the capability (`task/mod.rs`: `usb_disk: false`).
pub fn usb_disk_sectors() -> u64 { 0 }
pub fn usb_disk_read(_lba: u64, _dst: &mut [u8]) -> bool { false }
pub fn usb_disk_write(_lba: u64, _src: &[u8]) -> bool { false }
pub fn usb_disk_flush() -> bool { false }
/// Counter ticks a core may make NO forward progress before the liveness watchdog panics. `0` = this
/// arch cannot say (no calibrated counter rate yet), so the check stays off - see the x86 and arm
/// implementations for what a real answer looks like.
pub fn liveness_deadline_cycles() -> u64 { 0 }

pub fn usb_disk_busy() -> bool { false }
/// Is there no USB disk attached at all? Distinct from busy - see `USB_DISK_ABSENT` in the syscall
/// dispatch. This arch has no USB-disk backend, so a request never reaches one and the question is
/// moot; the read/write primitives already answer false.
pub fn usb_disk_absent() -> bool { true }


// No GPIO on this arch (the ARM `gpio` shell command is Pi-only).
pub fn gpio_op(_op: u32, _pin: u32) -> i64 { -1 }
pub fn net_frame_rx(_dst: &mut [u8]) -> usize { 0 }
pub fn net_info() -> Option<([u8; 6], bool)> { None }
pub use syscall_entry::{read_cycle_counter, read_user_bytes, validate_user_ptr, write_user_bytes};

/// Switch to a new stack top - `sp` on AArch64. `#[inline(always)]` for the same reason as x86.
/// # Safety: caller guarantees `top` is a valid aligned stack top; nothing live is on the old stack.
#[inline(always)]
pub unsafe fn switch_to_boot_stack(top: u64) { unimplemented!("aarch64::switch_to_boot_stack") }

/// The ELF `e_machine` and `EI_CLASS` this arch's service binaries carry (LoongArch, ELFCLASS64).
/// The neutral loader checks a candidate ELF against these, so it can parse a 32-bit ARM
/// service ELF or a 64-bit one without any arch-specific code in the loader itself.
pub const ELF_MACHINE: u16 = 258;
pub const ELF_CLASS: u8 = 2; // 1 = ELFCLASS32, 2 = ELFCLASS64

/// A11-1 hook: called from the timer tick on every core so a panic can stop the machine, not just the
/// panicking core. A no-op on this port until its `halt_all_cores` actually signals the other cores -
/// see the aarch64 implementation for the shape (a published flag, checked here).
pub fn panic_halt_check() {}

pub fn halt_all_cores() -> ! { loop { core::hint::spin_loop(); } }
pub fn hardware_reset() -> ! { loop { core::hint::spin_loop(); } }

// ---- Serial / console (NS16550 on QEMU loongarch virt @ 0x1fe0_01e0; stubbed) ----
pub fn serial_write_byte(b: u8) { unsafe { UART_THR.write_volatile(b); } }
pub fn serial_write_bytes_lockfree(s: &[u8]) { for &b in s { unsafe { UART_THR.write_volatile(b); } } }
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

pub static CONSOLE_READ_WAITER: AtomicU32 = AtomicU32::new(u32::MAX);

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
/// Hook called when the scheduler commits a **user** task. x86 ignores it; ARM records the slot so the
/// timer runs its syscalls atomically. Nothing to do on this stub yet.
pub fn note_user_task(_slot: usize) {}

// --- Boot/panic console floor backend (`crate::bootcon`) ---
// The kernel's boot/panic floor owes each arch one item (see `crate::bootcon`). No framebuffer is
// mapped on this stub, so the console never initialises and every entry point no-ops.

/// Publish a written rectangle. Nothing to publish yet.
pub fn fb_commit(
    _base: usize, _pitch: usize, _bpp: usize,
    _x: usize, _y: usize, _w: usize, _h: usize,
) {}

pub mod page_tables {

    /// Arch hook run once a service's address space is built. x86 needs nothing; ARM clones the kernel
    /// identity mapping into it.
    ///
    /// # Safety
    /// `_root` must be a page-table root this task owns.
    pub unsafe fn finalize_service_address_space(_root: u64) {}

    /// Free a task's page-table root and the structure below it, at task death.
    ///
    /// # Safety
    /// `_root` must belong to a task already marked Dead, after a TLB shootdown.
    pub unsafe fn free_page_table_root(_root: u64) {}

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
            // A framebuffer, not device registers: RAM the display controller scans out. Each arch
            // picks the weakest type that is still coherent without cache maintenance - on AArch64
            // Normal Non-cacheable, which gathers and buffers writes where Device-nGnRnE cannot. An
            // arch that has nothing better may ignore it and keep its uncached-MMIO type.
            const WRITE_COMBINE = 1 << 5;
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

    pub const PHYS_IS_IDENTITY: bool = false;

    /// No bootloader placed page tables for this port - the kernel builds its own, in `.bss` inside the
    /// kernel image, which the memory map already excludes from usable RAM. So there is nothing for
    /// `protect_kernel_page_table_frames` to protect, and its x86-format walk must not run here.
    pub const BOOTLOADER_PLACED_TABLES: bool = false;
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
    pub fn copy_user_to_kernel(_src: u64, _dst: *mut u8, _len: usize) -> bool { false }
    pub fn write_user_bytes(dst: u64, src: &[u8]) -> bool { false }
    pub fn read_cycle_counter() -> u64 { 0 }                 // CNTPCT_EL0
}

// ---------------------------------------------------------------------------
pub mod interrupts {
    pub const XHCI_MSI_VECTOR: u8 = 0x28;

    /// Vectors for a device class this arch's kernel actually routes, `&[]` where the controller
    /// does not exist here.
    ///
    /// These answer `task::hw_irqs_for`, which used to ask `#[cfg(target_arch)]` directly - one arm
    /// naming the vector and a `not(...)` arm returning `&[]` - for the two classes that only one
    /// port routes. That is the leak CLAUDE.md 4.1 is about: a neutral file knowing which ISA it was
    /// built for, so the NEXT port has to edit it. `XHCI_MSI_VECTOR` beside them was always done the
    /// right way round, which is why these are shaped to match it.
    ///
    /// An IRQ vector is AUTHORITY, not a setting (`hw_irqs_for`'s own header): routing one to a task
    /// is what makes that task receive the device's interrupts. `&[]` therefore means "this arch
    /// routes nothing for that class", which is a refusal, not a default.
    pub const DWC2_VECTORS: &[u8] = &[];
    pub const SOC_NIC_VECTORS: &[u8] = &[];
    pub const EHCI_MSI_VECTOR: u8 = 0x29;
    pub fn enable_interrupts() {}                            // msr daifclr
    pub fn disable_interrupts() {}                           // msr daifset
    pub fn local_irq_save() -> bool { false }                // mrs DAIF
    pub fn local_irq_restore(was_enabled: bool) {}
    pub fn wait_for_interrupt() {}                           // wfi
/// May the idle loop MASK interrupts, re-check for runnable work, and then halt - relying on the
/// halt to unmask and halt in one indivisible step?
///
/// This exists to close a lost-wakeup window, and the answer is a property of the silicon, so each
/// arch answers for itself rather than inheriting x86's. The window: the idle loop asks `pick_next`
/// for work, is told there is none, and halts. With interrupts ENABLED across that gap, a wake
/// landing in it is taken and consumed BEFORE the halt - and the halt then sleeps through the very
/// event it was told about, until the next timer tick. An idle core's tick is deliberately slowed to
/// about a second, so the cost of losing one is about a second.
///
/// x86 says yes: `sti; hlt` is architecturally atomic, so masking first, re-checking, and then
/// executing it cannot lose an interrupt raised in between - it is latched while masked and taken
/// the instant `sti` retires.
///
/// ARM says NO, and this is the reason the guard is a question rather than a rule: both ARM ports do
/// real work inside `wait_for_interrupt` (draining the UART so a keystroke can wake a blocked shell,
/// watching hub ports so a replug is noticed) and that work REQUIRES interrupts enabled - their own
/// comments say masking there would freeze the machine for the ~100 ms an enumeration takes. Masking
/// them to fix an x86 race would be importing our answer into their design (26.14). They keep the
/// narrower window; it is recorded here rather than silently left (26.7).
    pub fn idle_mask_before_halt() -> bool { false }

    pub fn idle_can_halt() -> bool { false }
    pub fn send_eoi() {}                                     // GIC EOIR
    pub fn fire_test_irq(irq: u8) {}

    /// The pool of MSI vectors the kernel may hand to a driver, as (base, length).
    ///
    /// EMPTY here, which is the honest answer rather than a placeholder: a pool of zero says "ask me
    /// for a vector and you get nothing", and the allocator treats that as "this machine cannot do
    /// message-signalled interrupts" - true of a scaffold with no PCI. riscv64 answers the same way
    /// for the same reason. A real LoongArch port replaces both when it has an interrupt controller.
    pub const MSI_POOL_BASE: u8 = 0;
    pub const MSI_POOL_LEN: usize = 0;
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
        /// All-zero context. Neutral code builds zero contexts via this, naming no register.
        pub const ZERO: Self = Self { rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0, rip: 0, rsp: 0, cr3: 0 };

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
    pub fn set_wall_clock(_epoch: i64) -> bool { false } // no RTC on this stub; SNTP wall clock unused (arm is the live RTC-less port)
    pub fn now_epoch_monotonic() -> i64 { 0 }
}

// ---------------------------------------------------------------------------
/// Is there an ethernet controller SOLDERED TO THE SOC - one on no bus the kernel can walk?
/// See the x86 original for why this is not a second source for `pci::nic()`.
pub fn soc_nic_present() -> bool { false }

pub mod pci {
    use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32};
    use portable_atomic::AtomicU64;

    /// One device as the kernel's PCI scan records it. Shape borrowed from the ports that have a bus,
    /// so the neutral callers compile unchanged; on this scaffold nothing ever constructs one.
    #[derive(Clone, Copy)]
    pub struct PciDevice {
        pub index: usize,
        pub bdf: u32,
        pub class_code: u32,
        pub bar: [u64; 6],
        pub irq_line: u8,
        pub vendor: u16,
        pub device: u16,
    }

    /// A ceiling readable off the source (26.6.1).
    ///
    /// ONE, on a machine with no PCI bus at all - and that is a FINDING, not a choice. The truthful
    /// answer here is zero, and zero does not build: `task/mod.rs` has
    ///
    ///     _ => &PCI_DMA_PHYS[0],
    ///
    /// whose own comment calls the arm "Unreachable ... returns a real slot rather than panicking".
    /// It is unreachable at runtime, but a ZERO-LENGTH array makes the compiler evaluate the index
    /// anyway and `unconditional_panic` refuses the build. So the neutral kernel carries an unstated
    /// contract: an arch must offer at least one PCI slot, whether or not it has a bus.
    ///
    /// arm - a complete, hardware-verified port on a board with no PCI whatsoever - already pays it,
    /// also with `MAX_DEVICES = 1`. This scaffold is simply the first to have tried saying zero. The
    /// constraint is recorded here rather than worked around silently, and fixing it means editing
    /// neutral code, which is exactly the kind of split the bounded-port test exists to find
    /// (scripts/scaffold_check.py).
    pub const MAX_DEVICES: usize = 1;

    /// Find a device by its 24-bit PCI class code - the lookup that replaced per-driver names in
    /// step D1. `None` on a machine with no bus, which is what the caller already handles.
    pub fn find_by_class(_class_code: u32) -> Option<PciDevice> { None }

    /// Where an MSI should be delivered, as an interrupt-controller destination id.
    ///
    /// The name is x86's (a Local APIC id). It is the seam's, not this port's, and answering it
    /// truthfully here means returning a destination no device will ever use.
    pub fn msi_dest_lapic(_core_id: u32) -> u8 { 0 }

    /// Program a device's MSI / MSI-X to raise `vector` at `dest`. False = not programmed, so the
    /// caller falls back to polling rather than waiting for an interrupt that cannot arrive.
    pub fn program_msi(_bdf: u32, _vector: u8, _dest: u8) -> bool { false }
    pub fn program_msix(_bdf: u32, _vector: u8, _dest: u8) -> bool { false }

    /// No PCI on this port - see the x86 originals. `None` is the honest answer, and the callers all
    /// treat it as "this machine has no PCI ethernet controller", which is true.
    pub fn ehci() -> Option<PciDevice> { None }
    /// Scaffold: no USB of any kind yet.
    /// Not a scan result: there is no bus to scan for an on-SoC part, which is why
    /// `HwClass::found` asked `cfg!(target_arch = "arm")` here before this existed.
    pub fn dwc2_present() -> bool { false }

    pub fn xhci() -> Option<PciDevice> { None }
    pub fn nic() -> Option<PciDevice> { None }
    pub fn first_memory_bar(_d: &PciDevice) -> u64 { 0 }
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
pub mod ioapic {
    pub fn init() {}
    pub fn mask_vector(vector: u8) {}
    pub fn unmask_vector(vector: u8) {}
}

// ---------------------------------------------------------------------------
pub mod ap_boot {
    pub unsafe fn start_all_aps(boot_info: &super::BootInfo) -> u32 { 0 }
}

// ---------------------------------------------------------------------------
// Seam members the neutral kernel asks every arch for. Answered here so this scaffold COMPILES -
// milestone M1 of the bounded-port test (scripts/scaffold_check.py). Each returns the value that is
// TRUE of a machine with no bus and no userspace yet, never a value invented to make a caller happy:
// a wrong answer that compiles is worse than a missing one, because the compiler stops asking.

/// Copy from a user address into kernel memory, refusing anything not mapped to the caller.
///
/// FALSE until this port has user pages, and refusing is the safe direction: a caller that cannot
/// read user memory fails its syscall, where one that wrongly SUCCEEDS reads someone else's memory.
pub fn copy_user_to_kernel(_src: u64, _dst: *mut u8, _len: usize) -> bool { false }

/// Per-core interrupt counters, as (count, last vector). `(0, 0)` here.
///
/// A STUB THAT RETURNS ZERO IS A KNOWN TRAP on this seam member: x86's was `(0, 0)` for the life of
/// the port, and a liveness investigation could not tell "the timer stopped" from "the tick was
/// skipped" because the instrument reported the same thing either way. It is acceptable only while
/// this arch takes no interrupts at all. The moment it does, this must become real or the first
/// wedge here is undiagnosable.
pub fn core_irq_debug(_core: u32) -> (u32, u32) { (0, 0) }

/// What the CPU calls itself, for the boot line. An ARCH is not a MACHINE, so this is the ISA name
/// until there is a way to read the model.
pub fn cpu_identity(buf: &mut [u8]) -> usize {
    let name = b"LoongArch64";
    let n = name.len().min(buf.len());
    buf[..n].copy_from_slice(&name[..n]);
    n
}

/// Record that `vector` was taken on this core. No-op: nothing raises an interrupt here yet, and a
/// counter that only ever counts zero is better left obviously empty than quietly wrong.
pub fn note_irq(_vector: u32) {}

/// Read one 32-bit PCI configuration register, or `None` where config space is unreachable.
///
/// `None` is load-bearing rather than lazy: this is the seam `hw-enumerator` reaches through, and the
/// riscv64 port shipped with it returning `None` unconditionally - so the service was spawned, found
/// nothing, and the failure looked like a missing driver rather than a missing seam answer. On this
/// scaffold there is genuinely no config space, so `None` is the truth.
pub fn pci_cfg_read32(_sel: u32, _off: u16) -> Option<u32> { None }

/// Publish the boot core's interrupt-controller id so the scheduler can address it.
///
/// The NAME is x86's - a Local APIC id - and LoongArch has no LAPIC. It is recorded here rather than
/// renamed because the seam is shared: every arch answers this, so the vocabulary is the seam's debt,
/// not this port's. No-op until secondary cores exist.
pub fn publish_bsp_lapic_id() {}

/// How many bytes the panic path emitted without taking the serial lock. Zero until this port has a
/// lock to bypass.
pub fn serial_unlocked_emit_count() -> u64 { 0 }

/// Is a driver's DMA arena mapped uncached?
///
/// FALSE, and on a scaffold that is the truthful answer rather than a deferral: nothing here grants a
/// DMA arena, so there is no mapping whose cacheability could differ. Note this is a PER-MASTER fact
/// on real silicon, not a per-arch one - the VisionFive's display is non-coherent while its USB is
/// coherent - so a real port must not assume one answer covers the board.
pub const DMA_ARENA_UNCACHED: bool = false;

/// The virtual address a driver's DMA arena is mapped at. Matches the other ports' choice so the
/// neutral layout code is unchanged; unused until something here grants an arena.
pub const DRIVER_DMA_VA: u64 = 0x7000_0000;
