// SPDX-License-Identifier: GPL-2.0-only
//! RISC-V (rv64) arch layer - STUB scaffold that BOOTS in QEMU `virt` (docs/aarch64.md pattern).
//!
//! The THIRD architecture. Exposes the SAME `arch::imp` surface as arch/x86_64/ and arch/aarch64/, so
//! the arch-NEUTRAL kernel compiles for riscv64 with only this file written - the boundary, generalised
//! to a third ISA. Bodies are stubs; real bodies (Sv39 MMU, S-mode trap vec, PLIC/CLINT, SBI) come later.

#![allow(unused_variables, dead_code)]

pub mod fdt;

use core::sync::atomic::{AtomicU32, AtomicUsize, AtomicBool, Ordering};

// ============================ Boot bring-up (S-mode via OpenSBI) ============================
// The 16550 sits at 0x1000_0000 on BOTH QEMU `virt` and the StarFive JH7110, which is luck rather
// than design and the only reason one banner prints on both.
//
// WHAT DIFFERS IS THE REGISTER LAYOUT, and it cost the board its first sixteen characters. The
// transmit-hold register is at offset 0 either way, so writing bytes blind appeared to work - until
// hardware, where output stopped at EXACTLY 16 characters, twice, at the same byte. Sixteen is the
// 16550's transmit FIFO depth: we filled it and every byte after was dropped on the floor. QEMU
// accepts bytes as fast as they are written and has no FIFO to overrun, so it could not have shown
// this. The fix is to wait for the transmitter, which is Commandment VIII in its smallest possible
// form: wait on TRUTH (the THRE bit), never on time.
//
// The layouts, read out of the board's own device tree rather than guessed:
//   QEMU `virt`   ns16550a          reg-shift 0, byte registers   -> LSR at +0x05
//   JH7110        snps,dw-apb-uart  reg-shift 2, 32-bit registers -> LSR at +0x14
// A `reg-shift` of 2 means the registers are four bytes apart, so every register except the one at
// offset 0 moves. That is why THR worked and LSR would not have.
//
// THIS IS NO LONGER A BUILD-TIME CHOICE. It was, briefly, behind a `visionfive` feature; the device
// tree states the base, the shift and the width, so the kernel reads them instead of being told.
// The feature still exists but now selects ONE thing and nothing else: the LINK ADDRESS, which
// cannot be discovered at runtime because a non-relocatable kernel has to be placed before it runs.

/// The UART, as the MACHINE describes it - not as a build flag asserts it.
///
/// This replaces a `visionfive` feature that picked the register layout at compile time. The layout
/// is a fact about the hardware, the device tree states it, and a kernel that is TOLD which board it
/// is on is exactly what this port is trying not to be. Two machines, one binary's worth of logic:
///
///     QEMU `virt`   ns16550a          reg-shift 0, byte registers
///     JH7110        snps,dw-apb-uart  reg-shift 2, 32-bit registers
///
/// Written once, before the first character is printed, and read-only afterwards. Boot is
/// single-threaded at that point - one hart, no interrupts enabled - so `Relaxed` is honest here
/// rather than merely cheap.
static UART_BASE: AtomicUsize = AtomicUsize::new(0x1000_0000);
static UART_SHIFT: AtomicU32 = AtomicU32::new(0);
static UART_WIDTH: AtomicU32 = AtomicU32::new(1);

/// Point the console at the UART the device tree describes.
///
/// The DEFAULTS ABOVE ARE A DELIBERATE FALLBACK, not a guess at the board: offset 0 is the transmit
/// register at ANY `reg-shift`, so a kernel that cannot read its device tree can still say so. It
/// will be slow - the LSR poll reads the wrong address and times out per character - but a slow
/// error message beats a silent hang, which is the whole argument of invariant 12.
fn uart_configure(base: u64, shift: u32, width: u32) {
    UART_BASE.store(base as usize, Ordering::Relaxed);
    UART_SHIFT.store(shift, Ordering::Relaxed);
    UART_WIDTH.store(width, Ordering::Relaxed);
}

#[inline]
fn uart_reg(index: usize) -> usize {
    UART_BASE.load(Ordering::Relaxed)
        + (index << (UART_SHIFT.load(Ordering::Relaxed) as usize))
}

/// Line status register. Read at the width the tree declares: a 32-bit register read as a byte
/// returns the right lane on a little-endian machine, but reading a byte-wide one as 32 bits does
/// not, so the width is honoured rather than assumed.
#[inline]
fn uart_lsr() -> u8 {
    let addr = uart_reg(5);
    // SAFETY: MMIO the firmware left mapped, at the address and width the device tree declares.
    unsafe {
        if UART_WIDTH.load(Ordering::Relaxed) >= 4 {
            (addr as *const u32).read_volatile() as u8
        } else {
            (addr as *const u8).read_volatile()
        }
    }
}

#[inline]
fn uart_thr(b: u8) {
    let addr = uart_reg(0);
    // SAFETY: as above; index 0 is the transmit-hold register at every `reg-shift`.
    unsafe {
        if UART_WIDTH.load(Ordering::Relaxed) >= 4 {
            (addr as *mut u32).write_volatile(b as u32);
        } else {
            (addr as *mut u8).write_volatile(b);
        }
    }
}

/// LSR bit 5: transmit holding register empty.
const LSR_THRE: u8 = 1 << 5;

/// Send one byte, waiting for the transmitter rather than for a delay.
///
/// The spin is BOUNDED. A wrong LSR address would otherwise never report ready and would hang the
/// boot before anything had been printed - a silent hang being strictly worse than dropped output,
/// which is the failure this function exists to fix. Past the bound it writes anyway: on a UART that
/// is genuinely wedged the byte is lost either way, and a partial banner still tells a reader that
/// the kernel reached this line.
fn putc(b: u8) {
    let mut spins: u32 = 0;
    while uart_lsr() & LSR_THRE == 0 {
        spins += 1;
        if spins > 200_000 {
            break;
        }
    }
    uart_thr(b);
}

/// ELF entry - OpenSBI (QEMU default firmware) jumps here in S-mode at 0x8020_0000 (a0=hartid, a1=dtb).
/// Only the boot hart arrives (OpenSBI parks the rest via HSM). Set the stack, zero BSS, call Rust. No
/// FP-enable needed: riscv64imac is integer-only (soft-float), so no FP traps (unlike aarch64 CPACR).
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.boot"]
pub unsafe extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        // ---- RISC-V Linux Image header, 64 bytes ----------------------------------------
        // U-Boot's `booti` REFUSES an image without it: the VisionFive printed
        // "Bad Linux RISCV Image magic!" after loading all 4279 bytes correctly. QEMU never
        // asked, because `-kernel` is handed an ELF and reads the entry from its header - so
        // this is a requirement of the BOOT PROTOCOL, not of the silicon, and it could only
        // ever have shown up on hardware.
        //
        // Layout is Documentation/riscv/boot-image-header.rst. The first field is executable:
        // it jumps over the rest, so `_start` remains the entry on every path and the QEMU
        // boot is unaffected.
        //
        // `norvc` around the jump because the header is a fixed layout: with compressed
        // instructions enabled `j` assembles to 2 bytes, every field after it shifts by two,
        // and the magic lands somewhere U-Boot does not look.
        ".option push",
        ".option norvc",
        "j    99f",                  // code0: jump past the header
        ".word 0",                   // code1
        ".option pop",
        ".dword 0x200000",           // text_offset: where we expect to be loaded past RAM base
        ".dword __image_size",       // image_size: how much U-Boot must keep clear for us
        ".dword 0",                  // flags: LE, 4 KiB pages
        ".word  2",                  // version 0.2
        ".word  0",                  // res1
        ".dword 0",                  // res2
        ".dword 0",                  // magic: deprecated, must be zero
        ".word  0x05435352",         // magic2: "RSC" + 0x05
        ".word  0",                  // res3
        "99:",
        // ---------------------------------------------------------------------------------
        "la   sp, __stack_top",
        "la   t0, __bss_start",
        "la   t1, __bss_end",
        "1:",
        "bgeu t0, t1, 2f",
        "sd   zero, 0(t0)",
        "addi t0, t0, 8",
        "j    1b",
        "2:",
        "call {main}",
        "3:",
        "wfi",
        "j    3b",
        main = sym riscv_boot_main,
    )
}

/// Rust side of boot. Milestone: write to the 16550 UART and halt. Later: Sv39 MMU, S-mode trap vector
/// (stvec) for ecall/faults/IRQ, PLIC/CLINT, SBI HSM for SMP - toward the neutral `kernel_main`.
/// Boot hart id and device tree, exactly as the firmware left them.
///
/// `_start` never touches `a0` or `a1` - it writes `sp`, `t0` and `t1` only - so the two arguments
/// the RISC-V boot protocol puts there are still live at the `call`, and taking them as parameters is
/// enough to receive them. No asm change, no scratch space, nothing to keep in step.
///
/// TAKE THE HART ID, NEVER ASSUME IT. On QEMU `virt` the boot hart is 0 and every "hart 0" shortcut
/// looks correct; on the JH7110 it is hart 1, because hart 0 is the S7 monitor core. That is the same
/// trap that cost a day on x86, where the BSP's APIC id was assumed to be 0 and QEMU happened to
/// agree - so it is taken from the register here rather than inferred anywhere.
extern "C" fn riscv_boot_main(hartid: usize, fdt: *const u8) -> ! {
    // PARSE BEFORE PRINTING. The device tree states where the UART is and how its registers are
    // laid out, so reading it first is what lets the banner print correctly on a machine nobody
    // compiled for. Doing it the other way round is why a build flag existed at all.
    //
    // SAFETY: `fdt` is the pointer the boot protocol placed in `a1`; `from_ptr` validates the magic
    // before trusting anything and yields None for a pointer that is not a device tree.
    let tree = unsafe { fdt::Fdt::from_ptr(fdt) };
    if let Some(t) = tree.as_ref() {
        let mut props = [None, None];
        if let Some(u) = t
            .find_compatible("snps,dw-apb-uart", &["reg-shift", "reg-io-width"], &mut props)
            .or_else(|| t.find_compatible("ns16550a", &["reg-shift", "reg-io-width"], &mut props))
        {
            uart_configure(u.base, props[0].unwrap_or(0), props[1].unwrap_or(1));
        }
    }

    for &b in b"
GodspeedOS riscv64: _start reached S-mode, 16550 UART alive - the demarcation BOOTS on a THIRD arch.
" {
        putc(b);
    }
    // Report what the firmware handed us, and CHECK the device tree rather than trusting the
    // pointer. A magic mismatch means the FDT is not where `a1` says, and every address read out of
    // it afterwards would be garbage pointed at real hardware - the kind of failure that presents as
    // an unexplained hang rather than as a wrong number.
    print_str("riscv64: boot hart ");
    print_dec(hartid as u64);
    print_str(", fdt at ");
    print_hex(fdt as u64);
    let Some(tree) = tree else {
        print_str(" (NO FDT MAGIC - device tree not usable)
");
        halt();
    };
    print_str(" (valid, ");
    print_dec(tree.total_size() as u64);
    print_str(" bytes)
");

    // Everything below is READ FROM THE MACHINE. No address here is a constant, which is the whole
    // point: the same code says different, correct things on QEMU `virt` and on the JH7110.
    if let Some(m) = tree.memory() {
        print_str("riscv64: ram ");
        print_hex(m.base);
        print_str(" + ");
        print_dec(m.size / (1024 * 1024));
        print_str(" MiB
");
    }

    let (harts, max_hart) = tree.usable_harts();
    print_str("riscv64: ");
    print_dec(harts as u64);
    print_str(" usable hart(s), highest id ");
    print_dec(max_hart as u64);
    if let Some(c) = tree.boot_cpuid() {
        print_str(", fdt says boot cpu ");
        print_dec(c as u64);
    }
    print_str("
");

    if let Some(hz) = tree.timebase_frequency() {
        print_str("riscv64: timebase ");
        print_dec(hz as u64);
        print_str(" Hz
");
    }

    // The UART, by PROGRAMMING MODEL rather than by address. Two compatibles because two machines
    // implement two different 16550s - which is a driver supporting its hardware, not an arch
    // knowing which board it is on.
    let mut props = [None, None];
    let uart = tree
        .find_compatible("snps,dw-apb-uart", &["reg-shift", "reg-io-width"], &mut props)
        .or_else(|| tree.find_compatible("ns16550a", &["reg-shift", "reg-io-width"], &mut props));
    if let Some(u) = uart {
        print_str("riscv64: uart ");
        print_hex(u.base);
        print_str(" shift ");
        print_dec(props[0].unwrap_or(0) as u64);
        print_str(" width ");
        print_dec(props[1].unwrap_or(1) as u64);
        print_str("
");
    }

    let mut none = [];
    if let Some(p) = tree.find_compatible("riscv,plic0", &[], &mut none)
        .or_else(|| tree.find_compatible("sifive,plic-1.0.0", &[], &mut none))
    {
        print_str("riscv64: plic ");
        print_hex(p.base);
        print_str("
");
    }

    // What the FIRMWARE says is off limits, reported separately from the map it feeds. Printed
    // because an empty list is a fact worth seeing: it means nothing but our own bookkeeping is
    // protecting the memory OpenSBI is running from.
    {
        let mut r = [fdt::Reg { base: 0, size: 0 }; 8];
        let n = tree.reservations(&mut r);
        print_str("riscv64: firmware reserved ");
        print_dec(n as u64);
        print_str(" region(s)");
        for e in &r[..n] {
            print_str(" ");
            print_hex(e.base);
            print_str("+");
            print_hex(e.size);
        }
        print_str("
");
    }

    // The memory map the neutral kernel will be handed, printed before it is used. An allocator
    // given a wrong map fails LATER and somewhere else, so the map is stated where it is built.
    match build_boot_info(&tree, fdt) {
        Some(bi) => {
            for r in bi.memory_map {
                print_str("riscv64: mem ");
                print_hex(r.base);
                print_str("..");
                print_hex(r.base + r.len);
                print_str(match r.kind {
                    MemoryKind::Usable => "  usable",
                    MemoryKind::KernelImage => "  kernel",
                    _ => "  reserved",
                });
                print_str(" (");
                print_dec(r.len / 1024);
                print_str(" KiB)
");
            }
        }
        None => print_str("riscv64: could not build a memory map from the device tree
"),
    }

    for &b in b"riscv64: neutral kernel linked; arch/riscv64 stubs pending real bodies. halting.
" {
        putc(b);
    }
    halt();
}

/// Stop this hart. Not a panic: there is nothing above the arch layer yet to report to.
fn halt() -> ! {
    loop {
        // SAFETY: `wfi` is architecturally a hint; waking spuriously simply re-enters the loop.
        unsafe { core::arch::asm!("wfi") };
    }
}


/// FDT header magic and total size, or `None` if `p` does not point at a device tree.
///
/// Big-endian by specification, on a little-endian machine, so every field needs swapping - a fact
/// worth stating because reading one field the wrong way round yields a plausible-looking number.
fn fdt_total_size(p: *const u8) -> Option<u32> {
    if p.is_null() {
        return None;
    }
    // SAFETY: reading 8 bytes at the pointer the boot protocol supplied in `a1`. If it is not an
    // FDT the magic check below rejects it before anything acts on the contents.
    let (magic, total) = unsafe {
        (
            u32::from_be((p as *const u32).read_volatile()),
            u32::from_be((p as *const u32).add(1).read_volatile()),
        )
    };
    if magic == 0xd00d_feed { Some(total) } else { None }
}

fn print_str(s: &str) {
    for &b in s.as_bytes() {
        putc(b);
    }
}

fn print_dec(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if v == 0 {
        putc(b'0');
        return;
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    for &b in &buf[i..] {
        putc(b);
    }
}

fn print_hex(v: u64) {
    print_str("0x");
    let mut started = false;
    for shift in (0..16).rev() {
        let n = ((v >> (shift * 4)) & 0xf) as u8;
        if n != 0 || started || shift == 0 {
            started = true;
            putc(if n < 10 { b'0' + n } else { b'a' + n - 10 });
        }
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

/// Who made this CPU - see the x86 implementation for what this is for. RISC-V reports its vendor in
/// `mvendorid`, which is an M-mode CSR: this port runs under OpenSBI in S-mode and cannot read it, so
/// the ISA is all that can be said honestly here.
pub fn cpu_identity(buf: &mut [u8]) -> usize {
    let name = b"RISC-V";
    let n = name.len().min(buf.len());
    buf[..n].copy_from_slice(&name[..n]);
    n
}

/// The SD/EMMC controller's base clock in Hz, or 0 where the platform does not report one
/// (the block driver then refuses to guess a divider). Only the Pi's ARM port learns this,
/// from the VideoCore mailbox at boot.
pub fn emmc_base_clock_hz() -> u32 { 0 }
/// No board mailbox on this architecture: the driver uses whatever the chip holds. See query 23.
pub fn board_mac_packed() -> Option<u64> { None }

/// USB mass-storage block device (the ARM DWC2 Bulk-Only bridge). Only the Pi's ARM port has an
/// in-kernel USB stack; elsewhere disks are userspace drivers, so there is no device here.
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

/// The ELF `e_machine` and `EI_CLASS` this arch's service binaries carry (RISC-V, ELFCLASS64).
/// The neutral loader checks a candidate ELF against these, so it can parse a 32-bit ARM
/// service ELF or a 64-bit one without any arch-specific code in the loader itself.
pub const ELF_MACHINE: u16 = 243;
pub const ELF_CLASS: u8 = 2; // 1 = ELFCLASS32, 2 = ELFCLASS64

/// A11-1 hook: called from the timer tick on every core so a panic can stop the machine, not just the
/// panicking core. A no-op on this port until its `halt_all_cores` actually signals the other cores -
/// see the aarch64 implementation for the shape (a published flag, checked here).
pub fn panic_halt_check() {}

pub fn halt_all_cores() -> ! { loop { core::hint::spin_loop(); } }
pub fn hardware_reset() -> ! { loop { core::hint::spin_loop(); } }

// ---- Serial / console (NS16550 on QEMU virt @ 0x1000_0000; stubbed) ----
pub fn serial_write_byte(b: u8) { putc(b); }
pub fn serial_write_bytes_lockfree(s: &[u8]) { for &b in s { putc(b); } }
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
    /// MSI vector pool. Empty: RISC-V delivers device interrupts through the PLIC by wire, and MSI
    /// (via AIA/IMSIC) is a separate controller this port does not have yet. A zero-length pool means
    /// the neutral allocator hands out nothing rather than handing out vectors nobody routes.
    pub const MSI_POOL_BASE: u8 = 0;
    pub const MSI_POOL_LEN: usize = 0;

    pub const XHCI_MSI_VECTOR: u8 = 0x28;
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

// PCI seam. QEMU `virt` DOES have a PCIe host bridge (ECAM at 0x3000_0000, described in the FDT), and
// the VisionFive 2 has one too - so unlike arm32 this is a stub by STAGE, not by platform. Everything
// answers "nothing here", which is honest for a kernel that has not yet read the device tree: it
// reports no devices rather than guessing at fixed addresses.
//
// Ported from `arch/arm`'s no-PCI seam so the surface matches exactly. Filling it in means walking the
// FDT for `pci-host-ecam-generic` and enumerating from there - the arch's own work, not the neutral
// kernel's, which is the whole point of this boundary.
// ---------------------------------------------------------------------------
// Seam members the neutral kernel grew after this stub was written.
//
// Every one of these is a compile error, not a runtime one, which is the boundary working: the neutral
// layers may only reach hardware through `arch::imp`, so a new member is felt by every arch at once.
// What it did NOT do is TELL anyone - nothing builds this target, so the stub rotted silently until
// someone tried. Wiring riscv64 into a build path is therefore worth more than any single body below.
// ---------------------------------------------------------------------------

/// Interrupt vector taken by this core, and the last one seen. Stubbed until the PLIC is real.
pub fn note_irq(_vector: u32) {}
pub fn core_irq_debug(_core: u32) -> (u32, u32) { (0, 0) }

/// Publish the boot hart's identity before any secondary starts.
///
/// Nothing to do here YET, and for a reason worth stating rather than leaving as an empty body: on
/// RISC-V the hart id arrives in `a0` at entry rather than being read back from an interrupt
/// controller, so there is no equivalent of the x86 bug this exists to prevent (a core marked ready
/// whose identity was never written). When SMP lands, the boot hart records its id here.
pub fn publish_bsp_lapic_id() {}

/// PCI config read. See the `pci` module: no bus is enumerated until the FDT is parsed.
pub fn pci_cfg_read32(_sel: u32, _off: u16) -> Option<u32> { None }

/// Bytes emitted by the panic-path serial writer that bypasses the lock.
pub fn serial_unlocked_emit_count() -> u64 { 0 }

/// Copy from a user address into kernel memory, refusing anything not mapped to the caller.
///
/// Returns false until S-mode user pages exist. Refusing is the safe direction: a caller that cannot
/// read user memory fails its syscall, where a caller that wrongly SUCCEEDS reads someone else's.
pub fn copy_user_to_kernel(_src: u64, _dst: *mut u8, _len: usize) -> bool { false }

/// Is a driver's DMA arena mapped uncached? True until Sv39 attributes are wired, because assuming
/// COHERENT when it is not gives a driver silently stale descriptors - the failure that cannot be
/// debugged from a log.
pub const DMA_ARENA_UNCACHED: bool = true;
/// Virtual base at which a driver's DMA arena is mapped.
pub const DRIVER_DMA_VA: u64 = 0x7000_0000;

pub mod pci {
    use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32};
    use portable_atomic::AtomicU64;

    /// No PCI on this port - see the x86 originals. `None` is the honest answer, and the callers all
    /// treat it as "this machine has no PCI ethernet controller", which is true.
    pub fn ehci() -> Option<PciDevice> { None }
    pub fn xhci() -> Option<PciDevice> { None }
    pub fn nic() -> Option<PciDevice> { None }
    pub fn first_memory_bar(_d: &PciDevice) -> u64 { 0 }

    // ---- The generic device table (step D1). See `arch/x86_64/pci.rs` for the real one.
    /// One device as the bus reports it. Same shape on every arch so the spawn path is arch-neutral.
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
    pub static DEVICE_COUNT: AtomicU32 = AtomicU32::new(0);
    pub fn device_at(_n: usize) -> Option<PciDevice> { None }
    /// ARM32 HAS NO PCI AT ALL - the DWC2 is soldered to the BCM283x and there is no bus to walk.
    /// So this is not "unimplemented", it is EMPTY BY CONSTRUCTION: no class code can ever match,
    /// and every driver on this port names a non-PCI kind (`HwClass::Dwc2`). One slot, because the
    /// array it sizes must exist and nothing will ever fill it.
    pub const MAX_DEVICES: usize = 1;
    pub fn find_by_class(_class_code: u32) -> Option<PciDevice> { None }

    pub fn init() {}
    pub fn clear_bus_master(bdf: u32) {}
    pub fn set_bus_master(bdf: u32) {}
    pub fn set_power_d0(bdf: u32) {}
    pub fn xhci_bios_handoff() {}
    pub fn ehci_flr_probe() {}
    pub fn program_msi(_bdf: u32, _vector: u8, _dest: u8) -> bool { false }
    pub fn program_msix(_bdf: u32, _vector: u8, _dest: u8) -> bool { false }
    /// No LAPIC on ARM; the pool is x86-only until this port grows a generic MSI path.
    pub fn msi_dest_lapic(_core_id: u32) -> u8 { 0 }
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

// ============================ BootInfo, built from the machine ============================
//
// The neutral kernel's boot sequence starts with `memory::init(boot_info)`, so a real `BootInfo` is
// the first thing standing between this port and shared code running on it. Every field below is
// either read from the device tree or taken from the link - none is a constant, which is what lets
// the same logic describe QEMU's 256 MiB at 0x8000_0000 and the board's 8 GiB at 0x4000_0000.

unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

/// Regions handed to the frame allocator. FIXED, no heap (§26.6.1): a machine that describes more
/// banks than this simply has the extra ones ignored, which is a bounded loss the boot reports,
/// rather than an allocation that can fail halfway through bring-up.
const MAX_REGIONS: usize = 16;
static mut REGIONS: [MemoryRegion; MAX_REGIONS] = [MemoryRegion {
    base: 0,
    len: 0,
    kind: MemoryKind::Reserved,
}; MAX_REGIONS];

/// Describe memory to the neutral kernel: what RAM exists, minus what is already spoken for.
///
/// THREE THINGS MUST NOT BE HANDED OUT and each would fail differently if it were. The kernel image
/// itself, because allocating it returns frames we are executing from. The boot stack, which is why
/// `__kernel_end` sits past `__stack_top` rather than at the end of `.bss`. And the device tree,
/// because the firmware placed it in RAM and nothing else marks it - overwrite it and every fact
/// this port depends on becomes garbage, at a point far from the write.
fn build_boot_info(tree: &fdt::Fdt, fdt_ptr: *const u8) -> Option<BootInfo> {
    let ram = tree.memory()?;
    let k_start = (&raw const __kernel_start) as u64;
    let k_end = (&raw const __kernel_end) as u64;

    // The device tree's own extent, rounded out to whole pages so a partial page is never reused.
    let fdt_start = (fdt_ptr as u64) & !0xfff;
    let fdt_end = ((fdt_ptr as u64) + tree.total_size() as u64 + 0xfff) & !0xfff;

    let mut n = 0usize;
    let mut push = |base: u64, len: u64, kind: MemoryKind| {
        if len > 0 && n < MAX_REGIONS {
            // SAFETY: single-threaded boot, before any other hart is started; this static is
            // written once here and only read afterwards.
            unsafe { REGIONS[n] = MemoryRegion { base, len, kind } };
            n += 1;
        }
    };

    // Everything that must be carved out of RAM, gathered before any of it is used.
    //
    // THE FIRMWARE'S OWN RESERVATIONS ARE THE ONE THAT BITES. OpenSBI runs in M-mode from RAM and
    // lists itself in the FDT reservation block - 0x8000_0000 on QEMU, 0x4000_0000 on the JH7110 -
    // and nothing else in the tree marks it. A first version of this function omitted them and
    // cheerfully described the firmware's memory as usable; the frame allocator would have handed
    // out the code we make SBI calls into, and the failure would have surfaced far from the write.
    let mut resv = [fdt::Reg { base: 0, size: 0 }; 8];
    let nres = tree.reservations(&mut resv);

    const MAX_CUTS: usize = 12;
    let mut cuts = [(0u64, 0u64, MemoryKind::Reserved); MAX_CUTS];
    let mut ncuts = 0usize;
    let mut add_cut = |start: u64, end: u64, kind: MemoryKind, cuts: &mut [(u64, u64, MemoryKind); MAX_CUTS], n: &mut usize| {
        if end > start && *n < MAX_CUTS {
            cuts[*n] = (start, end, kind);
            *n += 1;
        }
    };
    // EVERYTHING BELOW THE KERNEL IS RESERVED, and this is a deliberate refusal to guess rather
    // than a measurement. The firmware runs from RAM: OpenSBI sits at 0x8000_0000 on QEMU and
    // 0x4000_0000 on the JH7110, immediately below where the kernel is loaded. Nothing tells us its
    // extent - QEMU's OpenSBI reserves ZERO regions in the FDT, which the boot line above reports,
    // so the mechanism that exists for exactly this says nothing.
    //
    // The costs are wildly asymmetric. Being conservative loses the 2 MiB the Image header's
    // `text_offset` already says we are placed past; being wrong hands the frame allocator the code
    // we make SBI calls into, and the corruption surfaces far from the write. So an unprovable
    // region is reserved, never usable. If a machine ever describes that space properly, this
    // becomes dead weight rather than a bug - which is the right way round.
    add_cut(ram.base, k_start, MemoryKind::Reserved, &mut cuts, &mut ncuts);
    add_cut(k_start, k_end, MemoryKind::KernelImage, &mut cuts, &mut ncuts);
    add_cut(fdt_start, fdt_end, MemoryKind::Reserved, &mut cuts, &mut ncuts);
    for r in &resv[..nres] {
        let s = r.base & !0xfff;
        let e = (r.base.saturating_add(r.size).saturating_add(0xfff)) & !0xfff;
        add_cut(s, e, MemoryKind::Reserved, &mut cuts, &mut ncuts);
    }

    // Sort by start. An insertion sort over at most twelve entries, because the walk below assumes
    // address order and a fixed tiny array does not justify anything cleverer (§26.13).
    for i in 1..ncuts {
        let mut j = i;
        while j > 0 && cuts[j - 1].0 > cuts[j].0 {
            cuts.swap(j - 1, j);
            j -= 1;
        }
    }

    // Walk RAM once, cutting out each reserved span in address order. Done by comparison rather
    // than by assuming a layout: the kernel sits at the bottom of RAM on the board and the device
    // tree above it, but nothing guarantees either.
    let ram_end = ram.base.saturating_add(ram.size);
    let mut cur = ram.base;
    for (start, end, kind) in cuts[..ncuts].iter().copied() {
        let start = start.max(ram.base).min(ram_end);
        let end = end.max(ram.base).min(ram_end);
        if end <= start {
            continue;
        }
        if start > cur {
            push(cur, start - cur, MemoryKind::Usable);
        }
        push(start, end - start, kind);
        cur = cur.max(end);
    }
    if ram_end > cur {
        push(cur, ram_end - cur, MemoryKind::Usable);
    }

    // SAFETY: as above - written once during single-threaded boot, read-only from here.
    let map: &'static [MemoryRegion] = unsafe { &*core::ptr::addr_of!(REGIONS[..n]) };
    Some(BootInfo {
        memory_map: map,
        kernel_phys_start: k_start,
        kernel_phys_end: k_end,
        // IDENTITY-MAPPED FOR NOW: S-mode entry runs with `satp` still zero, so virtual equals
        // physical and the direct-map offset is nothing. This becomes a real offset when Sv39 is
        // set up, and it is a field rather than an assumption precisely so that change is one line.
        hhdm_offset: 0,
        rsdp_addr: 0,
    })
}
