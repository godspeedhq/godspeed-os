// SPDX-License-Identifier: GPL-2.0-only
//! RISC-V (rv64) arch layer - STUB scaffold that BOOTS in QEMU `virt` (docs/aarch64.md pattern).
//!
//! The THIRD architecture. Exposes the SAME `arch::imp` surface as arch/x86_64/ and arch/aarch64/, so
//! the arch-NEUTRAL kernel compiles for riscv64 with only this file written - the boundary, generalised
//! to a third ISA. Bodies are stubs; real bodies (Sv39 MMU, S-mode trap vec, PLIC/CLINT, SBI) come later.

#![allow(unused_variables, dead_code)]

pub mod fdt;
pub mod sbi;
pub mod sv39;
pub mod context_switch;
pub mod display;
mod net;
mod usb;
pub mod syscall;
pub mod trap;
pub mod usermode;

use core::sync::atomic::{AtomicU32, AtomicUsize, AtomicBool, Ordering};
use portable_atomic::AtomicU64;

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
    // There is now a path by which a keystroke can arrive. On a machine whose console is a USB
    // keyboard this is announced by that keyboard's SERVICE; here the console is this UART, so the
    // kernel is the only thing that can say so - and the shell stops reporting that no input driver
    // announced itself when none ever will.
    INPUT_READY.store(true, Ordering::Release);
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

/// Held across a whole string so two harts cannot interleave their output.
///
/// **Four cores turned the log into a hazard.** With one hart, `print_str` looping over `putc` was
/// fine by construction. With four, a line from one hart lands inside a line from another, character
/// by character - observed the first time all four came up:
///
/// ```text
/// srimpsc:v h64ar: t en3 tereriadngy  tashe c sorche e2d
/// ```
///
/// which is `smp: hart 3 ready as core 2` and `riscv64: entering the scheduler` woven together. That
/// is worse than ugly. The log is the primary instrument on this port - there is no debugger and no
/// display - and this project has already had a spliced line make a test REPORT PASS on output that
/// was not what the test produced. An instrument that corrupts what it measures is the worst kind.
///
/// The neutral logger already stages a whole message and flushes it once for exactly this reason, so
/// what was missing was only this side of the bargain: the arch taking the lock ONCE for the string
/// rather than per byte. Per-byte locking would still let two lines interleave in the gaps.
static SERIAL_LOCK: AtomicBool = AtomicBool::new(false);

/// Spins before giving up and writing anyway.
///
/// **Bounded, and it proceeds on expiry rather than waiting.** A hart that dies holding this lock
/// must not silence the machine - the output that would be lost is exactly the output explaining why
/// it died. So a contended writer eventually writes regardless, accepting a spliced line in the case
/// where the alternative is no line at all. Same trade x86 makes, for the same reason.
const SERIAL_LOCK_SPIN_CAP: u32 = 2_000_000;

/// Take the lock if it can be had within the bound. Returns whether it was.
fn serial_lock_acquire() -> bool {
    let mut t = 0u32;
    while t < SERIAL_LOCK_SPIN_CAP {
        if SERIAL_LOCK
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
        core::hint::spin_loop();
        t += 1;
    }
    false
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

/// LSR bit 0: a received byte is waiting in the receive buffer.
const LSR_DR: u8 = 1 << 0;

/// Receive-buffer register. Index 0, like the transmit register it shares an address with - the
/// 16550 tells them apart by the direction of the access, which is why this reads where `uart_thr`
/// writes.
#[inline]
fn uart_rbr() -> u8 {
    let addr = uart_reg(0);
    // SAFETY: MMIO the firmware left mapped, at the address and width the device tree declares.
    // Reading it CONSUMES a byte from the FIFO, so it is only ever called with `LSR_DR` set.
    unsafe {
        if UART_WIDTH.load(Ordering::Relaxed) >= 4 {
            (addr as *const u32).read_volatile() as u8
        } else {
            (addr as *const u8).read_volatile()
        }
    }
}

/// Keystrokes read out of the UART and not yet consumed by a reader.
///
/// `AtomicU8` cells rather than a `static mut` array, so the ring needs no `unsafe` at all: the
/// producer is the timer tick (or a syscall draining directly) and the consumer is whichever task
/// holds `CONSOLE_READ`, and they genuinely run at different times on different stacks. A ring of
/// atomics states that plainly and costs nothing on a machine with the A extension.
///
/// 256 bytes: a fixed ceiling readable off the source (§26.6.1). A human types perhaps ten bytes a
/// second and the tick drains a hundred times a second, so this is roughly two seconds of a paste
/// burst, and overflowing it drops the OLDEST keystroke loudly rather than growing.
const RX_RING: usize = 256;
static RX_BUF: [core::sync::atomic::AtomicU8; RX_RING] =
    [const { core::sync::atomic::AtomicU8::new(0) }; RX_RING];
static RX_HEAD: AtomicU32 = AtomicU32::new(0);
static RX_TAIL: AtomicU32 = AtomicU32::new(0);
static RX_DROPPED: AtomicU32 = AtomicU32::new(0);

/// True once there is a path by which a keystroke can arrive.
///
/// On a machine whose console is a USB keyboard this is set by the keyboard SERVICE announcing
/// itself. Here the console is the serial port the kernel is already printing through, so the input
/// path exists from the moment the UART does - and saying so is what stops the shell reporting that
/// no input driver has announced itself when one never will.
static INPUT_READY: AtomicBool = AtomicBool::new(false);

/// Put one byte in the ring. Returns false if it was full.
fn rx_push(b: u8) -> bool {
    let tail = RX_TAIL.load(Ordering::Relaxed) as usize;
    let head = RX_HEAD.load(Ordering::Acquire) as usize;
    let next = (tail + 1) % RX_RING;
    if next == head {
        // FULL, and a KEYSTROKE is being dropped. Counted rather than silent: input that vanishes
        // with no trace is the kind of fault a user reports as "it missed a character sometimes",
        // which is unfalsifiable without a number to point at.
        RX_DROPPED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    RX_BUF[tail].store(b, Ordering::Relaxed);
    RX_TAIL.store(next as u32, Ordering::Release);
    true
}

/// Move everything the UART has received into the ring, and wake a blocked reader if anything
/// arrived.
///
/// **The wake is the half that matters.** A task blocked in `ConsoleRead` is parked, and the neutral
/// code that parked it says it expects to be woken "by the RX IRQ". This port has no PLIC yet, so
/// there is no RX IRQ - the drain runs from the timer tick instead, and if it did not wake the waiter
/// the shell would sit blocked forever with its keystroke sitting in the ring.
///
/// The loop is BOUNDED by the ring rather than by the FIFO: a UART wedged with `DR` permanently set
/// would otherwise spin here forever, inside a timer interrupt, and take the machine with it.
fn uart_rx_drain() {
    let mut got = false;
    for _ in 0..RX_RING {
        if uart_lsr() & LSR_DR == 0 {
            break;
        }
        let b = uart_rbr();
        got |= rx_push(b);
    }
    if got {
        let w = CONSOLE_READ_WAITER.load(Ordering::Acquire);
        if w != u32::MAX {
            crate::task::scheduler::wake_by_slot(w as usize, 0);
        }
    }
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
    // Recorded before anything can want it. `a0` is the only place this is true: the device tree
    // disagrees on this board, and there is no interrupt controller to read it back from.
    BOOT_HART.store(hartid as u32, Ordering::Relaxed);
    // This hart's own identity, in the place every hart keeps it. Set here rather than in `_start`
    // so there is one line that does it for the boot hart and one for a secondary, each next to the
    // code that knows the id - and `get_lapic_id` reads the same register on both.
    // SAFETY: writing `tp` on a kernel with no thread-local storage, before anything reads it.
    unsafe { core::arch::asm!("mv tp, {}", in(reg) hartid as u64, options(nomem, nostack)) };
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
riscv64: S-mode entered, 16550 UART alive
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

    // THE FOURTH BOOT PATH. `banner()` is the neutral kernel's identity line, and its own comment
    // says who calls it: "`kernel_main` on x86, and the two ARM `*_boot_main`s... three call sites
    // because there are three boot paths". There are four. This port brings the machine up itself and
    // never reaches `kernel_main`, so it opened with a line of its own invention and every log from it
    // began differently from every other board's - which is exactly the divergence `banner` exists to
    // prevent, arriving through the one route the comment did not anticipate.
    crate::banner();

    let tree_hz = tree.timebase_frequency();
    if let Some(hz) = tree_hz {
        // RECORD IT HERE, not when the scheduler tick starts several hundred lines below. Every
        // bounded wait between this point and there - the power domain, each reset, each PLL lock,
        // and the frame counter that measures the display - asks `timebase_hz()` for the machine's
        // rate, and until this store they were all being answered ZERO. The waits survived it on
        // their fallback constants; the frame counter did not, and reported `0.00 Hz` about a
        // display running at exactly 60. A rate read from the device tree is a fact the moment it is
        // read, and holding it back until the timer starts made it a fact only for the timer.
        TIMEBASE_HZ.store(hz, Ordering::Relaxed);
        print_str("riscv64: timebase ");
        print_dec(hz as u64);
        print_str(" Hz\n");
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
    // Bound BEFORE the match so it outlives it: the secondary harts are started much later, once the
    // scheduler's arenas exist, and `start_all_aps` takes the same `BootInfo` every arch's does.
    let boot_info = build_boot_info(&tree, fdt);
    match boot_info {
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

            USABLE_HARTS.store(harts.max(1), Ordering::Relaxed);
            // The IDs, not just how many. `hart_start` needs a number, and on this board they are
            // 1..4 with hart 0 disabled, so counting would name the wrong harts.
            {
                let mut ids = [0u32; 8];
                let n = tree.usable_hart_ids(&mut ids);
                HART_COUNT.store(n as u32, Ordering::Relaxed);
                for (i, id) in ids.iter().enumerate().take(n) {
                    HART_IDS[i].store(*id, Ordering::Relaxed);
                }
                print_str("riscv64: usable harts");
                for id in ids.iter().take(n) {
                    print_str(" ");
                    print_dec(*id as u64);
                }
                print_str("\n");
            }

            // THE FIRST NEUTRAL SUBSYSTEM TO RUN ON THIS ARCH. `memory::init` is shared code - the
            // same frame allocator x86, arm and aarch64 use - and it is reached here by handing it
            // facts, not by teaching it anything. Everything it needs came from the device tree or
            // the link, so nothing inside it knows which machine it is on.
            crate::memory::init(&bi);

            // Keep walking `kernel_main`'s own sequence. Each of these is shared code that needs
            // nothing from the MMU, so they run now rather than waiting behind Sv39 - and each one
            // that works is a subsystem this arch did not have to be taught.
            crate::smp::percpu_init(&bi);
            // This arch's own per-core arena, sized from the same live count. It carries the vector
            // an SBI IPI cannot.
            IPI_PENDING.init_with(ap_count() + 1, |_| AtomicU32::new(0));
            print_str("riscv64: percpu arenas sized for ");
            print_dec((ap_count() + 1) as u64);
            print_str(" core(s)
");

            // Publish this hart's id now the per-core arenas exist, so `current_core_id()` resolves
            // through a value the machine reported rather than a default. The board boots on hart 1,
            // so a default would name a hart the kernel is not running on.
            publish_bsp_lapic_id();

            // PCI Express, if this machine has any. The window's base comes from the tree, never
            // from a constant: QEMU `virt` puts it at 0x3000_0000 and a board will not, and a
            // hard-coded base is the mistake this port has already paid for three times.
            {
                let mut want: [Option<u32>; 0] = [];
                if let Some(reg) = tree.find_compatible("pci-host-ecam-generic", &[], &mut want) {
                    pci::set_ecam(reg.base, reg.size);
                    // The 32-bit memory window the bridge forwards, from `ranges`: triplets of
                    // <child 3 cells><parent 2><size 2>, where the top byte of the first child cell
                    // says which space it is. 0x02 is 32-bit memory, which is the one a BAR here can
                    // live in. Parsed where the meaning is known rather than in the tree reader.
                    if let Some(r) = tree.find_compatible_prop("pci-host-ecam-generic", "ranges") {
                        let mut off = 0usize;
                        while off + 28 <= r.len() {
                            let flags = u32::from_be_bytes([r[off], r[off+1], r[off+2], r[off+3]]);
                            let space = (flags >> 24) & 0x03;
                            let parent = u64::from_be_bytes([
                                r[off+12], r[off+13], r[off+14], r[off+15],
                                r[off+16], r[off+17], r[off+18], r[off+19]]);
                            let size = u64::from_be_bytes([
                                r[off+20], r[off+21], r[off+22], r[off+23],
                                r[off+24], r[off+25], r[off+26], r[off+27]]);
                            if space == 0x02 {
                                pci::set_mem_window(parent, size);
                                print_str("riscv64: pci mem window ");
                                print_hex(parent);
                                print_str("+");
                                print_hex(size);
                                print_str("\n");
                                break;
                            }
                            off += 28;
                        }
                    }
                } else {
                    print_str("riscv64: no pci-host-ecam-generic in the device tree - no PCI\n");
                }
            }
            pci::init();

            crate::capability::init();
            crate::ipc::init();
            print_str("riscv64: capability table and ipc routing initialised
");

            // EXERCISE THE WALKER BEFORE TRUSTING IT WITH `satp`. Writing that register is the one
            // step where a mistake gives no output at all: translation changes under the program
            // counter, and a wrong table faults on the next instruction fetch with nothing left to
            // report it. So the tables are built and read back while addressing is still identity
            // and a bug is merely a wrong number.
            sv39_selftest();

            // THE STEP THAT CAN GO SILENT. Everything after `csrw satp` runs through the table
            // built here, including the instruction fetch immediately following it, so the printing
            // is arranged so that WHICH LINE IS LAST tells you what failed.
            enable_paging(&bi);
        }
        None => print_str("riscv64: could not build a memory map from the device tree
"),
    }

    // Install the trap vector as early as there is a UART to report through. Everything before
    // this line faults silently; everything after it names itself.
    if trap::init() {
        print_str("riscv64: trap vector installed - faults will report
");
    } else {
        print_str("riscv64: TRAP VECTOR REFUSED - handler address is not 4-byte aligned
");
    }

    probe_rdcycle();
    calibrate_cycle_counter();

    // The last-level cache, and the window used to flush it. Both come from the cache controller's
    // own node: its first range is the registers and its second is a memory window that reads zeros
    // and exists solely to be written through. Wanted before the display, because the display is
    // what needs it.
    if let Some(reg) = tree.find_compatible_prop("starfive,jh7110-ccache", "reg") {
        // THREE ranges of four cells each, big-endian, and the one that matters is the THIRD.
        //
        // The cache controller's node lists its registers, then two 32 MiB windows. The first version
        // of this took the second range for the zero device, which is the obvious reading and the
        // wrong one: the vendor driver asks for index TWO. Pointed at the wrong window the flush
        // wrote thirty-three thousand zeros into somewhere harmless, evicted nothing, and cost 154
        // microseconds doing it - which from the serial console is indistinguishable from a flush
        // that works. The screen was the only instrument that could tell the difference.
        if reg.len() >= 48 {
            let cell = |i: usize| -> u64 {
                let mut v = 0u64;
                for b in 0..8 {
                    v = (v << 8) | reg[i * 8 + b] as u64;
                }
                v
            };
            ccache_init(cell(0), cell(4));
        }
    }

    // THE DISPLAY, first stage: the power domain everything else in VOUT sits behind. Nothing here
    // touches the display controller - reading an unpowered domain is a transaction with nothing to
    // answer it, not a zero - so this asks the always-on PMU instead, which is safe at any time.
    {
        let mut want: [Option<u32>; 0] = [];
        if let Some(reg) = tree.find_compatible("starfive,jh7110-pmu", &[], &mut want) {
            display::set_pmu_base(reg.base);
        }
        let mut w2: [Option<u32>; 0] = [];
        let sys = tree.find_compatible("starfive,jh7110-syscrg", &[], &mut w2).map(|r| r.base);
        let mut w3: [Option<u32>; 0] = [];
        let vout = tree.find_compatible("starfive,jh7110-voutcrg", &[], &mut w3).map(|r| r.base);
        display::set_crg_bases(sys.unwrap_or(0), vout.unwrap_or(0));
        // The display controller's register windows, so stage three can ask whether it answers. The
        // node lists three ranges; the first two are the controller, the third is the PMU it uses to
        // switch its own power domain - which is stage one's job, not this one's.
        let mut w4: [Option<u32>; 0] = [];
        if let Some(reg) = tree.find_compatible("starfive,jh7110-dc8200", &[], &mut w4) {
            // `find_compatible` hands back the FIRST range; the second is 0x800 further on, which is
            // where the controller proper lives (the tree says 0x2940_0000+0x100 then
            // 0x2940_0800+0x2000).
            display::set_dc_bases(reg.base, reg.base + 0x800);
        }
        if display::power_on_vout() && display::clocks_on() {
            display::probe_dc8200();
            // The system controller holding the PLL registers, and the display sub-system
            // controller. Neither is needed to program the display; both are needed to explain one
            // that does not run.
            let mut w5: [Option<u32>; 0] = [];
            let syscon =
                tree.find_compatible("starfive,jh7110-sys-syscon", &[], &mut w5).map(|r| r.base);
            let mut w6: [Option<u32>; 0] = [];
            let dss = tree.find_compatible("starfive,jh7110-dssctrl", &[], &mut w6).map(|r| r.base);
            display::set_syscon_bases(syscon.unwrap_or(0), dss.unwrap_or(0));
            let mut w7: [Option<u32>; 0] = [];
            if let Some(reg) = tree.find_compatible("starfive,jh7110-hdmi", &[], &mut w7) {
                display::set_hdmi_base(reg.base);
            }
            // THE USB HOST CONTROLLER, brought up next to the display for one reason: both are blocks
        // this SoC leaves switched off, and both are worth nothing until something says whether they
        // answer. What comes out of this is an address for a driver that already exists.
        {
            let mut w8: [Option<u32>; 0] = [];
            let crg = tree
                .find_compatible("starfive,jh7110-stgcrg", &[], &mut w8)
                .map(|r| r.base)
                .unwrap_or(0);
            let mut w9: [Option<u32>; 0] = [];
            let stg_syscon = tree
                .find_compatible("starfive,jh7110-stg-syscon", &[], &mut w9)
                .map(|r| r.base)
                .unwrap_or(0);
            // The controller sits behind a wrapper that translates addresses, so its window is the
            // wrapper's parent base plus the controller's own second range - taken from the tree
            // rather than added up by hand, because a constant here would be this board only.
            let be32 = |b: &[u8], i: usize| -> u64 {
                let mut v = 0u64;
                for k in 0..4 {
                    v = (v << 8) | b[i * 4 + k] as u64;
                }
                v
            };
            let mut xhci = 0u64;
            if let Some(ranges) = tree.find_compatible_prop("starfive,jh7110-usb", "ranges") {
                if let Some(reg) = tree.find_compatible_prop("cdns,usb3", "reg") {
                    if ranges.len() >= 16 && reg.len() >= 12 {
                        // ranges: child address, then the parent address in two cells.
                        let parent = (be32(ranges, 1) << 32) | be32(ranges, 2);
                        // The controller's ranges are otg, xhci, dev - the second is the host half.
                        xhci = parent + be32(reg, 2);
                    }
                }
            }
            let mut w10: [Option<u32>; 0] = [];
            let phy = tree
                .find_compatible("starfive,jh7110-usb-phy", &[], &mut w10)
                .map(|r| r.base)
                .unwrap_or(0);
            usb::set_bases(crg, stg_syscon, xhci, sys.unwrap_or(0), syscon.unwrap_or(0), phy);
            let mut w11: [Option<u32>; 0] = [];
            if let Some(reg) = tree.find_compatible("starfive,jh7110-sys-pinctrl", &[], &mut w11) {
                usb::set_pinctrl_base(reg.base);
            }
        }

        }

        if display::mode_set() {
            if display::hdmi_on() {
                // The framebuffer is live and on a wire: hand it to the kernel's boot console so
                // everything printed from here appears on the screen as well as the serial line.
                display::adopt_as_boot_console();
            }
        } else {
            display::diagnose();
        }

        // THE USB CONTROLLER LAST, and the ordering is the lesson from the boot it cost. Reading an
        // unclocked window on this interconnect does not fault - it stalls, with no output at all, so
        // the machine stopped dead and the television stayed black. Running it after the display
        // means the same stall now leaves the entire boot log on the screen AND on the serial line,
        // ending with the line that says what was about to be read. That is the difference between a
        // failure and a mystery, and it costs nothing but a position in the boot.
        usb::init();

        // THE ETHERNET MAC, last for the same reason USB is late: a register read into an unclocked
        // block on this interconnect stalls rather than faulting, so anything that might do it goes
        // after the display, where a stall still leaves the whole boot log on the screen.
        {
            let mut w12: [Option<u32>; 0] = [];
            let aon = tree
                .find_compatible("starfive,jh7110-aoncrg", &[], &mut w12)
                .map(|r| r.base)
                .unwrap_or(0);
            let mut w13: [Option<u32>; 0] = [];
            let mac = tree
                .find_compatible("starfive,jh7110-dwmac", &[], &mut w13)
                .map(|r| r.base)
                .unwrap_or(0);
            // The system controller again, looked up here rather than carried down from the display
            // block that found it first: this stage runs whether or not that one did, and a base
            // borrowed across an `if` it does not control is a base that is zero on the day the
            // display is skipped.
            let mut w14: [Option<u32>; 0] = [];
            let net_syscon = tree
                .find_compatible("starfive,jh7110-sys-syscon", &[], &mut w14)
                .map(|r| r.base)
                .unwrap_or(0);
            net::set_bases(aon, sys.unwrap_or(0), mac, net_syscon);
            net::init();
        }
    }

    // What the firmware beneath us offers. Probed rather than assumed: the two machines disagree
    // about their own capabilities, and calling into a missing extension is how a boot goes quiet.
    {
        let (maj, min) = sbi::spec_version();
        print_str("riscv64: sbi v");
        print_dec(maj);
        print_str(".");
        print_dec(min);
        print_str(", timer extension ");
        let has_timer = sbi::probe(sbi::EXT_TIME);
        print_str(if has_timer { "present" } else { "ABSENT" });
        print_str("
");

        // Reading `time` is the first thing this port does that the FIRMWARE can refuse: it is
        // permitted from S-mode only if `mcounteren` allows it. If it refuses, the trap vector
        // installed above reports an illegal instruction by name - which is precisely why the
        // vector was built before the timer rather than after.
        let t0 = sbi::time();
        let t1 = sbi::time();
        print_str("riscv64: time csr readable, ticks ");
        print_dec(t0);
        print_str(" -> ");
        print_dec(t1);
        print_str(if t1 > t0 { "  (advancing)
" } else { "  (NOT ADVANCING)
" });
    }


    // Start the scheduler tick at the rate the MACHINE reports, then let it run. The deliberate
    // fault below is what ends the boot, so the ticks in between prove the timer is periodic
    // rather than a single interrupt that happened to arrive.
    if let Some(hz) = tree_hz {
        if start_timer(hz) {
            print_str("riscv64: timer started, 10ms quantum from a ");
            print_dec(hz as u64);
            print_str(" Hz timebase
");
            // Spin briefly so several ticks land before the fault ends the boot. A count, not a
            // duration - it is bounded and its only job is to let interrupts arrive.
            for _ in 0..40_000_000u64 {
                core::hint::spin_loop();
            }
        } else {
            print_str("riscv64: TIMER REFUSED - no TIME extension or set_timer failed
");
        }
    }

    // THE FIRST CODE ON THIS ISA THAT IS NOT THE KERNEL. Runs here because it needs everything
    // above it: paging on (so USER permissions exist to honour), the trap vector installed (so the
    // way back in leads somewhere), and the tick running (so a timer interrupt taken FROM user mode
    // is exercised too, rather than left as the one path nothing has entered).
    // The context switch, before user mode: it is the mechanism a scheduler is, and proving it on
    // two KERNEL tasks isolates the register half from the MMU half. Both tasks share the kernel's
    // one address space, so nothing here depends on a `satp` switch working.
    context_switch::selftest();
    context_switch::address_space_selftest();

    usermode::selftest();
    usermode::task_selftest();

    // THE KERNEL'S ONE DIRECT SPAWN. Everything above is scaffolding proving a mechanism; this is the
    // first time neutral kernel code is asked to do the real thing on this ISA - parse a 2 MB ELF,
    // build an address space for it, and make a task out of it. Loud on failure by construction: a
    // boot-time supervisor spawn failure is a panic (§11.3), so there is no quiet way for this to
    // half-work.
    // Per-core scheduler arenas, then this core's identity, then READY - in that order, because
    // `lapic_to_core_id` matches only a core that has been marked ready, and `spawn_supervisor`
    // places its task on core 0 through that mapping.
    crate::task::scheduler::init_arenas(crate::smp::percpu::num_cores());
    crate::smp::core::mark_ready(0);

    // RELEASE THE OTHER HARTS, once everything they touch on arrival exists: the kernel's address
    // space, the per-core arenas, the scheduler's tables and this core's own readiness. A hart
    // started before any of those reads them half-built, and the failure is a silent one on a core
    // with no console. Interrupts are enabled here so an arriving hart's first IPI has somewhere to
    // go; software interrupts are admitted on this hart for the same reason.
    trap::enable_software_interrupts();
    // SAFETY: the boot hart, once, with every structure a secondary reads on arrival already built.
    if let Some(bi) = boot_info.as_ref() {
        // SAFETY: the boot hart, once, with every structure a secondary reads on arrival built.
        unsafe { ap_boot::start_all_aps(bi) };
    }

    // WAIT FOR THEM BEFORE COUNTING THEM. `start_all_aps` asks the firmware to start each hart and
    // returns; the harts mark themselves ready some microseconds later, so counting immediately
    // reported `1 core ready` on a machine that was about to have four - a true statement about the
    // wrong instant. Bounded, so a hart that never arrives costs a tenth of a second and is then
    // simply not counted, which is what CLAUDE.md 11.3 says a boot does about a core that fails.
    {
        let want = HART_COUNT.load(Ordering::Relaxed) as u32;
        let hz = TIMEBASE_HZ.load(Ordering::Relaxed) as u64;
        let deadline = sbi::time().wrapping_add(if hz == 0 { 400_000 } else { hz / 10 });
        while crate::smp::core::ready_count() < want && sbi::time() < deadline {
            core::hint::spin_loop();
        }
    }

    // THE ONE CANONICAL SENTENCE, and this port was not saying it. `report_cores_ready` exists in the
    // neutral kernel precisely so that every architecture cannot help but agree on the wording - x86
    // used to say `kernel: N cores ready`, the Pi 2 `smp: N cores ready`, and the Pi 4 had grown a
    // third phrasing, so a reader comparing boot logs across boards had to translate before they
    // could compare. This port printed each hart's own arrival and then never printed the total at
    // all, which is the same divergence in its purest form: not a different spelling, an absent line.
    // GodspeedOS should read the same whatever it is running on.
    crate::smp::core::report_cores_ready();

    crate::task::spawn_supervisor();

    // HAND THE CORE OVER. Every tick from here is a preemption point rather than the boot's own,
    // and `run` does not return.
    print_str("riscv64: entering the scheduler\n");
    NEUTRAL_SCHED.store(true, Ordering::Relaxed);
    crate::task::scheduler::run(0)

    // THE DELIBERATE TRAP-VECTOR FAULT USED TO LIVE HERE, and its own comment said it would go the
    // moment the kernel had real work after this point. That moment is this line: `run` does not
    // return, so the fault was unreachable, and an unreachable proof is not one.
    //
    // Nothing is lost. The vector is still proved on EVERY boot, by a fault that is now part of a
    // real test rather than staged for its own sake: `usermode::selftest` has its user stub load a
    // kernel address, and the handler catching that fault is the same handler this block existed to
    // exercise.
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

/// The boot's own output, through the same one-hold path as everything else - so a boot line and a
/// service's log line cannot interleave either.
fn print_str(s: &str) {
    serial_write_bytes_lockfree(s.as_bytes());
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
#[derive(Clone, Copy)]
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
/// How many harts besides this one the machine says are usable.
///
/// FROM THE DEVICE TREE, not from a constant and not from OpenSBI's count. OpenSBI reports five on
/// the JH7110; the tree marks one of them `disabled` (an S7 monitor core), so four are ours and
/// three are APs. Reading `status` is what makes that distinction without the kernel knowing what a
/// JH7110 is - see `fdt::usable_harts`.
///
/// Zero until the tree has been read, which is honest: before that we genuinely do not know, and a
/// single-core boot is a supported configuration (§11.3) rather than an error.
static USABLE_HARTS: AtomicU32 = AtomicU32::new(1);

/// The hart this kernel was entered on, taken from `a0` at `_start`.
///
/// **NOT ZERO ON REAL HARDWARE.** The VisionFive 2 Lite boots on hart 1 - hart 0 is the S7 monitor
/// core - while QEMU `virt` boots on hart 0. So the emulator's value is exactly the one an
/// assumption would have picked, and a kernel that assumed it would work in QEMU and address the
/// wrong core on the board. The FDT header's `boot_cpuid_phys` is no help either: it reads 0 on this
/// board while `a0` and OpenSBI both say 1. The register is the only truth.
static BOOT_HART: AtomicU32 = AtomicU32::new(0);

/// The usable hart ids the device tree reported, and how many.
///
/// The IDS, because `hart_start` needs a number and on this board the numbers are 1..4 - hart 0 is
/// a disabled S7 monitor core. A count would say "four harts" and an index would name the wrong ones.
static HART_IDS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
static HART_COUNT: AtomicU32 = AtomicU32::new(0);

/// Which core id each hart answers to, indexed by hart id.
///
/// **A hart cannot look this up.** `lapic_to_core_id` only matches a core already marked READY, and
/// a hart cannot mark itself ready until it knows which core it is - so asking is circular, and the
/// lookup's fall-through to 0 makes the circle silent rather than an error: every secondary reported
/// itself "ready as core 0", four harts scribbled on one core's scheduler state, and the first
/// service to run died of a capability error that had nothing to do with capabilities.
///
/// ARM does not need this because MPIDR tells a core its own number. RISC-V has no such register in
/// S-mode, so the assignment is made by the hart that does the starting and read by the hart that
/// was started. Told, not derived.
const MAX_HART_ID: usize = 8;
static HART_TO_CORE: [AtomicU32; MAX_HART_ID] = [const { AtomicU32::new(u32::MAX) }; MAX_HART_ID];

/// Record that `hart` is `core`. Ignores a hart id past the table rather than writing out of bounds;
/// `start_all_aps` refuses to start such a hart, so the pair stays consistent.
fn set_hart_core(hart: u32, core: u32) {
    if (hart as usize) < MAX_HART_ID {
        HART_TO_CORE[hart as usize].store(core, Ordering::Release);
    }
}

/// The core id assigned to `hart`, or `None` if nothing assigned one.
fn hart_core(hart: u32) -> Option<u32> {
    if (hart as usize) >= MAX_HART_ID {
        return None;
    }
    match HART_TO_CORE[hart as usize].load(Ordering::Acquire) {
        u32::MAX => None,
        c => Some(c),
    }
}

pub fn ap_count() -> usize {
    (USABLE_HARTS.load(Ordering::Relaxed).saturating_sub(1)) as usize
}
pub fn init(boot_info: &BootInfo) { unimplemented!("riscv64::init") }
pub fn init_timer() { unimplemented!("riscv64::init_timer") }
pub fn ap_init(core_id: u32) { unimplemented!("riscv64::ap_init") }

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
/// How long a core may go dark before the machine says so, in ticks of the counter the scheduler
/// stamps with.
///
/// **This returned 0, which DISABLED the cross-core watchdog on this port, and that is why a chaos
/// freeze here is a machine that goes quiet instead of one that says what happened.** The neutral
/// check treats 0 as "this arch cannot say" and skips - a deliberate per-arch answer, but nobody had
/// given this arch's real one. arm32 shipped its whole port that way and the note in
/// `task/scheduler.rs` is blunt about the cost: the Pi 2 "ran with NO liveness defence at all", a
/// chaos run wedged it for 20-30 s and then permanently, "and the machine that is supposed to fail
/// LOUD (invariant 12) went silent instead, which is what made it undiagnosable". Twice now on this
/// board a chaos run has ended with a log that simply stops, which is the same symptom and the same
/// missing instrument.
///
/// Ten seconds, matching both ARM ports. Derived from the MEASURED quantum rather than the device
/// tree's timebase, because that is the counter `read_cycle_counter` returns and therefore the one
/// the scheduler's timestamps are in - a deadline in a different clock from the stamps it is
/// compared against is the bug this port already had once, in userspace budgets.
///
/// Still 0 before calibration, which is honest: with no rate there is no way to express ten seconds,
/// and a guessed deadline would panic a healthy machine.
pub fn liveness_deadline_cycles() -> u64 {
    const LIVENESS_SECS: u64 = 10;
    const QUANTA_PER_SEC: u64 = 100; // a quantum is 10 ms
    boot::tsc_ticks_per_quantum().saturating_mul(QUANTA_PER_SEC * LIVENESS_SECS)
}

pub fn usb_disk_busy() -> bool { false }
/// Is there no USB disk attached at all? Distinct from busy - see `USB_DISK_ABSENT` in the syscall
/// dispatch. This arch has no USB-disk backend, so a request never reaches one and the question is
/// moot; the read/write primitives already answer false.
pub fn usb_disk_absent() -> bool { true }


// No GPIO on this arch (the ARM `gpio` shell command is Pi-only).
pub fn gpio_op(_op: u32, _pin: u32) -> i64 { -1 }
pub fn net_frame_rx(_dst: &mut [u8]) -> usize { 0 }
pub fn net_info() -> Option<([u8; 6], bool)> { None }
pub use syscall_entry::{
    copy_user_to_kernel, read_cycle_counter, read_user_bytes, validate_user_ptr, write_user_bytes,
};

/// Switch to a new stack top - `sp` on AArch64. `#[inline(always)]` for the same reason as x86.
/// # Safety: caller guarantees `top` is a valid aligned stack top; nothing live is on the old stack.
#[inline(always)]
pub unsafe fn switch_to_boot_stack(top: u64) { unimplemented!("riscv64::switch_to_boot_stack") }

/// The ELF `e_machine` and `EI_CLASS` this arch's service binaries carry (RISC-V, ELFCLASS64).
/// The neutral loader checks a candidate ELF against these, so it can parse a 32-bit ARM
/// service ELF or a 64-bit one without any arch-specific code in the loader itself.
pub const ELF_MACHINE: u16 = 243;
pub const ELF_CLASS: u8 = 2; // 1 = ELFCLASS32, 2 = ELFCLASS64

/// A11-1 hook: called from the timer tick on every core so a panic can stop the machine, not just the
/// panicking core. A no-op on this port until its `halt_all_cores` actually signals the other cores -
/// see the aarch64 implementation for the shape (a published flag, checked here).
pub fn panic_halt_check() {
    note_stage(stage::NEUTRAL_TICK);
    // AND STOP, IF THE MACHINE IS ALREADY DYING. x86 leaves this a stub because it halts its
    // siblings with an NMI broadcast; RISC-V has no NMI, which is exactly why this seam member
    // exists and why leaving it empty here left `halt_all_cores` halting only the hart that called
    // it - a recorded gap, and the reason a panicking machine kept running services on three harts
    // while one printed its post-mortem. Every other hart takes a timer tick within a quantum, so
    // polling here reaches all of them promptly without an IPI that a wedged hart could not answer.
    if DUMPED.load(Ordering::Relaxed) {
        note_stage(stage::PANIC_HALTED);
        loop {
            // SAFETY: `wfi` is a hint with no operands and no memory effect. Halting rather than
            // spinning keeps the core quiet while the winner writes its dump over the same UART.
            unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
        }
    }
}

/// Stop, and SAY WHAT EVERY HART WAS DOING on the way down.
///
/// This was a bare spin loop that printed nothing, which is a waste of the one moment when the
/// answer is still in the registers. The kernel reaches here from a panic - and on this port the
/// panic that matters is the liveness watchdog, which knows a core stopped but not where. Each
/// hart's last stage and interrupt count are exactly the two facts that turn "core 0 wedged" into a
/// phase of the trap handler.
///
/// Written through the LOCK-FREE serial path on purpose. A wedged hart may be holding the console
/// lock - that is one of the shapes being hunted - and a diagnostic that waits for a lock held by
/// the thing it is diagnosing prints nothing at all, which is how the machine came to go silent in
/// the first place.
pub fn halt_all_cores() -> ! {
    // ONE WRITER. Every hart that panics arrives here, and the liveness watchdog panics on EVERY
    // core that notices a dark one - so the last wedge printed this dump three times, concurrently,
    // through a lock-free writer that by design does not serialise. The result was unreadable:
    //
    //   5kernel: hart stages at halt (stage/irqs) -/kernel: hart stages at halt...86243 h h14 h=15=/5=/5
    //
    // Three correct dumps interleaved character by character are worth less than one, and this is
    // the output a wedge diagnosis depends on. The losers halt silently; the winner speaks. It has
    // to be lock-free (a wedged hart may hold the console lock, which is one of the shapes being
    // hunted) so the claim is a CAS rather than a lock - the one place where "first past the post,
    // everyone else quiet" is exactly right.
    if DUMPED.swap(true, Ordering::AcqRel) {
        loop {
            core::hint::spin_loop();
        }
    }
    serial_write_bytes_lockfree(b"kernel: hart stages at halt (stage/irqs) -");
    for hart in 0..MAX_HART_ID {
        let st = CORE_STAGE[hart].load(Ordering::Relaxed);
        let n = CORE_IRQ_COUNT[hart].load(Ordering::Relaxed);
        if st == 0 && n == 0 {
            continue; // a hart that never ran; saying so for all eight buries the ones that did
        }
        serial_write_bytes_lockfree(b" h");
        emit_dec_lockfree(hart as u64);
        serial_write_bytes_lockfree(b"=");
        emit_dec_lockfree(st as u64);
        serial_write_bytes_lockfree(b"/");
        emit_dec_lockfree(n as u64);
        // The syscall this hart is INSIDE, if any. Printed next to the stage because that is the
        // pair that identifies a stuck hart: stage 1 alone said "somewhere in the trap handler",
        // which was true and useless.
        let sc = CORE_SYSCALL[hart].load(Ordering::Relaxed);
        if sc != u32::MAX {
            serial_write_bytes_lockfree(b"/s");
            emit_dec_lockfree(sc as u64);
        }
        // AND WHERE THAT STAGE WAS STAMPED FROM. See `CORE_STAGE_RA`: a stage names a function, and a
        // function has callers - this names the CALL SITE, which cannot be ambiguous.
        let ra = CORE_STAGE_RA[hart].load(Ordering::Relaxed);
        if ra != 0 {
            serial_write_bytes_lockfree(b"@");
            serial_write_hex_lockfree(ra);
        }
    }
    serial_write_bytes_lockfree(b"\n  stages: 1 trap-entry 2 timer-rearmed 3 usermode-hook 4 fb-publish 5 neutral-sched 6 tick-done 7 trap-exit 8 syscall 9 ipi-drain 10 idle-wfi 11 timer-enter(pre-SBI) 12 fault-report 13 kill 14 in-drain 15 past-drain(pick_next) 16 halted-by-another-hart 17 pre-switch (syscall NR shown as sN)\n");
    // The idle sample, for any hart that ever halted. `now` is the wall clock as that hart last saw
    // it, so comparing it against `deadline` says whether the wake it was waiting for was already
    // due - and STIE (bit 5 of sie) says whether it could have been delivered at all.
    let now = sbi::time();
    serial_write_bytes_lockfree(b"  idle sample (hart: armed-in/last-seen-ago/stie/stip) now=");
    emit_dec_lockfree(now);
    for hart in 0..MAX_HART_ID {
        let d = IDLE_DEADLINE[hart].load(Ordering::Relaxed);
        let t = IDLE_TIME[hart].load(Ordering::Relaxed);
        if d == 0 && t == 0 {
            continue;
        }
        serial_write_bytes_lockfree(b" h");
        emit_dec_lockfree(hart as u64);
        serial_write_bytes_lockfree(b"=");
        // Signed-ish: a deadline already past when it halted is the interesting case, so say which
        // side of `now` it fell on rather than printing a huge wrapped number.
        if d >= now {
            serial_write_bytes_lockfree(b"+");
            emit_dec_lockfree(d - now);
        } else {
            serial_write_bytes_lockfree(b"-");
            emit_dec_lockfree(now - d);
        }
        serial_write_bytes_lockfree(b"/");
        emit_dec_lockfree(now.saturating_sub(t));
        serial_write_bytes_lockfree(if IDLE_SIE[hart].load(Ordering::Relaxed) & (1 << 5) != 0 {
            b"/STIE"
        } else {
            b"/no-stie"
        });
        serial_write_bytes_lockfree(if IDLE_SIP[hart].load(Ordering::Relaxed) & (1 << 5) != 0 {
            b"/STIP"
        } else {
            b"/no-stip"
        });
    }
    serial_write_bytes_lockfree(b"\n");
    loop {
        core::hint::spin_loop();
    }
}

/// One 64-bit value in hex, straight out of the port, taking no lock.
///
/// For the paths that must print when the ordinary console cannot be trusted: a fault raised while
/// reporting a fault, and the halt dump. Fixed width, because a reader comparing two `sepc` values
/// should not have to count digits, and because a fixed loop cannot get its own bounds wrong.
pub fn serial_write_hex_lockfree(v: u64) {
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let nib = ((v >> (60 - i * 4)) & 0xf) as u8;
        buf[2 + i] = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 };
    }
    serial_write_bytes_lockfree(&buf);
}

/// One unsigned number, straight out of the port, taking no lock. Only for the halt path above.
fn emit_dec_lockfree(v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut n = v;
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 || i == 0 {
            break;
        }
    }
    serial_write_bytes_lockfree(&buf[i..]);
}
pub fn hardware_reset() -> ! { loop { core::hint::spin_loop(); } }

// ---- Serial / console (NS16550 on QEMU virt @ 0x1000_0000; stubbed) ----
/// One byte, under the lock. Used for single characters; a whole message goes through the function
/// below so it cannot be split.
pub fn serial_write_byte(b: u8) {
    let got = serial_lock_acquire();
    putc(b);
    if got {
        SERIAL_LOCK.store(false, Ordering::Release);
    }
}

/// A whole string, in ONE lock hold.
///
/// The name says lock-free and the neutral logger's comment says "a single `SERIAL_LOCK` hold" - the
/// second is the contract. It means free of the KERNEL's locks, not of this one: the arch is the only
/// place that knows the device is a single shared resource, so serialising it is this function's job
/// and doing it per byte would leave exactly the gaps two harts interleave through.
pub fn serial_write_bytes_lockfree(s: &[u8]) {
    let got = serial_lock_acquire();
    for &b in s {
        putc(b);
    }
    if got {
        SERIAL_LOCK.store(false, Ordering::Release);
    }
    mirror_to_screen(s);
}

/// Everything printed before there was a screen to print it on.
///
/// **The display cannot come up first, and this is what the screen would otherwise lose.** Bringing
/// it up needs the frame allocator, which needs the memory map, which needs the device tree - so a
/// hundred lines about the machine's own discovery are already on the serial line before there is
/// anywhere else to put them, and the television used to join the boot part-way through, at the first
/// timer tick. Holding them costs sixteen kilobytes of always-allocated memory and buys a screen that
/// shows the whole boot rather than the end of it.
///
/// Bounded by construction: a fixed array that stops accepting when full, so a boot that talks more
/// than expected loses the tail of the replay rather than growing anything. The kernel ring buffer is
/// not an alternative - the arch's own `print_str` writes the UART directly and never enters it, and
/// draining it here would take it from the `events` service, which is the one thing it is for.
const EARLY_LOG_BYTES: usize = 16 * 1024;
static mut EARLY_LOG: [u8; EARLY_LOG_BYTES] = [0; EARLY_LOG_BYTES];
static EARLY_LOG_LEN: AtomicUsize = AtomicUsize::new(0);

/// Set once the display is up and the boot console owns the framebuffer. Until then every byte in
/// the boot goes to serial alone, which is what makes this safe to call from the very first line.
static SCREEN_READY: AtomicBool = AtomicBool::new(false);
/// Held while a core is painting. A second core writing at the same time gets its bytes on the serial
/// line and skips the screen rather than interleaving glyphs into a half-drawn one - the same trade
/// the ARM port makes, and for the same reason: serial already has the complete text.
static PAINTING: AtomicBool = AtomicBool::new(false);

pub(super) fn screen_ready() {
    // Replay first, then arm - so the boot the screen shows starts where the boot started rather than
    // wherever the display happened to finish coming up.
    let n = EARLY_LOG_LEN.load(Ordering::Relaxed);
    if n > 0 {
        // SAFETY: `n` bytes were written into this array by `mirror_to_screen` and nothing has
        // written it since; the screen is not armed yet, so no other writer is in the array.
        let held = unsafe {
            core::slice::from_raw_parts(core::ptr::addr_of!(EARLY_LOG).cast::<u8>(), n)
        };
        crate::bootcon::put_bytes(held);
    }
    SCREEN_READY.store(true, Ordering::Release);
}

/// Put the same bytes on the screen that just went to the serial line.
///
/// The kernel has ONE output path and this is a second sink on it, not a second path - which is why
/// the display gets the boot log, the panic message and a service's log lines without any of them
/// knowing a screen exists.
fn mirror_to_screen(s: &[u8]) {
    // The same flag guards both halves, so a byte is either kept for later or drawn now, never both
    // and never torn between two cores.
    if PAINTING.swap(true, Ordering::Acquire) {
        return;
    }
    if SCREEN_READY.load(Ordering::Acquire) {
        crate::bootcon::put_bytes(s);
    } else {
        let n = EARLY_LOG_LEN.load(Ordering::Relaxed);
        let room = EARLY_LOG_BYTES - n;
        let take = if s.len() < room { s.len() } else { room };
        if take > 0 {
            // SAFETY: exclusive while `PAINTING` is held, and `take` is clamped to the space left in
            // the array, so the copy cannot reach past it.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    s.as_ptr(),
                    core::ptr::addr_of_mut!(EARLY_LOG).cast::<u8>().add(n),
                    take,
                );
            }
            EARLY_LOG_LEN.store(n + take, Ordering::Relaxed);
        }
    }
    PAINTING.store(false, Ordering::Release);
}
/// Console output for a SERVICE - the path the shell's prompt and its echo take.
///
/// The kernel's own `kprintln` reaches the UART through `serial_write_byte`, which is why boot output
/// and service LOGS worked long before this did. A shell prompt is neither: it is console output,
/// routed through the `console` service and back down to here, and while this was empty the shell
/// prompted into nothing. The symptom was a system that booted perfectly and showed no prompt.
///
/// `to_fb` selects the framebuffer as well. It is honoured now that there is one: the serial write
/// mirrors to the screen on its own, so a caller that does NOT want the screen has to be given a path
/// that skips it rather than one that cannot reach it.
pub fn console_write_bytes_gated(s: &[u8], to_fb: bool) {
    // `to_fb` is IGNORED, and putting it back that way is deliberate. Honouring it looked tidier -
    // a caller that does not want the framebuffer gets a path that cannot reach it - but this port
    // mirrors the serial write to the screen rather than writing the screen separately, so the flag
    // selects nothing here. What it did select was whether the early-boot log captured the bytes,
    // which is not what any caller is asking about.
    let _ = to_fb;
    serial_write_bytes_lockfree(s);
}
pub fn set_console_echo(on: bool) { let _ = on; }
pub fn claim_console_foreground(task_slot: u32) {}
pub fn release_console_foreground() {}
pub fn release_console_foreground_if_owner(task_slot: u32) {}
pub fn console_foreground_allows(task_slot: u32) -> bool { true }
pub fn console_boot_complete() {}
/// Inject a byte as though it had been typed. The kernel uses this to deliver a newline that
/// re-prompts a shell, and a keyboard SERVICE would use it to deliver real keys.
pub fn console_push_byte(b: u8) {
    if rx_push(b) {
        let w = CONSOLE_READ_WAITER.load(Ordering::Acquire);
        if w != u32::MAX {
            crate::task::scheduler::wake_by_slot(w as usize, 0);
        }
    }
}
pub fn set_input_ready() { INPUT_READY.store(true, Ordering::Release); }
pub fn input_ready() -> bool { INPUT_READY.load(Ordering::Acquire) }
pub fn com2_init() {}
pub fn com2_try_read_byte() -> Option<u8> { None }
/// Take the oldest unread keystroke, if there is one.
pub fn uart_rx_pop() -> Option<u8> {
    let head = RX_HEAD.load(Ordering::Relaxed) as usize;
    let tail = RX_TAIL.load(Ordering::Acquire) as usize;
    if head == tail {
        return None;
    }
    let b = RX_BUF[head].load(Ordering::Relaxed);
    RX_HEAD.store(((head + 1) % RX_RING) as u32, Ordering::Release);
    Some(b)
}
/// Called from the core-0 timer tick. On this port that tick IS the input path - see
/// `boot::rearm_idle_timer` for why the idle tick is not slowed here.
pub fn uart_rx_poll() { uart_rx_drain(); }
/// Drain on demand, so capturing input never depends on the tick having run. The blocked
/// `ConsoleRead` path calls this before it parks, and the non-blocking read calls it before it
/// answers empty.
pub fn uart_rx_drain_now() { uart_rx_drain(); }

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
    /// Re-arm an idle core - at the QUANTUM on this port, not at the usual ~1 s idle rate.
    ///
    /// **Because here the idle tick IS the keystroke latency.** A task blocked in `ConsoleRead` is
    /// parked waiting to be woken "by the RX IRQ", and this port has no PLIC, so there is no RX IRQ:
    /// the timer tick is the only thing that drains the UART and wakes that task. Slowing the tick to
    /// a second while a core idles - which is exactly when a shell is waiting at a prompt - would put
    /// up to a second between a key being pressed and the shell seeing it, and typing would be
    /// unusable.
    ///
    /// So this deliberately declines the power saving until there is an interrupt that can replace
    /// it. The cost is 100 wakes a second on an idle core instead of one; the core still halts in
    /// between (`wait_for_interrupt`), so it is a slower sleep rather than a spin. When the PLIC
    /// lands and the UART can raise an interrupt, the slow idle tick becomes correct and this becomes
    /// the one-second re-arm the other ports use.
    pub fn rearm_idle_timer() {
        rearm_quantum_timer();
    }

    /// Re-arm at the 10 ms scheduler quantum (CLAUDE.md 9.1).
    ///
    /// There is no periodic mode to fall back on: the supervisor timer fires when `time` passes the
    /// deadline and then stays asserted, so every tick MUST set the next one or the machine
    /// live-locks in the handler. This is the same call the tick itself makes; it exists separately
    /// because the scheduler re-arms on LEAVING idle, before it has taken a tick to do it in.
    pub fn rearm_quantum_timer() {
        let interval = super::TICK_INTERVAL.load(Ordering::Relaxed) as u64;
        if interval != 0 {
            let when = super::sbi::time().wrapping_add(interval);
            // RECORDED, and the return value is no longer discarded. `set_timer` reports whether the
            // firmware accepted the call, and this threw that away - so a refused arm and a
            // successful one were the same line of code. A core that halts on a deadline the
            // firmware never took is a core that does not wake, which is the shape being hunted.
            let ok = super::sbi::set_timer(when);
            super::note_deadline(if ok { when } else { 0 });
        }
    }

    pub fn audit_wx() {}

    /// Ticks of the counter `read_cycle_counter` RETURNS, in one 10 ms quantum.
    ///
    /// **Those two must describe the same clock, and for a while they did not.** This used to return
    /// the device tree's timebase rate, with a comment explaining that `read_cycle_counter` reads
    /// `time` so both halves of any duration come from one counter. That was true when it was
    /// written and stopped being true the moment `rdcycle` was probed for: on this board the counter
    /// became the CPU's, running at roughly 1.5 GHz, while the rate reported here stayed at the
    /// 4 MHz timebase - a factor of nearly four hundred.
    ///
    /// Userspace computes a duration as `rate * ms / 10` and compares it against `read_tsc`, so
    /// every such duration came out hundreds of times too SHORT. The keyboard's typematic repeat is
    /// the one a person notices: half a second before a held key repeats became about a
    /// millisecond, so typing `ping 8` produced a burst of eights. The same arithmetic had already
    /// gone the other way earlier in this port, when a cycle-denominated AHCI link wait was answered
    /// in timebase ticks and became forty seconds per port. One mismatch, two opposite symptoms, and
    /// both invisible until something took long enough or short enough for a person to feel it.
    ///
    /// MEASURED rather than declared, which also makes it right whichever counter the probe settled
    /// on: it times `read_cycle_counter` against the timebase, so if `rdcycle` was refused and the
    /// counter fell back to `time`, the measurement simply returns the timebase's own rate.
    pub fn tsc_ticks_per_quantum() -> u64 {
        let measured = super::CYCLES_PER_QUANTUM.load(Ordering::Relaxed);
        if measured != 0 {
            return measured;
        }
        super::TICK_INTERVAL.load(Ordering::Relaxed) as u64
    }
    pub unsafe fn rearm_tsc_deadline() {}
    pub unsafe fn apic_send_eoi() {
        // NOT AN EOI - RISC-V clears the timer interrupt by re-arming the deadline, which
        // `timer_tick` already did. The neutral tick calls this immediately after
        // `drain_pending_kstack`, so the empty stub is a free boundary marker in the window a wedge
        // has now been found in twice. See `stage::TICK_PAST_DRAIN`.
        super::note_stage(super::stage::TICK_PAST_DRAIN);
    }
    /// The id of the hart this call is running on.
    ///
    /// # Safety
    /// Architecturally always safe; `unsafe` to match the seam every arch implements.
    pub unsafe fn get_lapic_id() -> u32 {
        // `tp`, which each hart sets to its own id at its own entry. RISC-V has no register that
        // reads back "which hart am I" - `mhartid` is M-mode only - so the id arrives in `a0` once,
        // at entry, and must be PARKED somewhere per-hart or it is lost. `tp` is the conventional
        // place, and it is free here because a kernel with no thread-local storage never uses it
        // (rustc reserves it, so it is never allocated for anything else either).
        let id: u64;
        // SAFETY: reading a general register with no side effects.
        unsafe { core::arch::asm!("mv {}, tp", out(reg) id, options(nomem, nostack)) };
        id as u32
    }
    /// Poke one hart with a vector.
    ///
    /// Two steps and the order matters: RECORD the vector, then send. A hart that takes the
    /// interrupt before the bit is set would find nothing to do and the vector would be lost.
    ///
    /// # Safety
    /// Architecturally safe; `unsafe` to match the seam every arch implements.
    pub unsafe fn send_ipi_to_lapic(lapic_id: u32, vector: u8) {
        let core = crate::smp::core::lapic_to_core_id(lapic_id) as usize;
        if core < crate::smp::percpu::num_cores() && super::IPI_PENDING.initialised() {
            super::IPI_PENDING.get(core).fetch_or(super::ipi_bit(vector), Ordering::Release);
        }
        super::sbi::send_ipi(1u64 << (lapic_id as u64 & 63), (lapic_id as u64) & !63);
    }

    /// Poke every hart but this one.
    ///
    /// # Safety
    /// Architecturally safe; `unsafe` to match the seam every arch implements.
    pub unsafe fn broadcast_ipi_all_but_self(vector: u8) {
        // SAFETY: reading this hart's own id.
        let me = unsafe { get_lapic_id() };
        for core in 0..crate::smp::percpu::num_cores() {
            let hart = crate::smp::core::core_lapic_id(core as u32);
            if hart == me {
                continue;
            }
            // SAFETY: as `send_ipi_to_lapic`.
            unsafe { send_ipi_to_lapic(hart, vector) };
        }
    }
    /// x86 tells the HARDWARE where a ring-3 interrupt lands by writing `TSS.rsp0`. The RISC-V
    /// equivalent is `sscratch` - and writing it here would be WRONG.
    ///
    /// `sscratch` must read zero for as long as kernel code is running (`trap.rs`), because that is
    /// how the trap entry knows it is already on a kernel stack. The scheduler calls this while the
    /// kernel is running, so writing the task's stack pointer now would mean a trap taken between
    /// here and the return to user mode swaps in a stack it is ALREADY standing on, and builds its
    /// frame over the one it is using.
    ///
    /// The latch is armed at the only correct moment instead: the trap epilogue arms it when it is
    /// about to return to user mode, and `user_entry_trampoline` arms it for a task's first entry.
    /// Empty here is the answer, not a stub.
    ///
    /// # Safety
    /// Nothing to do, so nothing to get wrong; `unsafe` to match the seam every arch implements.
    pub unsafe fn set_tss_rsp0(core_id: usize, rsp: u64) {
        let _ = (core_id, rsp);
    }
}

// ---------------------------------------------------------------------------
/// Hook called when the scheduler commits a **user** task. x86 ignores it; ARM records the slot so the
/// timer runs its syscalls atomically. Nothing to do on this stub yet.
pub fn note_user_task(_slot: usize) {}

// --- Boot/panic console floor backend (`crate::bootcon`) ---
// The kernel's boot/panic floor owes each arch one item (see `crate::bootcon`). No framebuffer is
// mapped on this stub, so the console never initialises and every entry point no-ops.

/// The last-level cache, and the memory window used to flush it. Zero until the boot finds them.
static CCACHE_BASE: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
static CCACHE_ZERO_DEV: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
/// Bytes per way and the highest line index within one, worked out from the cache's own config
/// register rather than from constants, because a constant here would be a different cache on the
/// next part.
static CCACHE_BYTES_PER_WAY: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
static CCACHE_MAX_WAY: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// Registers, from Linux's `drivers/cache/sifive_ccache.c`.
const CCACHE_CONFIG: usize = 0x00;
const CCACHE_WAYENABLE: usize = 0x08;
const CCACHE_WAYMASK_BASE: usize = 0x800;
const CCACHE_MASTERS: usize = 32;
const CCACHE_ALL_WAYS: u64 = 0xffff;

/// Work out the cache's shape and remember where to reach it.
///
/// Called once at boot with the two addresses the device tree gives the cache controller: its
/// registers, and a 32 MiB window that reads as zeros and exists for no purpose except this.
pub(super) fn ccache_init(base: u64, zero_dev: u64) {
    if base == 0 || zero_dev == 0 {
        return;
    }
    // SAFETY: an MMIO register the device tree describes, inside the identity map, read-only here.
    let cfg = unsafe { ((base as usize + CCACHE_CONFIG) as *const u32).read_volatile() };
    let banks = (cfg & 0xff) as u64;
    let ways = ((cfg >> 8) & 0xff) as u64;
    let sets = 1u64 << ((cfg >> 16) & 0xff);
    let block = 1u64 << ((cfg >> 24) & 0xff);
    if banks == 0 || ways == 0 || block == 0 {
        return;
    }
    // SAFETY: as above.
    let max_way = unsafe { ((base as usize + CCACHE_WAYENABLE) as *const u32).read_volatile() } as u64;

    CCACHE_BASE.store(base, Ordering::Relaxed);
    CCACHE_ZERO_DEV.store(zero_dev, Ordering::Relaxed);
    CCACHE_BYTES_PER_WAY.store(banks * sets * block, Ordering::Relaxed);
    CCACHE_MAX_WAY.store(max_way, Ordering::Relaxed);

    // TIME IT ONCE, because everything after this is a judgement about how often the flush can be
    // afforded, and that judgement should not rest on a guess. The first pass is also the warm-up,
    // so the one that is timed is the second.
    ccache_flush_all();
    let t0 = sbi::time();
    ccache_flush_all();
    let ticks = sbi::time().wrapping_sub(t0);
    let rate = TIMEBASE_HZ.load(Ordering::Relaxed) as u64;
    CCACHE_FLUSH_US.store(if rate == 0 { 0 } else { ticks * 1_000_000 / rate }, Ordering::Relaxed);

    print_str("riscv64: last-level cache ");
    print_dec(banks * sets * ways * block / 1024);
    print_str(" KiB, ");
    print_dec(ways);
    print_str(" ways (");
    print_dec(max_way + 1);
    print_str(" enabled), ");
    print_dec(block);
    print_str(" byte lines, a full flush costs ");
    print_dec(CCACHE_FLUSH_US.load(Ordering::Relaxed));
    print_str(" us\n");
}

/// How long one whole-cache flush takes, in microseconds. MEASURED at boot, because the publish rate
/// below is a trade against it and a trade against a guess is not a trade.
static CCACHE_FLUSH_US: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
/// When the framebuffer was last published, on the machine's wall clock.
static FB_LAST_PUBLISH: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
/// How often to publish, in microseconds. 200 Hz.
///
/// **A DURATION, because a tick countdown made the picture depend on how many harts existed.** This
/// was "one publish every two ticks" against a counter every hart incremented - so on four harts it
/// fired about 200 times a second, and the number 2 described nothing anyone had chosen. Restricting
/// the publish to core 0 to stop four harts racing over the cache controller therefore also cut the
/// rate to 50 Hz, and the operator saw exactly that: tearing and dirt on a screen that had been
/// clean, on a port where the other ports have no such problem.
///
/// The rate is now stated rather than emergent. 200 Hz is what the screen actually had before, and
/// the cost is affordable and measured rather than assumed: `CCACHE_FLUSH_US` times five publishes
/// per 10 ms tick - about 3% of one core at the 154 us this board measures. That buys a framebuffer
/// which is never more than 5 ms stale on hardware that cannot keep it coherent for us.
///
/// Paced on the CLOCK and claimed with a compare-exchange, so it holds whichever hart calls it and
/// however many do. The try-lock inside the flush is still what makes it SAFE; this only makes it
/// REGULAR.
const FB_PUBLISH_US: u64 = 5_000;

/// Publish the framebuffer on behalf of whoever is drawing on it.
///
/// **This exists because this SoC cannot give USERSPACE a coherent framebuffer, and once the screen
/// has been granted away the kernel is the only thing left that can fix it.** Every other port
/// answers this by mapping the framebuffer NON-CACHEABLE into the `console` service - that is what
/// `task/mod.rs` says it does and what the ARM ports do - and a page-based memory type is precisely
/// what this part does not have. `Svpbmt` is absent from its ISA and there is no other way to say it,
/// so the service's writes land in a write-back cache the display controller cannot see into and the
/// screen shows whatever was in memory before.
///
/// The kernel's own drawing does not need this: `fb_commit` publishes after every rectangle. This is
/// only for the period after the grant, when the writer is a service that neither knows nor should
/// know that this machine has a cache with an opinion about its pixels.
///
/// The cost is real and is stated rather than hidden (26.4): a full flush, twenty times a second,
/// emptying a two megabyte cache that everything else on the machine was using. The rate is a
/// deliberate trade and the flush's measured cost is printed at boot so it can be checked.
fn publish_framebuffer_on_tick() {
    // ANY HART MAY PUBLISH; AT MOST ONE DOES PER INTERVAL.
    //
    // Restricting this to core 0 was the wrong lever. `ccache_flush_all` is not a local operation -
    // it reprograms the way-mask of every bus master on the SoC - so four harts entering it at once
    // is a genuine race, but the fix for a race is mutual exclusion, which the try-lock inside the
    // flush already provides. Cutting the number of CALLERS instead also cut the publish RATE by
    // four, and on a framebuffer this SoC cannot keep coherent that is visible as tearing.
    //
    // So the pacing is a clock and the safety is a lock, which is what each is actually for.
    if CCACHE_BASE.load(Ordering::Relaxed) == 0 || !display::framebuffer_is_live() {
        return;
    }
    let hz = TIMEBASE_HZ.load(Ordering::Relaxed) as u64;
    if hz == 0 {
        return; // no clock to pace against yet; the boot console still owns the screen
    }
    let interval = (hz / 1_000_000).max(1) * FB_PUBLISH_US;
    let now = sbi::time();
    let last = FB_LAST_PUBLISH.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < interval {
        return;
    }
    // Compare-exchange, so exactly one hart wins an interval however many arrive together. A loser
    // returns rather than retrying: the winner is about to publish everything, including whatever
    // the loser was here for.
    if FB_LAST_PUBLISH
        .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    ccache_flush_all();
}

/// Push every dirty line in the last-level cache out to memory.
///
/// **This part cannot flush by address, and that is a property of the silicon rather than a choice.**
/// The cache controller defines a flush-by-physical-address register and Linux's driver for it
/// defines the offset - and never uses it, because on this SoC it does not work. What the vendor
/// driver does instead is the only mechanism there is: for each way in turn, restrict every bus
/// master to that one way, then write a way's worth of zeros through a memory window that exists for
/// this purpose, which forces every line in that way to be allocated afresh and its old contents
/// written back. Sixteen ways of two thousand lines is about thirty-three thousand stores.
///
/// So a whole-cache flush is the only granularity available, and `fb_commit`'s rectangle is
/// information this hardware cannot use. That is recorded rather than hidden: the cost is the same
/// for one character as for the whole screen.
///
/// The RISC-V ISA has nothing to offer here either - this part implements neither `Zicbom` (cache
/// block operations) nor `Svpbmt` (a non-cacheable page attribute), so there is no portable way to
/// do this and no way to avoid needing to.
/// Claimed by the first hart to reach the halt dump, so the other harts stop quietly instead of
/// interleaving three copies of it into one line.
static DUMPED: AtomicBool = AtomicBool::new(false);

/// Held while a hart is inside the flush.
///
/// **The flush is not a local operation and never was.** For each cache way it writes that way's
/// mask into the way-mask register of EVERY bus master on the SoC - confining the whole chip to one
/// sixteenth of its L2 - writes a way's worth of zeros through the zero device to force the
/// eviction, and finally restores every master to all ways. Two harts interleaving that do not
/// merely duplicate work: one restores ALL_WAYS while the other still believes the chip is
/// confined, so the second hart's remaining ways are evicted with no confinement and the flush it
/// believes it performed did not happen.
///
/// A correctness argument, not a performance one, and it stands whatever turns out to cause the
/// chaos wedge: reprogramming global hardware state from four harts with no mutual exclusion is not
/// something to leave in place while looking for a reason to remove it.
static CCACHE_BUSY: AtomicBool = AtomicBool::new(false);
/// Flushes skipped because another hart held it. Reported rather than silent (26.7).
static CCACHE_SKIPPED: AtomicU32 = AtomicU32::new(0);

fn ccache_flush_all() {
    // ONE AT A TIME, and a caller that cannot get in RETURNS rather than waits.
    //
    // Waiting is the obvious choice and the wrong one here. Every caller is inside a trap handler
    // with interrupts masked, so a hart spinning for the holder cannot be preempted, cannot service
    // an IPI, and cannot be seen to be alive - which is the exact shape of the wedge this port is
    // chasing. A skipped flush costs a stale framebuffer for one tick; a flush that waits can cost
    // the machine.
    //
    // Skipping is safe for the publisher because the flush is WHOLE-CACHE: the hart already holding
    // it is flushing everything, including whatever this caller wrote - those writes landed first,
    // since a publish happens after the write and not before.
    if CCACHE_BUSY.swap(true, Ordering::Acquire) {
        CCACHE_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    ccache_flush_all_locked();
    CCACHE_BUSY.store(false, Ordering::Release);
}

/// How many flushes were skipped because another hart was already flushing.
pub fn ccache_skipped() -> u32 {
    CCACHE_SKIPPED.load(Ordering::Relaxed)
}

fn ccache_flush_all_locked() {
    let base = CCACHE_BASE.load(Ordering::Relaxed) as usize;
    let zero = CCACHE_ZERO_DEV.load(Ordering::Relaxed) as usize;
    let per_way = CCACHE_BYTES_PER_WAY.load(Ordering::Relaxed) as usize;
    let max_way = CCACHE_MAX_WAY.load(Ordering::Relaxed) as usize;
    if base == 0 || zero == 0 || per_way == 0 {
        return;
    }
    const LINE: usize = 64;

    // SAFETY: two MMIO windows the device tree describes, both inside the identity map, written
    // exactly as the vendor driver writes them. The way-mask registers are restored to "all ways" on
    // the way out, including if the loop is entered zero times, so no path leaves the cache confined
    // to one way - which would be a machine that still works and is sixteen times smaller.
    unsafe {
        core::arch::asm!("fence rw, rw", options(nostack, preserves_flags));
        for way in 0..=max_way {
            let mask = 1u64 << way;
            for m in 0..CCACHE_MASTERS {
                ((base + CCACHE_WAYMASK_BASE + m * 8) as *mut u64).write_volatile(mask);
            }
            let line_base = zero + way * per_way;
            let mut off = 0usize;
            while off < per_way {
                ((line_base + off) as *mut u64).write_volatile(0);
                off += LINE;
            }
            core::arch::asm!("fence rw, rw", options(nostack, preserves_flags));
        }
        for m in 0..CCACHE_MASTERS {
            ((base + CCACHE_WAYMASK_BASE + m * 8) as *mut u64).write_volatile(CCACHE_ALL_WAYS);
        }
        core::arch::asm!("fence rw, rw", options(nostack, preserves_flags));
    }
}

/// Publish a written rectangle so the display controller's next scan reads it.
///
/// **It did turn out to be wrong, and the television said so.** This used to be a bare `fence`, on
/// the reasoning that the framebuffer is ordinary cacheable RAM, that ordering is all a coherent
/// machine needs, and that this board's device tree marks nothing `dma-noncoherent`. The picture came
/// up as colour bars with a corrupted band across them - which is precisely the shape of the mistake:
/// eight megabytes written through a two megabyte cache pushes most of itself out on the way, and
/// what is left dirty is the last part written. The device tree's silence was not a claim of
/// coherence; it was silence, and I read it as evidence.
///
/// So this owes ordering AND a writeback. The RISC-V ISA offers nothing for the second - this part
/// implements neither `Zicbom` nor `Svpbmt`, so there is no cache-block operation and no way to mark
/// the pages non-cacheable - and the cache's own flush-by-address register does not work on this SoC.
/// What is left is `ccache_flush_all`, which is a WHOLE-CACHE flush; the rectangle this function is
/// handed is information the hardware cannot use, and the cost is the same for one character as for
/// the entire screen.
pub fn fb_commit(
    _base: usize, _pitch: usize, _bpp: usize,
    _x: usize, _y: usize, _w: usize, _h: usize,
) {
    // SAFETY: a memory fence has no operands and no side effect beyond ordering. `rw, rw` is the
    // full barrier - every earlier load and store before every later one.
    unsafe { core::arch::asm!("fence rw, rw", options(nostack, preserves_flags)) };
    ccache_flush_all();
}

pub mod page_tables {

    /// Arch hook run once a service's address space is built. x86 needs nothing; ARM and RISC-V
    /// clone the kernel identity mapping into it.
    ///
    /// WITHOUT THIS A TASK CANNOT BE SWITCHED TO. The instant `switch_context` writes `satp`, the
    /// kernel is executing through the new table - the very next instruction fetch, the stack under
    /// it, and the trap vector a fault would need. A root that maps only the task's own pages is a
    /// machine that stops, with nothing left able to say why.
    ///
    /// # Safety
    /// `root` must be a page-table root this task owns.
    pub unsafe fn finalize_service_address_space(root: u64) {
        // NOTHING IS PUBLISHED HERE ANY MORE, and the absence is deliberate.
        //
        // This used to broadcast `sbi_remote_fence_i` so every hart would discard stale instruction
        // bytes for the frames this spawn had just written. That is the right GUARANTEE in the wrong
        // PLACE: a broadcast waits for every other hart to acknowledge, from inside a path that runs
        // with interrupts off, which is the deadlock `task/scheduler.rs` already documents for TLB
        // shootdowns. It survived three supervisor respawns and hung the machine on the fourth,
        // three rounds into a chaos run.
        //
        // The guarantee now lives in `context_switch::user_entry_trampoline`: a local `fence.i` on
        // the hart that is about to run the task, one instruction before it does. Same property, no
        // cross-hart wait, and cheaper.

        // The KERNEL's root, not the live one. This runs inside a spawn, and a spawn is a syscall
        // made by a task, so the live root belongs to whoever asked - see `KERNEL_ROOT`.
        let kernel_root = super::KERNEL_ROOT.load(core::sync::atomic::Ordering::Relaxed);
        if kernel_root == 0 || root == 0 {
            return; // paging is not on yet, or no root: nothing to inherit and nowhere to put it
        }
        super::sv39::clone_kernel_map(root, kernel_root);
    }

    /// Free a dead task's page-table ROOT - just the root.
    ///
    /// The structure below it was already freed by `reclaim_user_frames`; this is the last frame, and
    /// it is separate because a task that kills ITSELF is still executing in that address space when
    /// the rest goes. The neutral kill path defers this one until the core has switched away, which
    /// is why it is its own call and not the tail of the walk.
    ///
    /// # Safety
    /// `root` must belong to a task already marked Dead, after a TLB shootdown, and must not be the
    /// address space currently in `satp`.
    pub unsafe fn free_page_table_root(root: u64) {
        if root == 0 || !crate::memory::allocator::phys_in_ram(root) {
            return;
        }
        // SAFETY: contract delegated to the caller above; the frame is a root table this kernel
        // allocated, and nothing walks it any more.
        unsafe { crate::memory::allocator::free_frame(super::sv39::frame_of(root)) };
    }

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
        pub fn new() -> Result<Self, MapError> {
            let root = super::sv39::new_root().ok_or(MapError::FrameAllocFailed)?;
            Ok(PageTable { root })
        }
        pub fn map(&mut self, virt: VirtAddr, phys: PhysAddr, flags: PageFlags) -> Result<(), MapError> {
            let bits = super::sv39::flags_to_pte_bits(flags.bits());
            super::sv39::map_page(self.root, virt.0, phys.0, bits).map_err(|e| match e {
                super::sv39::MapFail::NoFrame => MapError::FrameAllocFailed,
                super::sv39::MapFail::AlreadyMapped => MapError::AlreadyMapped,
                super::sv39::MapFail::NotMapped => MapError::NotMapped,
                // The seam has no word for "that address cannot exist"; the nearest true thing is
                // that it is not mapped, and never could be.
                super::sv39::MapFail::NotCanonical => MapError::NotMapped,
            })
        }
        pub fn unmap(&mut self, virt: VirtAddr) -> Result<Frame, MapError> {
            let phys = super::sv39::unmap_page(self.root, virt.0).map_err(|_| MapError::NotMapped)?;
            // SAFETY: the frame came from a leaf PTE this table owned, so it is page-aligned and is
            // now unreferenced by it - which is exactly the contract `from_phys` asks for.
            Ok(unsafe { super::sv39::frame_of(phys) })
        }
        pub fn cr3_value(&self) -> u64 { self.root }
        pub fn into_cr3(self) -> u64 { self.root }
    }

    /// Physical addresses are directly usable as virtual ones.
    ///
    /// TRUE, and it is a statement about where this port currently IS, not a permanent property.
    /// OpenSBI enters S-mode with `satp` zero, so translation is off and VA equals PA. The neutral
    /// allocator asks because a zero HHDM offset means "nobody set it" on an arch with a
    /// higher-half map and "correct" on one without, and it panics rather than guess between them -
    /// which is the right behaviour and is why this constant has to be honest.
    ///
    /// This becomes `false`, with a real `hhdm_offset`, the moment Sv39 maps the kernel high.
    pub const PHYS_IS_IDENTITY: bool = true;

    /// No bootloader placed page tables for this port - the kernel builds its own, in `.bss` inside the
    /// kernel image, which the memory map already excludes from usable RAM. So there is nothing for
    /// `protect_kernel_page_table_frames` to protect, and its x86-format walk must not run here.
    pub const BOOTLOADER_PLACED_TABLES: bool = false;
    pub fn get_hhdm_offset() -> u64 { 0 }
    pub unsafe fn set_hhdm_offset(offset: u64) {}
    pub fn read_page_table_base() -> u64 {
        let satp: u64;
        // SAFETY: reading a CSR has no side effects.
        unsafe { core::arch::asm!("csrr {}, satp", out(reg) satp, options(nomem, nostack)) };
        // The neutral kernel wants a physical ROOT ADDRESS, not the register's encoding, so the PPN
        // is shifted back. Handing it the raw `satp` would put the MODE nibble in the high bits of
        // what callers treat as an address.
        (satp & 0x0fff_ffff_ffff) << 12
    }
    /// # Safety
    /// `base` must be the physical address of a valid Sv39 root table that maps at least the code
    /// executing this write, or the very next instruction fetch faults.
    pub unsafe fn write_page_table_base(base: u64) {
        // SAFETY: contract delegated to the caller above. The fence AFTER the write is not optional:
        // `satp` takes effect immediately, but stale TLB entries from the previous space would
        // otherwise still satisfy translations that no longer exist.
        unsafe {
            core::arch::asm!(
                "csrw satp, {}",
                "sfence.vma",
                in(reg) super::sv39::satp_value(base),
                options(nostack)
            );
        }
    }
    /// # Safety
    /// Architecturally always safe; `unsafe` to match the seam every arch implements.
    pub unsafe fn invalidate_tlb_page(addr: u64) {
        // SAFETY: `sfence.vma` with an address operand invalidates translations for that page only.
        unsafe { core::arch::asm!("sfence.vma {}, zero", in(reg) addr, options(nostack)) };
    }
    /// Map one page into the table `satp` is currently using, and make the change visible.
    ///
    /// The active root rather than a caller-supplied one, because the caller that needs this is
    /// adding a page to the space it is already running in. Refuses if translation is off: there is
    /// no active table to add to, and inventing one silently would be the worst kind of success.
    ///
    /// # Safety
    /// `phys` must be a frame the caller owns, and `virt` an address the caller is entitled to
    /// claim. Mapping over something in use is refused by the walk rather than silently allowed,
    /// but mapping a frame that is already someone else's cannot be seen from here.
    pub unsafe fn map_in_active_tables(virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
        let root = read_page_table_base();
        if root == 0 {
            return Err(MapError::NotMapped);
        }
        let bits = super::sv39::flags_to_pte_bits(flags);
        super::sv39::map_page(root, virt, phys, bits).map_err(|e| match e {
            super::sv39::MapFail::NoFrame => MapError::FrameAllocFailed,
            super::sv39::MapFail::AlreadyMapped => MapError::AlreadyMapped,
            super::sv39::MapFail::NotMapped => MapError::NotMapped,
            // The seam has no word for "that address cannot exist"; the nearest true thing is that
            // it is not mapped, and never could be.
            super::sv39::MapFail::NotCanonical => MapError::NotMapped,
        })?;
        // A fresh mapping still needs the fence: a walk that previously found nothing here may have
        // cached that absence, and on RISC-V a negative TLB entry is permitted.
        // SAFETY: an address-scoped `sfence.vma` for the page just mapped.
        unsafe { invalidate_tlb_page(virt) };
        Ok(())
    }
    pub fn entry_for_va(virt: u64) -> Option<u64> {
        let root = read_page_table_base();
        if root == 0 {
            return None; // translation is off: there is no entry to report, which is not an error
        }
        super::sv39::translate(root, virt)
    }
    pub fn unmap_4k_strided(base: u64, stride: u64, count: usize) {}
    pub fn harden_hhdm_nx() {}
    /// Give a dead task's pages and page tables back to the allocator, and report how many frames
    /// that was.
    ///
    /// Returning ZERO from a stub is not neutral: the kill path prints the count, so `freed 0 frames`
    /// read as "this task had nothing" when it meant "nothing was reclaimed". A service that faults
    /// and restarts in a loop then leaks its whole address space per cycle, and the machine dies of
    /// `FrameAllocFailed` several hundred restarts later - a long way from the cause. Observed
    /// exactly that way: 709 fault-restart cycles, then out of memory.
    ///
    /// # Safety
    /// `cr3` must be a Dead task's root that no core will load again.
    pub unsafe fn reclaim_user_frames(cr3: u64) -> usize {
        if cr3 == 0 {
            return 0;
        }
        // SAFETY: contract delegated to the caller above.
        unsafe { super::sv39::reclaim_user(cr3) }
    }
}

// ---------------------------------------------------------------------------
pub mod uaccess;

/// The syscall-entry surface, which is the same set of primitives seen from the other side of the
/// seam. Both paths are real names the neutral kernel uses - `arch::imp::read_user_bytes` from the
/// dispatcher, `arch::imp::syscall_entry::syscall_slot` from the scheduler - so both answer, from
/// one implementation.
pub mod syscall_entry {
    pub use super::uaccess::{
        copy_user_to_kernel, init_percore_arenas, init_percore_syscall_arena, read_cycle_counter,
        read_user_bytes, syscall_slot, validate_user_ptr, write_user_bytes, PerCoreSyscallData,
        USER_END,
    };
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
    /// `sstatus.SIE` - the one bit that admits interrupts at all while the kernel is running.
    ///
    /// It governs S-mode only. Running in U-MODE, supervisor interrupts are enabled regardless of
    /// this bit, which is what makes a user task preemptible without the kernel arranging anything -
    /// and is why masking here cannot be used to protect a critical section from a user task.
    const SSTATUS_SIE: u64 = 1 << 1;

    pub fn enable_interrupts() {
        // SAFETY: setting `sstatus.SIE`. Sound because `stvec` is installed before the timer is
        // started, so there is always somewhere for an admitted interrupt to go.
        unsafe { core::arch::asm!("csrs sstatus, {}", in(reg) SSTATUS_SIE, options(nostack)) };
    }

    pub fn disable_interrupts() {
        // SAFETY: clearing `sstatus.SIE`. An interrupt raised while it is clear is held pending by
        // the hardware rather than lost, so this defers rather than discards.
        unsafe { core::arch::asm!("csrc sstatus, {}", in(reg) SSTATUS_SIE, options(nostack)) };
    }

    /// Mask interrupts and report whether they had been enabled, in ONE instruction.
    ///
    /// `csrrc` reads the old value and clears the given bits atomically. Reading and then clearing
    /// as two instructions would leave a window in which an interrupt is taken after the caller has
    /// already decided it was masking, and the handler would return into a critical section the
    /// caller believes it is protecting.
    pub fn local_irq_save() -> bool {
        let old: u64;
        // SAFETY: an atomic read-and-clear of `sstatus.SIE`, with no other effect.
        unsafe {
            core::arch::asm!(
                "csrrc {0}, sstatus, {1}",
                out(reg) old, in(reg) SSTATUS_SIE,
                options(nostack)
            )
        };
        old & SSTATUS_SIE != 0
    }

    /// Undo `local_irq_save`. Restores rather than enables: a nested save must not turn interrupts
    /// on inside an outer section that had them off.
    pub fn local_irq_restore(was_enabled: bool) {
        if was_enabled {
            enable_interrupts();
        }
    }

    /// Sleep until an interrupt arrives, and RE-ENABLE INTERRUPTS on the way out.
    ///
    /// **The name says halt; the contract says `sti`.** The neutral idle path masks interrupts,
    /// re-checks its run queue under the mask, and then calls this - and its own comment states the
    /// obligation plainly: "no ready tasks; re-enable interrupts and loop. `wait_for_interrupt`
    /// issues only `sti`". Implementing the NAME and not the contract is a core that masks once and
    /// never unmasks: the timer stops, the scheduler idles forever, and the machine is dead with no
    /// fault to report. That is exactly what happened the first time the supervisor was scheduled -
    /// it spawned two services and then took one more tick in forty seconds.
    ///
    /// Order matters and this one is race-free. `wfi` wakes when an ENABLED interrupt becomes
    /// pending REGARDLESS of `sstatus.SIE`, so sleeping first and unmasking second cannot lose a
    /// wake: an interrupt that arrived before the `wfi` leaves it already pending and `wfi` returns
    /// at once, and one that arrives during it wakes the hart. Unmasking after is what delivers it.
    /// Unmasking FIRST would reopen the window the neutral path masked to close.
    ///
    /// `wfi` is also architecturally a HINT that may return at any time, which is sound here for the
    /// same reason it is sound anywhere: the caller is a loop that re-checks its condition.
    pub fn wait_for_interrupt() {
        // SAMPLE BEFORE SLEEPING. On a hart that never wakes there is no "after", so anything not
        // captured here is unavailable forever - which is exactly why the first three attempts at
        // this wedge had nothing to read.
        let (sie, sip, now): (u64, u64, u64);
        // SAFETY: three CSR reads, no side effects.
        unsafe {
            core::arch::asm!("csrr {}, sie", out(reg) sie, options(nomem, nostack));
            core::arch::asm!("csrr {}, sip", out(reg) sip, options(nomem, nostack));
        }
        now = super::sbi::time();
        super::note_idle_sample(sie, sip, now);
        super::note_stage(super::stage::IDLE_HALT);

        // SAFETY: `wfi` has no memory effects and is permitted in S-mode while `mstatus.TW` is
        // clear, which it is under OpenSBI; if firmware did trap it, the trap vector names it. The
        // `csrs` then sets `sstatus.SIE`, which is this function's actual job.
        unsafe {
            core::arch::asm!(
                "wfi",
                "csrs sstatus, {sie}",
                sie = in(reg) SSTATUS_SIE,
                options(nostack)
            )
        };
    }
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
    /// **YES** - and for the same reason x86 says yes, reached differently.
    ///
    /// The idle loop masks interrupts, re-checks for work, and halts, relying on the halt not to
    /// sleep through an interrupt raised in that window. On RISC-V `wfi` resumes when an enabled
    /// interrupt becomes PENDING regardless of `sstatus.SIE` - the specification says so explicitly -
    /// so an interrupt raised after the re-check is latched and `wfi` returns immediately; the
    /// handler then runs once `SIE` is restored.
    ///
    /// ARM answers no because its idle path does real work that needs interrupts enabled. This one
    /// does nothing but wait, so there is no such obligation.
    pub fn idle_mask_before_halt() -> bool { true }

    /// Whether the idle loop may halt at all. Yes: `wfi` is the whole mechanism, and it is safe here
    /// because it is only ever executed inside a loop that re-checks its condition - `wfi` is
    /// architecturally a HINT and an implementation may return from it at any time.
    pub fn idle_can_halt() -> bool { true }
    pub fn send_eoi() {}                                     // GIC EOIR
    pub fn fire_test_irq(irq: u8) {}
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
pub mod rtc {
    use core::sync::atomic::Ordering;

    pub use crate::clock::epoch_secs;
    /// Baseline the monotonic clock. Idempotent, and already done by `start_timer` - which is the
    /// point where the timebase becomes known and so the earliest moment a baseline means anything.
    pub fn capture_boot_time() {
        if super::BOOT_TIME.load(Ordering::Relaxed) == 0 {
            super::BOOT_TIME.store(super::sbi::time(), Ordering::Relaxed);
        }
    }
    pub fn boot_datetime() -> u64 { 0 }
    pub fn read_datetime() -> u64 { 0 }
    pub fn set_wall_clock(_epoch: i64) -> bool { false } // no RTC on this stub; SNTP wall clock unused (arm is the live RTC-less port)
    /// Seconds since boot, from the machine's own counter.
    ///
    /// **Returning 0 from the stub this replaces was not harmless.** The shell's `wait` paces on this
    /// value - it loops until the elapsed count has advanced by N - so a clock frozen at zero meant
    /// `wait 1` could never satisfy itself and failed instead. `selfcheck` caught it on hardware as
    /// `assert: FAILED (ok 'wait 1')`, which is the whole reason a suite exists.
    ///
    /// Derived, not counted: `time` advances at the device tree's `timebase-frequency` whatever the
    /// core is doing, so this is correct across idle, preemption and any future frequency scaling -
    /// unlike a tick count, which measures how often the scheduler ran.
    pub fn now_epoch_monotonic() -> i64 {
        let hz = super::TIMEBASE_HZ.load(Ordering::Relaxed) as u64;
        if hz == 0 {
            return 0; // no timebase yet: say nothing rather than a number that means nothing
        }
        let base = super::BOOT_TIME.load(Ordering::Relaxed);
        (super::sbi::time().saturating_sub(base) / hz) as i64
    }
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

/// Interrupts this core has taken, and the last cause it saw.
///
/// **This was a stub returning `(0, 0)`, and the stub told a lie at the worst possible moment.** The
/// liveness watchdog prints these two numbers inside its panic, so the first wedge this port ever
/// caught reported "it has taken 0 timer interrupts, last vector 0x00000000" about a core whose
/// timer nobody had ever counted. Zero is the single most incriminating answer that message can
/// carry - it reads as "the timer stopped" - and it was not a reading at all.
///
/// x86 carried the same stub and fixing it is what finally let a repro there distinguish a timer
/// that STOPPED from a tick that was merely SKIPPED. Those have nothing in common: one is an
/// interrupt controller or a deadline that was never re-armed, the other is a core that is taking
/// ticks and still not reaching the scheduler. Guessing between them is how a wedge stays open for
/// sessions.
///
/// Counted at the trap itself rather than in the tick handler, so a timer that fires and is then
/// dropped somewhere later still increments - the question being asked is "did the interrupt ARRIVE",
/// and a counter further down the path cannot answer it. Relaxed ordering throughout: this is
/// evidence for a human, read after a core has already stopped, and a barrier per interrupt to make
/// a debug counter exact would be paying on the hot path for a precision nobody reads.
static CORE_IRQ_COUNT: [AtomicU32; MAX_HART_ID] = [const { AtomicU32::new(0) }; MAX_HART_ID];
static CORE_IRQ_LAST: [AtomicU32; MAX_HART_ID] = [const { AtomicU32::new(0) }; MAX_HART_ID];

/// Where in the kernel each hart last was, so a wedge can say WHICH PHASE it stopped in.
///
/// **The watchdog names a core and a task; it cannot name a line of code.** The first two wedges
/// this port caught reported core 1 running `block-driver` and core 0 running `supervisor`, which
/// between them rule out a single guilty service and leave the kernel path they share. The interrupt
/// counter then showed core 0 taking 3838 interrupts and FREEZING at 3838 - and since a RISC-V trap
/// clears `sstatus.SIE` and only `sret` restores it, a core that stops taking interrupts is a core
/// that entered a trap and never came out. That narrows it to the trap handler, and this narrows it
/// further, to a phase within one.
///
/// A single relaxed store per stamp, on a path that already does several. It is deliberately NOT a
/// ring buffer or a timestamped trail: the question is "where did it stop", one value answers it,
/// and an instrument heavy enough to change the timing of the thing it is watching is how a
/// heisenbug gets manufactured.
static CORE_STAGE: [AtomicU32; MAX_HART_ID] = [const { AtomicU32::new(0) }; MAX_HART_ID];
/// WHERE the last stage stamp was made from - the caller's return address.
///
/// **Because a stage names a FUNCTION, and a function has callers.** Three times in this wedge hunt a
/// stamp was read as a location and turned out to be ambiguous: stage 14 meant both "wedged in the
/// drain" and "halted because a neighbour panicked"; stage 15 meant both "before the progress stamp"
/// and "past it, hung in the switch"; stage 17 was placed on `syscall_slot` because the timer tick
/// reads it one line before `switch_context`, and `syscall_slot` turns out to have EIGHT call sites -
/// the timer switch, yield, block-and-reschedule and more. Each time the ambiguity was found only
/// after a conclusion had been drawn from it, and each cost a boot.
///
/// A return address cannot be ambiguous. `note_stage` is `#[inline(never)]` so its `ra` is the site
/// that called it, and the dump prints it beside the stage - one `llvm-objdump` away from the exact
/// line, however many callers the function grows later.
static CORE_STAGE_RA: [AtomicU64; MAX_HART_ID] = [const { AtomicU64::new(0) }; MAX_HART_ID];

/// Stages, in the order a tick passes through them. A wedge reports the LAST one reached, so the
/// culprit is between that stage and the next.
pub(super) mod stage {
    pub const TRAP_ENTRY: u32 = 1;
    pub const TIMER_REARMED: u32 = 2;
    pub const USERMODE_HOOK: u32 = 3;
    pub const FB_PUBLISH: u32 = 4;
    pub const NEUTRAL_SCHED: u32 = 5;
    pub const TICK_DONE: u32 = 6;
    pub const TRAP_EXIT: u32 = 7;
    pub const SYSCALL: u32 = 8;
    pub const IPI_DRAIN: u32 = 9;
    /// Inside `scheduler::timer_tick_from_irq`, PAST its entry and BEFORE `drain_pending_kstack`.
    ///
    /// **The window a wedged hart has now been found in three times, and the one stamp that can split
    /// it.** `NEUTRAL_SCHED` (5) is written by this port immediately before calling the neutral tick,
    /// and `TICK_DONE` (6) immediately after, so a hart resting at 5 is somewhere inside a function
    /// whose progress stamp - the one the liveness watchdog reads - sits 69 lines in. Every arch call
    /// in that window is an empty stub on this port (`apic_send_eoi`, `rearm_tsc_deadline`), leaving
    /// `drain_pending_kstack` as the only thing there that can block: it takes `KSTACK_USED` and the
    /// frame allocator's lock, from inside a trap with interrupts masked.
    ///
    /// `panic_halt_check` is called between the two, so stamping there answers the question with a
    /// reading rather than an argument. A wedge showing 5 is stuck BEFORE it - in `note_irq` or
    /// `current_core_id`, which are a store and a register read. A wedge showing this is stuck at or
    /// after the drain.
    pub const NEUTRAL_TICK: u32 = 14;
    /// Inside `timer_tick_from_irq`, PAST `drain_pending_kstack` and before the progress stamp.
    ///
    /// Stamped from `apic_send_eoi`, which is an empty stub on this port and is called on the very
    /// next line after the drain - so it is the boundary, for free, with no neutral code touched.
    /// Stage 14 against 15 now separates the two lock-takers left in that window: the drain, which
    /// takes `KSTACK_USED` and the frame allocator, from `scan_timed_wakes` and the core-0 work after
    /// it, which take the scheduler's.
    pub const TICK_PAST_DRAIN: u32 = 15;
    /// This hart stopped because ANOTHER hart is panicking - it is not the casualty.
    ///
    /// Separated because stage 14 was doing both jobs and could not tell them apart: the wedged hart
    /// rests there, and every healthy hart halted by `panic_halt_check` rested there too, so a dump
    /// showed four harts at 14 and the reader had to know which one the panic named. An instrument
    /// whose two meanings need a second instrument to distinguish is one instrument short.
    pub const PANIC_HALTED: u32 = 16;
    /// Past `pick_next`, one line before `switch_context`.
    ///
    /// **Because stage 15 was being read as a LOCATION when it is only the last stamp.** Nothing
    /// stamps between the progress stamp and the end of the tick, so a hart that recorded its
    /// progress and then hung in `pick_next` or the context switch shows 15 exactly like one stuck
    /// before it - with a `CORE_LAST_TICK_TSC` frozen at the moment it went in, which is precisely
    /// what the watchdog then reports ten seconds later. That reading fits the capture better than
    /// the window before the stamp does, because for a core that is NOT core 0 that window contains
    /// two atomic accesses and a CSR read and nothing that can block at all.
    ///
    /// The neutral tick calls `syscall_slot` - an arch function - on the line before the switch, so
    /// stamping there splits it with no neutral code touched. A wedge at 15 is in `pick_next`; a
    /// wedge here is in `switch_context`, which on this port writes `satp` and `sfence.vma`
    /// unconditionally, and would fault forever with no output if handed a reclaimed root.
    pub const PRE_SWITCH: u32 = 17;
    /// Inside `timer_tick`, BEFORE the SBI call that re-arms the deadline.
    ///
    /// The gap between `TRAP_ENTRY` and `TIMER_REARMED` is where a wedged core has now been found
    /// twice, and it contains two things with nothing in common: two `ecall`s into M-mode firmware,
    /// and - for a trap that is not an interrupt - the whole fault-reporting path. A core stuck at 1
    /// could be in either, and they have no shared fix. These two stamps split them so the next
    /// wedge names one instead of leaving a choice.
    pub const TIMER_ENTER: u32 = 11;
    /// Inside the KILL path, having already reported the fault.
    ///
    /// Split from `FAULT_REPORT` because stage 12 covered both and they are not remotely the same
    /// thing. Reporting is printing and a page-table read - bounded, and measured at well under a
    /// second even with a wedged UART, since `putc` gives up after 200k spins. Killing takes locks,
    /// walks a page table freeing every frame, and reschedules. A ten-second stall belongs to the
    /// second of those, and the dump could not say which.
    pub const KILL: u32 = 13;
    /// About to report a fault: the printing, the page-table walk, the task-name lookup.
    pub const FAULT_REPORT: u32 = 12;
    /// Sitting in `wfi`, in the idle path.
    ///
    /// Added after the first stage dump proved the other stages could not discriminate: 5
    /// (`NEUTRAL_SCHED`) is stamped BEFORE `timer_tick_from_irq`, which switches context away and
    /// does not come back until much later, so every hart rests at 5 whether it is healthy or dead.
    /// A stage is only evidence if the healthy value differs from the wedged one.
    pub const IDLE_HALT: u32 = 10;
}

/// What each hart saw at the instant it decided to halt.
///
/// **The four numbers that decide why a `wfi` did not wake, captured where the decision is made.**
/// A halted core that never returns has exactly three explanations and they are told apart here:
/// the deadline was never armed (`deadline` stale or zero), the timer was not enabled to wake it
/// (`sie` missing STIE, bit 5), or it was armed and enabled and the hardware did not deliver
/// (`deadline` in the past relative to a `time` that has since moved on, with STIP set in `sip`).
///
/// Sampled BEFORE the `wfi` rather than after, because after is a moment that never arrives on the
/// hart in question - which is the whole problem.
static IDLE_DEADLINE: [AtomicU64; MAX_HART_ID] = [const { AtomicU64::new(0) }; MAX_HART_ID];
static IDLE_TIME: [AtomicU64; MAX_HART_ID] = [const { AtomicU64::new(0) }; MAX_HART_ID];
static IDLE_SIE: [AtomicU64; MAX_HART_ID] = [const { AtomicU64::new(0) }; MAX_HART_ID];
static IDLE_SIP: [AtomicU64; MAX_HART_ID] = [const { AtomicU64::new(0) }; MAX_HART_ID];

/// Record what this hart saw immediately before halting.
pub(super) fn note_idle_sample(sie: u64, sip: u64, now: u64) {
    // SAFETY: reads `tp`; see `note_stage`.
    let hart = unsafe { boot::get_lapic_id() } as usize;
    if hart < MAX_HART_ID {
        IDLE_SIE[hart].store(sie, Ordering::Relaxed);
        IDLE_SIP[hart].store(sip, Ordering::Relaxed);
        IDLE_TIME[hart].store(now, Ordering::Relaxed);
    }
}

/// Record the deadline this hart just armed, so a halt can be checked against it.
pub(super) fn note_deadline(when: u64) {
    // SAFETY: reads `tp`; see `note_stage` for why that is this hart's id.
    let hart = unsafe { boot::get_lapic_id() } as usize;
    if hart < MAX_HART_ID {
        IDLE_DEADLINE[hart].store(when, Ordering::Relaxed);
    }
}

/// Stamp this hart's current phase.
///
/// **Reads `tp` from inside a trap, including a trap FROM USER MODE, and that is sound only because
/// of an invariant worth writing down before someone breaks it.** The trap entry does not restore a
/// kernel `tp`, so this reads whatever the interrupted context had. It is still the hart id, because
/// NOTHING ever writes `tp` after each hart sets it at entry: rustc reserves the register in both
/// the kernel and the service builds, `switch_context` saves only `ra`, `sp` and `s0`-`s11`, and
/// neither trampoline touches it.
///
/// That invariant is load-bearing far beyond this counter - `boot::get_lapic_id` is how the neutral
/// scheduler learns which core it is on, from exactly the same register, on exactly this path. If a
/// future service ever gets thread-local storage, or an `asm!` block clobbers `tp`, the symptom will
/// not be a wrong diagnostic; it will be the scheduler operating on another core's run queue. Checked
/// deliberately while auditing these counters, because a counter indexed by a user-controlled value
/// would have made every number this port has reported about harts worthless.
#[inline]
#[inline(never)]
pub(super) fn note_stage(st: u32) {
    // SAFETY: reads `tp`, which each hart sets to its own id at entry. No side effects.
    let hart = unsafe { boot::get_lapic_id() } as usize;
    // SAFETY: reads `ra`, this function's own return address. No operands, no memory effect. It is
    // meaningful only because of `#[inline(never)]` above - inlined, `ra` would belong to whoever the
    // compiler folded this into. See `CORE_STAGE_RA` for why a stage number alone was not enough.
    let ra: u64;
    unsafe { core::arch::asm!("mv {}, ra", out(reg) ra, options(nomem, nostack)) };
    if hart < MAX_HART_ID {
        CORE_STAGE[hart].store(st, Ordering::Relaxed);
        CORE_STAGE_RA[hart].store(ra, Ordering::Relaxed);
    }
}

/// The syscall number each hart is currently executing, or `u32::MAX` when it is not in one.
///
/// **A stage alone was not enough, and the last wedge is why.** The dump showed core 0 pinned at
/// stage 1 (`trap-entry`) with neither 11 (`timer-enter`) nor 12 (`fault-report`) - which rules out
/// the SBI calls and the whole fault-reporting path, and leaves exactly one route through the trap
/// handler that carried no stamp: `syscall::dispatch`. A hart inside a syscall therefore reported
/// "trap-entry" and looked like a mystery.
///
/// Recording the NUMBER rather than just the fact costs the same store and answers a different
/// question. "Stuck in a syscall" leaves sixty-odd candidates; "stuck in syscall 41" names the code
/// that has to be read, and the locks it takes are then a matter of reading it rather than of
/// another boot.
static CORE_SYSCALL: [AtomicU32; MAX_HART_ID] = [const { AtomicU32::new(u32::MAX) }; MAX_HART_ID];

/// Record which syscall this hart is entering, and that it has left one.
pub(super) fn note_syscall(nr: u32) {
    // SAFETY: reads `tp`; see `note_stage`.
    let hart = unsafe { boot::get_lapic_id() } as usize;
    if hart < MAX_HART_ID {
        CORE_SYSCALL[hart].store(nr, Ordering::Relaxed);
    }
}

/// Record that this hart took an interrupt whose `scause` code is `vector`.
///
/// Indexed by HART, not by core: the hart id is in `tp` and costs one register read, while the core
/// id needs a table lookup, and this runs on every interrupt on every hart. The translation happens
/// in `core_irq_debug`, which runs once, inside a panic.
pub fn note_irq(vector: u32) {
    // SAFETY: reads `tp`, which each hart sets to its own id at entry. No side effects.
    let hart = unsafe { boot::get_lapic_id() } as usize;
    if hart < MAX_HART_ID {
        CORE_IRQ_COUNT[hart].fetch_add(1, Ordering::Relaxed);
        CORE_IRQ_LAST[hart].store(vector, Ordering::Relaxed);
    }
}

/// `(interrupts taken, last cause)` for a CORE, for the liveness watchdog's panic line.
///
/// Walks the hart-to-core table rather than indexing it, because the mapping is one-way: harts are
/// told their core, and on this board the boot hart is 1, so hart and core numbers do not coincide
/// and assuming they do would report another core's counters under this one's name - a wrong number
/// being far worse here than no number, since this is read while deciding why a machine stopped.
pub fn core_irq_debug(core: u32) -> (u32, u32) {
    for hart in 0..MAX_HART_ID {
        if hart_core(hart as u32) == Some(core) {
            return (
                CORE_IRQ_COUNT[hart].load(Ordering::Relaxed),
                CORE_IRQ_LAST[hart].load(Ordering::Relaxed),
            );
        }
    }
    (0, 0)
}

/// Publish the boot hart's identity before any secondary starts.
///
/// Nothing to do here YET, and for a reason worth stating rather than leaving as an empty body: on
/// RISC-V the hart id arrives in `a0` at entry rather than being read back from an interrupt
/// controller, so there is no equivalent of the x86 bug this exists to prevent (a core marked ready
/// whose identity was never written). When SMP lands, the boot hart records its id here.
/// Publish the boot hart's id so `smp::core::lapic_to_core_id` can map it to core 0.
///
/// x86 needs this because the id must be read back from an interrupt controller and, until it is
/// published, every lookup answers with a default that happens to be right on one machine and wrong
/// on the next - which is the bug this function was added to fix there. RISC-V gets the id handed to
/// it in a register instead, so the READ cannot go wrong; the PUBLISH still can, and must happen for
/// the same reason. On this board the boot hart is 1, so an unpublished id means `current_core_id()`
/// resolves a hart the kernel is not running on.
pub fn publish_bsp_lapic_id() {
    let id = BOOT_HART.load(Ordering::Relaxed);
    crate::smp::core::set_core_lapic_id(0, id);
    set_hart_core(id, 0);
    print_str("riscv64: boot hart is ");
    print_dec(id as u64);
    print_str(" (core 0)
");
}

/// PCI config read, for the `hw-enumerator` SERVICE - the seam member, not the internal walk.
///
/// **This returned `None` unconditionally, which is why PCI semantics still live in ring 0 on this
/// port.** The kernel's own `pci::cfg_read32` has worked since the ECAM window was found; what was
/// missing was the seam the userspace enumerator reaches through, so `hw-enumerator` had nothing to
/// answer with and was left out of the build entirely. That is a gap in the port, not a property of
/// the machine, and it is the one thing keeping this arch behind x86 and aarch64 on step D2.
///
/// The selector encoding is the SERVICE's knowledge and stays there - that is the whole point of D2
/// - so this only decodes it.
pub fn pci_cfg_read32(sel: u32, off: u16) -> Option<u32> {
    pci::cfg_read_gated(sel, off)
}

/// Bytes emitted by the panic-path serial writer that bypasses the lock.
pub fn serial_unlocked_emit_count() -> u64 { 0 }

/// Copy from a user address into kernel memory, refusing anything not mapped to the caller.
///
/// Returns false until S-mode user pages exist. Refusing is the safe direction: a caller that cannot
/// read user memory fails its syscall, where a caller that wrongly SUCCEEDS reads someone else's.

/// Is a driver's DMA arena mapped uncached? **No - and on this port that is the truthful answer, not
/// a deferral.**
///
/// This read `true`, on the reasoning that assuming COHERENT when you are not gives a driver silently
/// stale descriptors, and that the flag could be honoured "until Sv39 attributes are wired". Both
/// halves were wrong in a way worth stating, because the constant was making a claim the machine
/// never carried out.
///
/// **It was never honoured.** `sv39::flags_to_pte_bits` builds `V|R|W|X|U|A|D` and nothing else;
/// `PageFlags::PCD` is discarded on the way into a PTE. So every arena this port has ever handed a
/// driver was ordinary cacheable write-back while the kernel believed it had mapped it uncached - a
/// silent fallback at exactly the boundary invariant 12 exists to keep honest.
///
/// **And there is nothing to wire.** Sv39 has no memory-type field, this part implements neither
/// `Svpbmt` nor `Zicbom` (see `fb_commit`, where the television proved it), so there is no page
/// attribute that could say "uncached" and no cache-block instruction a driver could use instead.
/// Leaving `true` in place would have kept a promise open that this silicon cannot ever keep.
///
/// **The arena does not need it.** DMA to a driver's arena is COHERENT with the CPU caches here, and
/// that is hardware evidence rather than an inference from the device tree's silence - which
/// `fb_commit` records being burned by. `xhci` takes its arena through this same path and enumerates
/// devices over it: the controller reads TRBs the service wrote and writes back event TRBs the
/// service reads, thousands of round trips, and the keyboard, hot-plug on every port, and mass
/// storage all work. Non-coherent memory does not do that intermittently; it does not do it at all.
///
/// The DISPLAY is the exception on this SoC, not the rule - coherence here is per master - and the
/// kernel already owns that case explicitly in `publish_framebuffer_on_tick`, at a stated rate with a
/// measured cost. A driver arena needs no such treatment, and now says so for the true reason.
pub const DMA_ARENA_UNCACHED: bool = false;
/// Virtual base at which a driver's DMA arena is mapped.
pub const DRIVER_DMA_VA: u64 = 0x7000_0000;

pub mod pci {
    //! PCI Express, reached through ECAM.
    //!
    //! **There is no port I/O on this ISA.** x86 walks the bus through the CF8/CFC address and data
    //! ports; RISC-V has no `in`/`out` instruction and no I/O space at all, so configuration space is
    //! MEMORY, mapped as a flat window: bus, device and function are bits of an address rather than a
    //! value written to a port. That is the whole difference, and it makes this the simpler of the
    //! two - one `read_volatile` where x86 needs a write then a read, with no lock between them.
    //!
    //! The window's base is READ FROM THE DEVICE TREE, never assumed. QEMU `virt` happens to put it
    //! at 0x3000_0000; a board will put it somewhere else, and a hard-coded base is the class of
    //! mistake this port has already paid for three times (the load address, the UART shift, the boot
    //! hart). The tree names it `pci-host-ecam-generic`, which is the same generic binding Linux
    //! matches on, so nothing here knows which machine it is on.

    use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
    use portable_atomic::AtomicU64;

    /// Base of the ECAM window, and how large it is. Zero means "no PCI on this machine", which is a
    /// true and common answer - the VisionFive's device tree may not describe one at all.
    static ECAM_BASE: AtomicU64 = AtomicU64::new(0);
    static ECAM_SIZE: AtomicU64 = AtomicU64::new(0);

    /// Tell this module where configuration space lives. Called once, from the boot, with what the
    /// device tree said.
    pub(super) fn set_ecam(base: u64, size: u64) {
        ECAM_BASE.store(base, Ordering::Relaxed);
        ECAM_SIZE.store(size, Ordering::Relaxed);
    }

    /// A bus/device/function triple packed the way the rest of the kernel passes it: `bus << 8 | dev
    /// << 3 | func`, which is x86's `bdf`. Kept identical so the neutral spawn path, which carries a
    /// `bdf` around without interpreting it, needs no change.
    #[inline]
    fn ecam_addr(bdf: u32, off: u16) -> Option<usize> {
        let base = ECAM_BASE.load(Ordering::Relaxed);
        if base == 0 {
            return None;
        }
        let bus = (bdf >> 8) & 0xff;
        let dev = (bdf >> 3) & 0x1f;
        let func = bdf & 0x7;
        // ECAM: bus[27:20] device[19:15] function[14:12] offset[11:0].
        let offset = ((bus as u64) << 20) | ((dev as u64) << 15) | ((func as u64) << 12)
            | ((off as u64) & 0xfff);
        if offset >= ECAM_SIZE.load(Ordering::Relaxed) {
            return None; // past the window the tree described: not ours to touch
        }
        Some((base + offset) as usize)
    }

    /// Read one configuration register on behalf of the userspace enumerator.
    ///
    /// `None` means REFUSED, and there is exactly one reason to refuse: there is no ECAM window on
    /// this machine, so nothing could answer. An ABSENT DEVICE IS NOT A REFUSAL - the bus floats
    /// high and the read returns all-ones, which is data for the caller to interpret. Conflating the
    /// two would have the enumerator report "the kernel would not let me look" for every empty slot.
    ///
    /// Simpler than either existing backend, and worth saying why rather than leaving it looking
    /// like an omission. x86 and the Pi 4 both reach configuration space through an index/data
    /// register PAIR, which needs a lock so two callers cannot interleave a select with a read. ECAM
    /// is flat - the address IS the selector - so there is no window between selecting and reading
    /// for anyone to race into, and no lock to take. The Pi 4's bus-0 special case is absent for the
    /// same reason: there is no shared bridge register block that every slot would alias to.
    pub(super) fn cfg_read_gated(sel: u32, off: u16) -> Option<u32> {
        if ECAM_BASE.load(Ordering::Relaxed) == 0 {
            return None; // no host bridge in the device tree: nothing to read, and saying so
        }
        // The service's encoding, decoded into the `bdf` the rest of this module passes around.
        // Both are ECAM; they differ only in whether the fields arrive pre-shifted.
        let bus = (sel >> 20) & 0xff;
        let dev = (sel >> 15) & 0x1f;
        let func = (sel >> 12) & 0x7;
        cfg_read32((bus << 8) | (dev << 3) | func, off & 0xfff)
    }

    /// Read one configuration-space register.
    pub fn cfg_read32(bdf: u32, off: u16) -> Option<u32> {
        let addr = ecam_addr(bdf, off & !3)?;
        // SAFETY: an address inside the ECAM window the device tree described, 4-byte aligned.
        // Configuration reads have no side effects on a conforming device.
        Some(unsafe { (addr as *const u32).read_volatile() })
    }

    /// Write one configuration-space register.
    fn cfg_write32(bdf: u32, off: u16, val: u32) {
        if let Some(addr) = ecam_addr(bdf, off & !3) {
            // SAFETY: as above. The caller is enabling a device this kernel owns.
            unsafe { (addr as *mut u32).write_volatile(val) };
        }
    }

    // ---- The generic device table. Same shape on every arch so the spawn path stays neutral. ----
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

    /// A ceiling readable off the source (26.6.1). QEMU `virt` presents a handful; a board with more
    /// than this many is reported rather than silently truncated.
    pub const MAX_DEVICES: usize = 32;
    pub static DEVICE_COUNT: AtomicU32 = AtomicU32::new(0);
    static DEVICES: [DeviceCell; MAX_DEVICES] = [const { DeviceCell::new() }; MAX_DEVICES];

    /// One table slot, as atomics rather than a `static mut`, so the table needs no `unsafe`. It is
    /// written once during boot enumeration and read for the life of the machine.
    struct DeviceCell {
        bdf: AtomicU32,
        class_code: AtomicU32,
        bar: [AtomicU64; 6],
        irq_line: AtomicU8,
        vendor: AtomicU32,
        device: AtomicU32,
    }

    impl DeviceCell {
        const fn new() -> Self {
            DeviceCell {
                bdf: AtomicU32::new(0),
                class_code: AtomicU32::new(0),
                bar: [const { AtomicU64::new(0) }; 6],
                irq_line: AtomicU8::new(0),
                vendor: AtomicU32::new(0),
                device: AtomicU32::new(0),
            }
        }
    }

    pub fn device_at(n: usize) -> Option<PciDevice> {
        if n >= DEVICE_COUNT.load(Ordering::Acquire) as usize {
            return None;
        }
        let c = &DEVICES[n];
        let mut bar = [0u64; 6];
        for (i, b) in bar.iter_mut().enumerate() {
            *b = c.bar[i].load(Ordering::Relaxed);
        }
        Some(PciDevice {
            index: n,
            bdf: c.bdf.load(Ordering::Relaxed),
            class_code: c.class_code.load(Ordering::Relaxed),
            bar,
            irq_line: c.irq_line.load(Ordering::Relaxed),
            vendor: c.vendor.load(Ordering::Relaxed) as u16,
            device: c.device.load(Ordering::Relaxed) as u16,
        })
    }

    /// The first device whose 24-bit class/subclass/prog-if matches.
    pub fn find_by_class(class_code: u32) -> Option<PciDevice> {
        (0..DEVICE_COUNT.load(Ordering::Acquire) as usize)
            .filter_map(device_at)
            .find(|d| d.class_code == class_code)
    }

    pub fn ehci() -> Option<PciDevice> { find_by_class(0x0c_03_20) }
    /// The machine's xHCI controller, whether or not it arrived on a bus.
    ///
    /// **On this board the controller is soldered to the SoC, and the neutral kernel asks for it
    /// here.** That is not a mismatch to route around. The question the class resolution asks is "is
    /// there an xHCI controller, and where does it start", and this port can answer it; the answer
    /// simply does not come from a configuration space. So the bus scan is tried first, as it must be
    /// on a machine that has a card, and the SoC controller is offered when the scan finds nothing -
    /// carrying the no-bus sentinel for its address, because it genuinely has none.
    ///
    /// It follows that this driver is a TCB member on this board, for exactly the reason CLAUDE.md
    /// 6.4 gives for the Pi 4: there is no IOMMU here to confine it, so a compromised driver can aim
    /// the controller's DMA anywhere. What is bounded is the ACCIDENT surface - a restartable service
    /// rather than ring-0 code parsing descriptors supplied by whatever was plugged in - and the
    /// trust posture is not. Recorded here rather than implied.
    pub fn xhci() -> Option<PciDevice> {
        if let Some(d) = find_by_class(0x0c_03_30) {
            return Some(d);
        }
        let base = super::usb::window();
        if base == 0 {
            return None;
        }
        let mut bar = [0u64; 6];
        bar[0] = base;
        Some(PciDevice {
            index: 0,
            bdf: 0xFFFF,
            class_code: 0x0c_03_30,
            bar,
            irq_line: 0,
            vendor: 0,
            device: 0,
        })
    }
    /// The machine's ethernet controller, whether or not it arrived on a bus.
    ///
    /// Same shape and same reasoning as `xhci()` above: the class resolution asks "is there an
    /// ethernet controller, and where does it start", and on this board the answer is a Synopsys
    /// DesignWare MAC soldered to the SoC rather than a card. The bus scan is tried first, because a
    /// machine with a card should use it, and the SoC controller is offered only when the scan finds
    /// nothing - with the no-bus sentinel for its address, because it genuinely has none.
    pub fn nic() -> Option<PciDevice> {
        if let Some(d) = find_by_class(0x02_00_00) {
            return Some(d);
        }
        let base = super::net::window();
        if base == 0 {
            return None;
        }
        let mut bar = [0u64; 6];
        bar[0] = base;
        Some(PciDevice {
            index: 0,
            bdf: 0xFFFF,
            class_code: 0x02_00_00,
            bar,
            irq_line: 0,
            vendor: 0,
            device: 0,
        })
    }

    /// The first MEMORY BAR, with its flag bits removed and a 64-bit BAR joined to its upper half.
    ///
    /// A BAR's low bits are type flags, not address: bit 0 selects I/O versus memory, bits 2:1 give
    /// the width. Returning the raw register would hand a driver an address a few bytes off, which
    /// maps and then fails in a way that looks like a broken device.
    pub fn first_memory_bar(d: &PciDevice) -> u64 {
        let mut i = 0;
        while i < 6 {
            let raw = d.bar[i];
            if raw == 0 {
                i += 1;
                continue;
            }
            if raw & 1 != 0 {
                i += 1; // an I/O BAR, which this ISA cannot address at all
                continue;
            }
            let sixty_four = (raw >> 1) & 0x3 == 0x2;
            let addr = raw & !0xf;
            return if sixty_four && i + 1 < 6 {
                addr | (d.bar[i + 1] << 32)
            } else {
                addr
            };
        }
        0
    }

    /// The 32-bit memory window the host bridge forwards, and the next free address in it.
    ///
    /// **Nothing has assigned these BARs.** On a PC the firmware does it before the kernel runs; here
    /// OpenSBI does not, so every BAR reads back zero and a driver handed one would map address zero
    /// and find nothing. Linux assigns them itself from the bridge's `ranges`, and so does this.
    static MEM32_BASE: AtomicU64 = AtomicU64::new(0);
    static MEM32_END: AtomicU64 = AtomicU64::new(0);
    static MEM32_NEXT: AtomicU64 = AtomicU64::new(0);

    /// Record the bridge's 32-bit memory window, from the tree's `ranges`.
    pub(super) fn set_mem_window(base: u64, size: u64) {
        MEM32_BASE.store(base, Ordering::Relaxed);
        MEM32_END.store(base.saturating_add(size), Ordering::Relaxed);
        MEM32_NEXT.store(base, Ordering::Relaxed);
    }

    /// Carve `size` bytes out of the window, aligned as a BAR requires (to its own size).
    fn alloc_mem(size: u64) -> Option<u64> {
        if size == 0 {
            return None;
        }
        let end = MEM32_END.load(Ordering::Relaxed);
        let mut at = MEM32_NEXT.load(Ordering::Relaxed);
        at = (at + size - 1) & !(size - 1); // a BAR must be aligned to its own size
        if at.saturating_add(size) > end {
            return None;
        }
        MEM32_NEXT.store(at + size, Ordering::Relaxed);
        Some(at)
    }

    /// Size and place one device's BARs, and report each one's type.
    ///
    /// **The type is in the PROBE, not in the current value.** An unassigned BAR reads back zero on
    /// every bit including bit 0, so asking "is bit 0 set" of the value already there says "memory"
    /// about an I/O BAR just as confidently as about a real one. Firmware has assigned nothing here,
    /// so every BAR looks like memory - and the first version of this put a memory address into an
    /// AHCI controller's legacy I/O BAR0 and then handed that address to the driver as if it were the
    /// register window. The type only exists once all-ones has been written and read back.
    ///
    /// Returns the six BAR values to record, with **zero for an I/O BAR** - deliberately, and for the
    /// same reason x86 does it: "the first non-zero BAR" is then the register window on every device
    /// without the kernel being told which device it is looking at. An AHCI controller keeps its
    /// registers in BAR5 and its legacy IDE ports in BAR0-4; an xHCI uses BAR0. One rule, no table of
    /// exceptions.
    fn assign_bars(bdf: u32) -> [u64; 6] {
        let mut out = [0u64; 6];
        let mut i = 0usize;
        while i < 6 {
            let off = 0x10 + (i as u16) * 4;
            let Some(orig) = cfg_read32(bdf, off) else { return out };
            cfg_write32(bdf, off, 0xffff_ffff);
            let probe = cfg_read32(bdf, off).unwrap_or(0);
            cfg_write32(bdf, off, orig);
            if probe == 0 {
                i += 1;
                continue; // not implemented
            }
            if probe & 1 != 0 {
                // An I/O BAR. This ISA has no I/O space at all, so it is left unassigned and recorded
                // as zero - which is what makes `first_memory_bar` correct by construction.
                i += 1;
                continue;
            }
            let sixty_four = (probe >> 1) & 0x3 == 0x2;
            let mask = probe & !0xf;
            if mask == 0 {
                i += if sixty_four { 2 } else { 1 };
                continue;
            }
            let size = (!(mask as u64) & 0xffff_ffff).wrapping_add(1);
            match alloc_mem(size) {
                Some(addr) => {
                    cfg_write32(bdf, off, (addr as u32) | (probe & 0xf));
                    if sixty_four {
                        cfg_write32(bdf, off + 4, (addr >> 32) as u32);
                    }
                    out[i] = addr | ((probe & 0xf) as u64);
                }
                None => {
                    super::print_str("riscv64: pci - no room in the memory window for a BAR\n");
                }
            }
            i += if sixty_four { 2 } else { 1 };
        }
        out
    }

    /// Walk every bus, device and function, and record what answers.
    ///
    /// Bounded twice over: by the ECAM window the tree described, and by `MAX_DEVICES`. A machine
    /// with more devices than the table holds is REPORTED rather than quietly truncated - a driver
    /// missing because its device fell off the end of a table is a bug that looks like absent
    /// hardware.
    pub fn init() {
        if ECAM_BASE.load(Ordering::Relaxed) == 0 {
            return; // no PCI on this machine, which is a true answer and not a failure
        }
        let mut n = 0usize;
        let mut overflowed = false;
        'buses: for bus in 0..256u32 {
            for dev in 0..32u32 {
                for func in 0..8u32 {
                    let bdf = (bus << 8) | (dev << 3) | func;
                    let Some(id) = cfg_read32(bdf, 0) else { continue };
                    let vendor = id & 0xffff;
                    if vendor == 0xffff {
                        // Nothing here. Function 0 absent means the whole device is absent, which is
                        // what makes a full walk affordable.
                        if func == 0 {
                            break;
                        }
                        continue;
                    }
                    if n >= MAX_DEVICES {
                        overflowed = true;
                        break 'buses;
                    }
                    // PLACE the BARs before reading them back. Nothing else has: the table would
                    // otherwise record six zeros and hand a driver address zero.
                    let bars = assign_bars(bdf);
                    let class = cfg_read32(bdf, 0x08).unwrap_or(0) >> 8;
                    let irq = (cfg_read32(bdf, 0x3c).unwrap_or(0) & 0xff) as u8;
                    let c = &DEVICES[n];
                    c.bdf.store(bdf, Ordering::Relaxed);
                    c.class_code.store(class, Ordering::Relaxed);
                    c.vendor.store(vendor, Ordering::Relaxed);
                    c.device.store((id >> 16) & 0xffff, Ordering::Relaxed);
                    c.irq_line.store(irq, Ordering::Relaxed);
                    for b in 0..6usize {
                        c.bar[b].store(bars[b], Ordering::Relaxed);
                    }
                    n += 1;
                    // A single-function device says so in its header type; asking its other seven
                    // functions is harmless but pointless.
                    if func == 0 && cfg_read32(bdf, 0x0c).unwrap_or(0) & 0x0080_0000 == 0 {
                        break;
                    }
                }
            }
        }
        DEVICE_COUNT.store(n as u32, Ordering::Release);
        super::print_str("riscv64: pci ecam at ");
        super::print_hex(ECAM_BASE.load(Ordering::Relaxed));
        super::print_str(", ");
        super::print_dec(n as u64);
        super::print_str(" device(s)");
        if overflowed {
            super::print_str(" - TABLE FULL, some not enumerated");
        }
        super::print_str("\n");
        for i in 0..n {
            if let Some(d) = device_at(i) {
                super::print_str("riscv64:   bdf ");
                super::print_hex(d.bdf as u64);
                super::print_str(" class ");
                super::print_hex(d.class_code as u64);
                super::print_str(" ");
                super::print_hex(d.vendor as u64);
                super::print_str(":");
                super::print_hex(d.device as u64);
                super::print_str(" bar0 ");
                super::print_hex(first_memory_bar(&d));
                super::print_str("\n");
            }
        }
    }

    /// Command register bit 2: allow this device to originate DMA. Without it a bus-mastering
    /// controller reads and writes nothing and reports no error - it simply never transfers.
    pub fn set_bus_master(bdf: u32) {
        if let Some(cmd) = cfg_read32(bdf, 0x04) {
            cfg_write32(bdf, 0x04, cmd | (1 << 2) | (1 << 1));
        }
    }

    pub fn clear_bus_master(bdf: u32) {
        if let Some(cmd) = cfg_read32(bdf, 0x04) {
            cfg_write32(bdf, 0x04, cmd & !(1 << 2));
        }
    }

    /// Power state D0. QEMU's devices come up in D0 and this port has no board device that does not,
    /// so this is a no-op that exists to answer the seam rather than a step being skipped.
    pub fn set_power_d0(_bdf: u32) {}

    pub fn xhci_bios_handoff() {}
    pub fn ehci_flr_probe() {}

    /// MSI and MSI-X need an interrupt controller to deliver INTO, and this port has no PLIC yet.
    /// Refusing is the honest answer: a driver that is told its MSI was programmed and then never
    /// receives one waits forever, which is the failure mode invariant 12 exists to prevent.
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
    use super::*;

    /// Where a secondary hart begins, in the state the firmware leaves it: `satp` zero, no stack, no
    /// trap vector, `a0` its hart id and `a1` whatever `hart_start` was given as `opaque`.
    ///
    /// It has to repeat the work `_start` did, because HSM releases a hart rather than cloning one.
    /// Naked, and only three instructions of it: a stack must exist before Rust runs, and the id must
    /// be parked in `tp` before anything asks which hart this is.
    #[unsafe(naked)]
    unsafe extern "C" fn ap_entry() -> ! {
        core::arch::naked_asm!(
            "mv tp, a0",   // this hart's identity, where `get_lapic_id` looks for it
            "mv sp, a1",   // the stack `start_all_aps` reserved for it
            "j  {main}",
            main = sym ap_main,
        )
    }

    /// The Rust half of a secondary hart's bring-up: install the kernel's address space and trap
    /// vector, then join the scheduler.
    ///
    /// ORDER IS THE WHOLE THING. `satp` first, because everything after it - the trap vector's own
    /// address, the per-core arenas, the scheduler's tables - is only reachable through the kernel's
    /// map. `stvec` second, so the first fault after that has somewhere to go. Only then does this
    /// hart say it is ready, because `mark_ready` is what makes other cores start routing work to it.
    extern "C" fn ap_main(hartid: usize) -> ! {
        let root = KERNEL_ROOT.load(Ordering::Relaxed);
        if root == 0 {
            // The boot hart has not finished building the kernel's map. Nothing this hart can do is
            // safe, and it cannot report - so park it rather than run without an address space.
            halt();
        }
        // SAFETY: the kernel's own root, built by the boot hart and unchanged since; it maps this
        // code, this hart's stack (a static in the kernel image) and the trap vector.
        unsafe { page_tables::write_page_table_base(root) };

        if !trap::init() {
            // No vector: a fault here would be silent. Say so through the UART, which the map above
            // makes reachable, and stop this hart rather than run it blind.
            print_str("riscv64: AP trap vector REFUSED - parking this hart\n");
            halt();
        }

        // The core id this hart was ASSIGNED before it was started. Asking `lapic_to_core_id` here
        // cannot work - see `HART_TO_CORE` - and its answer would be a plausible, wrong 0.
        let Some(core_id) = hart_core(hartid as u32) else {
            print_str("riscv64: hart started with no core assignment - parking it\n");
            halt();
        };
        // This hart's own tick. Each hart arms its own `stimecmp` through SBI - there is no shared
        // periodic timer to inherit - so a hart that skipped this would idle forever.
        start_timer_this_hart();
        // Software interrupts too: an IPI is how another core wakes this one, and `sie.SSIE` is what
        // admits it.
        trap::enable_software_interrupts();

        crate::smp::core::mark_ready(core_id);
        crate::kprintln!("smp: hart {} ready as core {}", hartid, core_id);
        crate::task::scheduler::run(core_id)
    }

    /// Release every usable hart but this one.
    ///
    /// One firmware call each, and no trampoline: SBI HSM is what x86's real-mode `ap_boot` is for.
    /// A hart that the firmware refuses is REPORTED and skipped - the system continues on the cores
    /// that did come up (11.3), which is the same rule x86 follows when an AP does not answer.
    ///
    /// # Safety
    /// Must run once, on the boot hart, after the kernel's address space and the per-core arenas
    /// exist - the harts it starts read both immediately.
    pub unsafe fn start_all_aps(_boot_info: &BootInfo) {
        let me = BOOT_HART.load(Ordering::Relaxed);
        let mut started = 0usize;
        let n = HART_COUNT.load(Ordering::Relaxed) as usize;
        for i in 0..n.min(AP_MAX + 1) {
            let hart = HART_IDS[i].load(Ordering::Relaxed);
            if hart == me {
                continue;
            }
            if started >= AP_MAX {
                crate::kprintln!("smp: more harts than reserved stacks ({}) - hart {} not started",
                                 AP_MAX, hart);
                break;
            }
            // Each secondary gets its own slice of the stack arena. `&raw` rather than a reference,
            // so no `&mut` to a static is ever created.
            let base = (&raw mut AP_STACKS) as usize + started * AP_STACK_BYTES;
            let top = (base + AP_STACK_BYTES) & !0xf;
            // The core id this hart will answer to, published BEFORE it starts: it calls
            // `lapic_to_core_id` almost immediately, and an unpublished id would resolve to core 0 -
            // two harts believing they are the same core, which is the x86 bug this project already
            // has a name for.
            if hart as usize >= MAX_HART_ID {
                crate::kprintln!("smp: hart {} is past the id table - not started", hart);
                continue;
            }
            let core = (started + 1) as u32;
            // BOTH directions, and both BEFORE the hart runs: the neutral core->hart map so
            // `lapic_to_core_id` resolves once this hart is ready, and this arch's hart->core map so
            // the hart can learn which core it is in the first place.
            crate::smp::core::set_core_lapic_id(core, hart);
            set_hart_core(hart, core);
            if sbi::hart_start(hart as u64, ap_entry as *const () as u64, top as u64) {
                started += 1;
            } else {
                crate::kprintln!("smp: firmware refused to start hart {} - continuing without it", hart);
            }
        }
        if started == 0 {
            crate::kprintln!("smp: no secondary harts started - running single-core");
        }
    }
}


/// Build a page table, map a page, read it back, unmap it. Reports a wrong number rather than
/// causing a fault, because it runs while translation is still off.
///
/// Checks the two rules that are easy to get wrong and hard to see: that a WRITABLE mapping is also
/// READABLE (`W` without `R` is a reserved encoding), and that `A`/`D` are set by software (the
/// JH7110 has no `svadu`, so a leaf without them faults on first touch there and works in QEMU).
fn sv39_selftest() {
    use crate::arch::imp::page_tables::{PageFlags, PageTable, VirtAddr};
    use crate::memory::frame::PhysAddr;

    let Ok(mut pt) = PageTable::new() else {
        print_str("riscv64: sv39 SELFTEST FAILED - no frame for a root table
");
        return;
    };
    // A virtual address in the user half, far from anything this kernel maps, and a physical frame
    // we know exists: the page the kernel itself starts on.
    let va = VirtAddr(0x0000_0010_0000_0000);
    let pa = PhysAddr((&raw const __kernel_start) as u64);
    let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;

    if pt.map(va, pa, flags).is_err() {
        print_str("riscv64: sv39 SELFTEST FAILED - map
");
        return;
    }
    let Some(pte) = sv39::translate(pt.cr3_value(), va.0) else {
        print_str("riscv64: sv39 SELFTEST FAILED - mapped page does not translate
");
        return;
    };
    let ok_addr = sv39::pte_phys(pte) == pa.0;
    let ok_rw = pte & sv39::PTE_R != 0 && pte & sv39::PTE_W != 0;
    let ok_ad = pte & sv39::PTE_A != 0 && pte & sv39::PTE_D != 0;
    let ok_user = pte & sv39::PTE_U != 0;
    let ok_unmap = pt.unmap(va).is_ok() && sv39::translate(pt.cr3_value(), va.0).is_none();

    print_str("riscv64: sv39 selftest addr=");
    print_str(if ok_addr { "ok" } else { "BAD" });
    print_str(" rw=");
    print_str(if ok_rw { "ok" } else { "BAD" });
    print_str(" a/d=");
    print_str(if ok_ad { "ok" } else { "BAD" });
    print_str(" user=");
    print_str(if ok_user { "ok" } else { "BAD" });
    print_str(" unmap=");
    print_str(if ok_unmap { "ok" } else { "BAD" });
    print_str("
");
}


/// Build the kernel's own address space and turn translation on.
///
/// Identity, via 1 GiB leaves, covering everything from zero to the top of RAM - so the kernel's
/// code, its stack, the frame allocator's bitmaps, the page tables themselves, the device tree, the
/// UART and the PLIC are all reachable at the addresses they already have. Enabling translation
/// then changes no address that is currently in flight, which is the only version of this step that
/// can be debugged afterwards.
///
/// The three prints are the instrument. If the machine stops after "building" the table could not
/// be allocated or filled; after "enabling" the write itself faulted; and reaching "on" means
/// translation is live and the UART is still reachable through it. Silence with no line at all
/// would mean the fault came before any of this, which is a different bug entirely.
fn enable_paging(bi: &BootInfo) {
    let Some(root) = sv39::new_root() else {
        print_str("riscv64: paging FAILED - no frame for the root table
");
        return;
    };

    // Where RAM ends, from the map that was built from the device tree. Rounded UP to a gigabyte so
    // the final partial leaf still covers the top of memory rather than stopping short of it.
    let mut top: u64 = 0;
    for r in bi.memory_map {
        top = top.max(r.base.saturating_add(r.len));
    }
    let top = (top + ((1 << 30) - 1)) & !((1u64 << 30) - 1);

    // Kernel access, every permission: this single map covers code, data, stack and MMIO, and
    // splitting it into properly-permissioned regions is a later change with its own risks. `U` is
    // NOT set - nothing here is reachable from user mode, which is the one permission that would be
    // a security property rather than a convenience.
    let bits = sv39::PTE_V | sv39::PTE_R | sv39::PTE_W | sv39::PTE_X | sv39::PTE_A | sv39::PTE_D;

    print_str("riscv64: building identity map to ");
    print_hex(top);
    print_str("
");
    if sv39::identity_map_gigapages(root, top, bits).is_err() {
        print_str("riscv64: paging FAILED - could not fill the root table
");
        return;
    }

    print_str("riscv64: enabling paging, satp root ");
    print_hex(root);
    print_str("
");
    // SAFETY: the table built above maps every address identically from zero to the top of RAM,
    // which includes the instruction stream executing this write and the stack it runs on. The
    // fence inside `write_page_table_base` discards translations from before the change.
    unsafe { page_tables::write_page_table_base(root) };

    // The one root every service's address space is built from. Recorded HERE, at the moment it
    // becomes the kernel's map, rather than read back later from a `satp` that may belong to a task.
    KERNEL_ROOT.store(root, Ordering::Relaxed);

    // Reaching here means the UART was reachable THROUGH the new table, not merely before it.
    print_str("riscv64: paging on, sv39 active
");
}


/// Ticks counted since the timer was started, and the interval between them.
static TICKS: AtomicUsize = AtomicUsize::new(0);
static TICK_INTERVAL: AtomicUsize = AtomicUsize::new(0);
/// The machine's monotonic counter rate, from the device tree. Kept because the idle tick is
/// expressed in SECONDS while the quantum is expressed in milliseconds, and deriving one from the
/// other needs the rate rather than a ratio.
static TIMEBASE_HZ: AtomicU32 = AtomicU32::new(0);
/// Ticks of whatever `read_cycle_counter` returns in one 10 ms quantum, MEASURED at boot against the
/// device tree's timebase. Zero until then, and the fallback is the timebase's own rate.
static CYCLES_PER_QUANTUM: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
/// Set once the neutral scheduler owns this core. Until then the tick is the boot's own; after it,
/// every tick is a preemption point and belongs to `scheduler::timer_tick_from_irq`.
static NEUTRAL_SCHED: AtomicBool = AtomicBool::new(false);
/// The `time` CSR at boot, so elapsed seconds can be derived without an RTC.
///
/// This board has no battery-backed clock - and neither does the Pi - so "what time is it" and "how
/// long have we been up" are different questions with different answers. This is the second one, and
/// it is the one a `wait` or an `uptime` actually needs: a count of seconds since the kernel started,
/// which the machine can answer on its own.
static BOOT_TIME: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// Which IPI vectors are pending for each core, one bit per vector.
///
/// **RISC-V's IPI carries no vector.** An APIC interrupt says which of 256 things happened; SBI's
/// `send_ipi` says only "someone poked you". So the vector travels out of band: the sender sets a
/// bit here for the target and then pokes it, and the receiver drains whatever it finds. A MASK and
/// not a single value, because two senders can arrive between one hart's poke and its handler, and
/// the second must not overwrite the first - which would lose a TLB shootdown and leave its
/// initiator spinning on an acknowledgement that never comes.
static IPI_PENDING: crate::smp::percpu::PerCore<AtomicU32> = crate::smp::percpu::PerCore::new();

/// Bit position for a vector in `IPI_PENDING`. The three neutral vectors are 0xF0..0xF2.
#[inline]
fn ipi_bit(vector: u8) -> u32 {
    1u32 << (vector & 0x0f)
}

/// Kernel stacks for the secondary harts, and how far into them each one starts.
///
/// A fixed arena rather than an allocation per hart, sized off the source: four secondaries at
/// 32 KiB each. A hart whose stack could not be found does not start, which is a reported failure
/// rather than one that runs with a stack pointer nobody chose.
const AP_STACK_BYTES: usize = 32 * 1024;
const AP_MAX: usize = 4;
#[repr(align(16))]
struct ApStacks([u8; AP_STACK_BYTES * AP_MAX]);
static mut AP_STACKS: ApStacks = ApStacks([0; AP_STACK_BYTES * AP_MAX]);

/// The KERNEL's own Sv39 root, recorded when paging is enabled.
///
/// **Not "whatever `satp` currently holds".** A service's address space is built by
/// `finalize_service_address_space`, which runs inside the spawn - and a spawn is a SYSCALL, made by
/// a task, with that task's root live. Cloning from the live root worked for the supervisor (spawned
/// by the kernel, from the kernel's root) and silently broke every service the supervisor spawned:
/// the supervisor's root reaches the low gigabyte through a POINTER table, because its own text sits
/// at 0x400000, and a clone that copies root-level LEAVES skips a pointer. So the new service
/// inherited RAM but not the UART or the PLIC, and the first line the kernel tried to log on its
/// behalf faulted on 0x1000_0005 - forever, silently, because the fault handler is what wanted to
/// print. QEMU's `-d int` named it in one line after reasoning had not.
static KERNEL_ROOT: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);


/// One scheduler tick.
///
/// ACKNOWLEDGED BY SCHEDULING THE NEXT ONE. There is no "clear" bit for the supervisor timer: the
/// interrupt is asserted for as long as the deadline is in the past, so a handler that returns
/// without setting a new one re-enters immediately and forever. That live lock presents as a machine
/// which boots and then does nothing, with no fault to report - which is why it is worth naming here
/// rather than discovering.
fn timer_tick(frame: &mut trap::TrapFrame) {
    note_stage(stage::TIMER_ENTER);
    let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    let interval = TICK_INTERVAL.load(Ordering::Relaxed) as u64;
    sbi::set_timer(sbi::time().wrapping_add(interval));
    note_stage(stage::TIMER_REARMED);

    // A tick taken while user mode was running is the preemption path, and the user-mode selftest is
    // the only thing that currently notices. Offered AFTER the next deadline is set, so the machine
    // is never left without one whatever the hook decides to do with the frame.
    note_stage(stage::USERMODE_HOOK);
    usermode::on_timer_tick(frame);

    // ONCE THE SCHEDULER OWNS THE CORE, A TICK IS A PREEMPTION POINT and belongs to neutral code.
    // `timer_tick_from_irq` may switch away from the interrupted task; when something switches back,
    // it RETURNS here, this function returns, and the trap epilogue restores the frame and resumes
    // the task. That works because the frame is on the task's own kernel stack, which the context
    // switch saves and restores as `sp` - which is the whole reason the `sscratch` latch had to exist
    // before this line could.
    note_stage(stage::FB_PUBLISH);
    publish_framebuffer_on_tick();

    if NEUTRAL_SCHED.load(Ordering::Relaxed) {
        note_stage(stage::NEUTRAL_SCHED);
        // SAFETY: the neutral preemption entry, reached only from this handler, with interrupts
        // masked by the trap and running on the interrupted task's kernel stack - the same contract
        // the ARM port's call site documents, met the same way.
        unsafe { crate::task::scheduler::timer_tick_from_irq(0, 0, 0) };
        note_stage(stage::TICK_DONE);
        return;
    }

    // Before that: the boot's own tick, printed sparsely enough to prove it is alive and periodic.
    // ONCE, not five times. These counted ticks were how the boot proved its timer was periodic
    // rather than a single interrupt that happened to arrive - a real question before there was a
    // scheduler, and answered permanently by the fact that one now runs. Five lines of a
    // forty-eight row console is a tenth of the screen spent saying the clock still works.
    if n == 1 {
        print_str("riscv64: tick ");
        print_dec(n as u64);
        print_str("\n");
    }
}

/// Is `cycle` readable from S-mode on this machine? Answered by the boot probe, not assumed.
static RDCYCLE_OK: AtomicBool = AtomicBool::new(false);
/// Set only while the probe's own instruction is executing, so the trap handler knows that one
/// illegal instruction is expected and everything else still halts.
static RDCYCLE_PROBING: AtomicBool = AtomicBool::new(false);

pub(super) fn rdcycle_available() -> bool {
    RDCYCLE_OK.load(Ordering::Relaxed)
}

/// Called from the trap handler for an illegal instruction. Returns true if it was the probe's.
pub(super) fn claim_rdcycle_probe() -> bool {
    RDCYCLE_PROBING.swap(false, Ordering::AcqRel)
}

/// Find out whether this machine lets S-mode read the cycle counter, by trying it.
///
/// **Asking is the only way.** `cycle` is readable only if M-mode set `mcounteren.CY`, there is no
/// bit to read that says so, and getting it wrong is an illegal-instruction trap rather than a zero.
/// The firmware's own banner lists `zicntr` among the ISA extensions, which says the counter EXISTS,
/// not that this privilege level may read it.
///
/// So the instruction is executed deliberately, with the handler told to expect exactly one of them -
/// the same shape as the user-mode selftest's deliberate page fault, and armed just as narrowly. If
/// it faults, the handler steps over it and the flag stays false; if it returns, the flag is set.
fn probe_rdcycle() {
    RDCYCLE_PROBING.store(true, Ordering::Release);
    let c: u64;
    // SAFETY: this is the probe. Either it reads a counter with no side effects, or it raises an
    // illegal instruction that `claim_rdcycle_probe` steps over - and `csrr` has no compressed
    // encoding, so the four bytes the handler skips are exactly this instruction.
    unsafe { core::arch::asm!("csrr {}, cycle", out(reg) c, options(nomem, nostack)) };
    // Reaching here means no trap was taken. `claim_rdcycle_probe` would have cleared the flag if one
    // had been, so a still-set flag is the proof.
    if RDCYCLE_PROBING.swap(false, Ordering::AcqRel) {
        RDCYCLE_OK.store(true, Ordering::Release);
    }
    let _ = c;
    print_str("riscv64: cycle counter ");
    if RDCYCLE_OK.load(Ordering::Relaxed) {
        print_str("readable (rdcycle) - userspace cycle budgets mean what they say");
    } else {
        print_str("NOT readable from S-mode; falling back to `time`, which is ");
        print_dec(TIMEBASE_HZ.load(Ordering::Relaxed) as u64);
        print_str(" Hz - cycle-denominated waits will be far longer than intended");
    }
    print_str("\n");
}

/// Time the counter USERSPACE will read against the one the device tree describes.
///
/// Ten milliseconds of boot, once, to learn a number every duration computed above the kernel
/// depends on. Measured rather than read from anywhere because nothing reports it: the device tree
/// gives the TIMEBASE frequency, which is a different clock from the CPU's cycle counter, and this
/// part has no register that states the core's frequency.
fn calibrate_cycle_counter() {
    let hz = TIMEBASE_HZ.load(Ordering::Relaxed) as u64;
    if hz == 0 {
        return;
    }
    let window = hz / 100; // 10 ms, in timebase ticks
    let t0 = sbi::time();
    let c0 = uaccess::read_cycle_counter();
    while sbi::time().wrapping_sub(t0) < window {
        core::hint::spin_loop();
    }
    let cycles = uaccess::read_cycle_counter().wrapping_sub(c0);
    if cycles == 0 {
        return;
    }
    CYCLES_PER_QUANTUM.store(cycles, Ordering::Relaxed);
    print_str("riscv64: cycle counter ");
    // ROUNDED, not truncated. 10 ms is 10,000 microseconds, so ticks-per-quantum over 10,000
    // is megahertz - and 39,999 ticks is 4 MHz, which integer division reports as 3. The
    // measurement below it was right all along; only the label was understating it by most of
    // a megahertz, which is exactly the kind of instrument that gets believed over the number
    // beside it. (That 4 MHz is also a fact worth reading: it is the device tree's TIMEBASE
    // rate, so `rdcycle` on this part is aliased to the timebase rather than to the core
    // clock, whatever its name suggests.)
    print_dec((cycles + 5_000) / 10_000);
    print_str(" MHz, ");
    print_dec(cycles);
    print_str(" ticks per 10 ms quantum\n");
}

/// The machine's monotonic counter rate, for anything that needs to bound a wait in real time.
pub(super) fn timebase_hz() -> u32 {
    TIMEBASE_HZ.load(Ordering::Relaxed)
}

/// Clear this hart's pending software interrupt.
///
/// `sip.SSIP` is writable from S-mode on any SBI 1.0 platform, which is what makes the IPI
/// acknowledgeable without another firmware call.
fn clear_software_interrupt() {
    // SAFETY: clearing one bit of `sip`, which acknowledges an interrupt already taken.
    unsafe {
        core::arch::asm!("csrc sip, {ssip}", ssip = in(reg) 1u64 << 1, options(nostack));
    }
}

/// Run whatever vectors were left for this core, then leave the mask empty.
///
/// `swap(0)` rather than read-then-clear: a sender adding a vector between the two would have it
/// erased. Taking the whole mask atomically means the worst case is a vector handled twice, which
/// every one of them tolerates, rather than one dropped, which none of them do.
fn drain_ipis() {
    if !IPI_PENDING.initialised() {
        return;
    }
    // SAFETY: reading this hart's own id.
    let core = crate::smp::core::lapic_to_core_id(unsafe { boot::get_lapic_id() }) as usize;
    if core >= crate::smp::percpu::num_cores() {
        return;
    }
    let pending = IPI_PENDING.get(core).swap(0, Ordering::Acquire);
    for vector in [
        crate::smp::ipi::vectors::WAKE_RECEIVER,
        crate::smp::ipi::vectors::TLB_SHOOTDOWN,
        crate::smp::ipi::vectors::SCHEDULER_TICK,
    ] {
        if pending & ipi_bit(vector) != 0 {
            // SAFETY: the neutral IPI handler, called with interrupts masked by the trap and on this
            // hart's kernel stack - the same context every other port's IPI stub provides.
            unsafe { crate::smp::ipi::ipi_handler(vector) };
        }
    }
}

/// Arm THIS hart's tick at the quantum already established by the boot hart.
///
/// Every hart owns its own `stimecmp` - there is no shared periodic source to inherit - so a
/// secondary that skipped this would never be preempted and would idle forever with work queued.
fn start_timer_this_hart() {
    let interval = TICK_INTERVAL.load(Ordering::Relaxed) as u64;
    if interval != 0 {
        sbi::set_timer(sbi::time().wrapping_add(interval));
        trap::enable_timer_interrupts();
    }
}

/// Start the scheduler tick, at the quantum the constitution specifies.
///
/// The interval is derived from the machine's own `timebase-frequency` rather than a constant: QEMU
/// counts at 10 MHz and the JH7110 at 4, so any fixed number would be a 2.5x error on one of them -
/// a scheduler running at the wrong speed, which is the kind of wrong that looks like working.
fn start_timer(hz: u32) -> bool {
    if hz == 0 || !sbi::probe(sbi::EXT_TIME) {
        return false;
    }
    // 10 ms, per CLAUDE.md 9.1. Written as a division of the machine's rate so the QUANTUM is the
    // constant and the tick count is derived, rather than the other way round.
    let interval = (hz as usize) / 100;
    TICK_INTERVAL.store(interval, Ordering::Relaxed);
    TIMEBASE_HZ.store(hz, Ordering::Relaxed);
    BOOT_TIME.store(sbi::time(), Ordering::Relaxed);
    if !sbi::set_timer(sbi::time().wrapping_add(interval as u64)) {
        return false;
    }
    trap::enable_timer_interrupts();
    true
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
