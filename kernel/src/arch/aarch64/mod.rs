// SPDX-License-Identifier: GPL-2.0-only
//! AArch64 arch layer - STUB scaffold for the demarcation test (docs/aarch64.md).
//!
//! Every item here mirrors the `arch::imp` surface `arch/x86_64/` exposes, so the arch-NEUTRAL kernel
//! compiles for `aarch64-unknown-none` with only this file written. Bodies are `unimplemented!()` /
//! stubs for now - the POINT of this stage is to prove the boundary: a clean compile means no neutral
//! file names anything x86-specific; any error OUTSIDE arch/aarch64/ is a leak to fix. Real AArch64
//! bodies (GIC, MMU, EL0/EL1, PSCI, PL011, generic timer) come next, incrementally, toward a QEMU boot.

#![allow(unused_variables, dead_code)]

use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};

#[cfg(feature = "pi4")]
pub mod context;
#[cfg(feature = "pi4")]
pub mod ctxdemo;
#[cfg(feature = "pi4")]
pub mod exceptions;
#[cfg(feature = "pi4")]
pub mod gic;
#[cfg(feature = "pi4")]
pub mod mailbox;
#[cfg(feature = "pi4")]
pub mod memmap;
#[cfg(feature = "pi4")]
pub mod mmu;
#[cfg(feature = "pi4")]
pub mod ptables;
#[cfg(all(feature = "pi4", feature = "pi4-sched-demo"))]
pub mod sched_demo;
#[cfg(feature = "pi4")]
pub mod timer;
#[cfg(feature = "pi4")]
pub mod usermode;

/// Reload value for the periodic tick, in generic-timer counter ticks. The timer is one-shot, so the
/// IRQ handler re-arms with this every time. Published here because the handler runs in an interrupt
/// context and must not recompute it.
#[cfg(feature = "pi4")]
pub static TIMER_INTERVAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// ============================ Boot bring-up ============================
//
// Two boards share this file, selected by the `pi4` cargo feature:
//
// - **QEMU `virt`** (default): the arch-boundary demarcation target. PL011 at 0x0900_0000, image
//   loaded at 0x4008_0000, entry at EL1.
// - **Raspberry Pi 4 Model B** (`--features pi4`): BCM2711. Peripherals at 0xFE00_0000 (low-peripheral
//   mode), so PL011 is at 0xFE20_1000. The VideoCore firmware loads a flat `kernel8.img` at 0x80000 -
//   note 0x80000, not the 32-bit port's 0x8000 - and enters at **EL2**.

/// PL011 base for the selected board.
#[cfg(not(feature = "pi4"))]
const PL011_BASE: usize = 0x0900_0000; // QEMU `virt`
#[cfg(feature = "pi4")]
const PL011_BASE: usize = 0xFE20_1000; // BCM2711 (peripheral base 0xFE00_0000 + 0x20_1000)

/// Offset added to every peripheral address, so the same MMIO code works on both sides of the move to
/// the high half.
///
/// Before the kernel relocates, peripherals are reached at their physical addresses through the low
/// identity map, and this is `0`. Afterwards `TTBR0` belongs to a task and the low map is gone, so the
/// only route to a device register is the kernel's high direct map - and this becomes `KERNEL_VA_BASE`.
///
/// **This is what made the low map look load-bearing when it was not.** Retiring `TTBR0` immediately
/// killed the boot, and the cause was not the kernel's code or stack (both already high) but
/// `put_str` writing to the UART at `0xFE20_1000`. The machine went silent at exactly the instruction
/// that would have reported the problem, which is the least informative failure available.
#[cfg(feature = "pi4")]
static MMIO_OFF: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// A peripheral register's address, wherever the kernel currently reaches peripherals from.
#[cfg(feature = "pi4")]
#[inline]
fn mmio(phys: usize) -> usize {
    phys + MMIO_OFF.load(core::sync::atomic::Ordering::Relaxed)
}

/// Switch every peripheral access to the high half. Called once, right after the kernel relocates and
/// before the low map is retired.
#[cfg(feature = "pi4")]
fn mmio_go_high() {
    MMIO_OFF.store(
        mmu::KERNEL_VA_BASE as usize,
        core::sync::atomic::Ordering::Relaxed,
    );
}

#[cfg(not(feature = "pi4"))]
#[inline]
fn mmio(phys: usize) -> usize { phys }

const PL011_DR: *mut u8 = PL011_BASE as *mut u8;
/// Flag register (+0x18). Bit 5 = TXFF (transmit FIFO full), bit 3 = BUSY.
const PL011_FR: *const u32 = (PL011_BASE + 0x18) as *const u32;
const PL011_FR_TXFF: u32 = 1 << 5;
#[cfg(feature = "pi4")]
const PL011_FR_BUSY: u32 = 1 << 3;
#[cfg(feature = "pi4")]
const PL011_LCRH: *mut u32 = (PL011_BASE + 0x2C) as *mut u32;
#[cfg(feature = "pi4")]
const PL011_CR: *mut u32 = (PL011_BASE + 0x30) as *mut u32;
#[cfg(feature = "pi4")]
const PL011_LCRH_8N1: u32 = (3 << 5) | (1 << 4); // WLEN=8, FIFOs enabled
#[cfg(feature = "pi4")]
const PL011_CR_ON: u32 = (1 << 0) | (1 << 8) | (1 << 9); // UARTEN | TXE | RXE

// BCM2711 GPIO. Peripheral base + 0x20_0000.
#[cfg(feature = "pi4")]
const GPIO_BASE: usize = 0xFE20_0000;
#[cfg(feature = "pi4")]
const GPFSEL1: *mut u32 = (GPIO_BASE + 0x04) as *mut u32; // function select, GPIO10-19
/// BCM2711 pull-up/down control. **This is not the BCM2835 mechanism.** The older SoC used a
/// GPPUD + GPPUDCLK strobe sequence; BCM2711 replaced it with direct 2-bits-per-pin registers
/// (00 = none, 01 = pull-up, 10 = pull-down). REG0 covers GPIO0-15, so GPIO14 is bits [29:28] and
/// GPIO15 is bits [31:30]. Porting the Pi 2's pull code verbatim would silently do nothing here.
#[cfg(feature = "pi4")]
const GPIO_PUP_PDN_CNTRL_REG0: *mut u32 = (GPIO_BASE + 0xE4) as *mut u32;

/// Mux GPIO14/15 to the PL011 and give the RX pin a pull-up.
///
/// The pull-up is not optional garnish: an undriven UART input floats, and a floating pin beside a
/// switching TX line frames noise into well-formed bytes with no error flags. That cost real time on
/// the Pi 2 (see `docs/arm32-status.md`), where a faulty RX line filled the shared input ring and
/// starved the keyboard. Set it here rather than rediscover it.
#[cfg(feature = "pi4")]
fn gpio_init_uart() {
    // SAFETY: BCM2711 GPIO registers, identity-mapped MMIO with the MMU off. Read-modify-write of
    // GPFSEL1 touches only GPIO14/15's function fields; the pull register is written whole with only
    // those two pins' fields changed.
    unsafe {
        let gpfsel1 = mmio(GPFSEL1 as usize) as *mut u32;
        let mut sel = gpfsel1.read_volatile();
        sel &= !((0b111 << 12) | (0b111 << 15)); // clear GPIO14, GPIO15 function fields
        sel |= (0b100 << 12) | (0b100 << 15);    // ALT0 = TXD0 / RXD0
        gpfsel1.write_volatile(sel);

        let pud_reg = mmio(GPIO_PUP_PDN_CNTRL_REG0 as usize) as *mut u32;
        let mut pud = pud_reg.read_volatile();
        pud &= !(0b11 << 28); // GPIO14 (TX): no pull - we drive it
        pud &= !(0b11 << 30);
        pud |= 0b01 << 30;    // GPIO15 (RX): pull-up, so an absent peer reads as idle, not noise
        pud_reg.write_volatile(pud);
    }
}

/// Bring the PL011 up for output, **preserving whatever baud divisors the firmware programmed**.
///
/// Do not assume the firmware did this - that lesson is already recorded for the 32-bit port, where an
/// uninitialised PL011 silently swallows every write. `enable_uart=1` in `config.txt` is what makes the
/// firmware configure it, and if that is missing (or the console was handed to Bluetooth instead of
/// GPIO14/15) the UART is off and `TXFF` never clears.
///
/// IBRD/FBRD are deliberately NOT touched: the reference clock differs between firmware versions and
/// boards, so recomputing the divisors is how you turn a working console into garbage.
#[cfg(feature = "pi4")]
fn pl011_init() {
    gpio_init_uart();
    // SAFETY: BCM2711 UART0 registers, identity-mapped MMIO, single-threaded boot. Writes in the order
    // the PL011 spec requires (disable, drain, set line control, enable).
    unsafe {
        let cr = mmio(PL011_CR as usize) as *mut u32;
        let fr = mmio(PL011_FR as usize) as *const u32;
        cr.write_volatile(0);
        // Bounded: a present-but-wedged UART must not hang the boot on BUSY (invariant 12).
        let mut t = 0u32;
        while fr.read_volatile() & PL011_FR_BUSY != 0 {
            t += 1;
            if t > 1_000_000 { break; }
        }
        (mmio(PL011_LCRH as usize) as *mut u32).write_volatile(PL011_LCRH_8N1);
        cr.write_volatile(PL011_CR_ON);
    }
}

/// ELF entry point - `qemu-system-aarch64 -M virt -kernel` jumps here (EL1/EL2). Park the secondary
/// cores (WFE loop), set the boot stack, zero BSS, then call the Rust boot. `.text.boot` so the linker
/// script places it first at the load address. First milestone: prove the toolchain + boot + UART.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.boot"]
pub unsafe extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        // --- Save the boot handoff BEFORE touching x0 ----------------------------------------
        // The firmware branches here with the **device tree pointer in x0** (the Linux AArch64 boot
        // protocol, which the Pi's stock firmware follows). The very next instruction clobbers x0, so
        // it is stashed in a callee-saved register first and stored once `.bss` is zeroed - storing it
        // before then would be overwritten by the zeroing loop.
        //
        // The DTB matters because it is the AUTHORITATIVE memory map on this board. The mailbox's
        // legacy GET-ARM-MEMORY tag returns a 32-bit size and, on firmware configured the usual way,
        // describes only the low portion of a >1 GiB board. Keeping the pointer costs one register and
        // leaves the door open; throwing it away would mean re-deriving the map from something worse.
        //
        // x19 survives the `eret` below - exception return does not alter general-purpose registers.
        "mov  x19, x0",
        // --- Drop to EL1 if we entered at EL2 -------------------------------------------------
        // QEMU `virt -kernel` enters at EL1, but the Pi 4's armstub hands the primary core over at
        // **EL2**. At EL2 the EL1 system registers below are writable but do not govern us, and FP/SIMD
        // is gated by CPTR_EL2 rather than CPACR_EL1 - so Rust's NEON memcpy would trap even though
        // CPACR_EL1 says otherwise. Rather than special-case the whole file by exception level, drop to
        // EL1 first and let everything after this assume EL1.
        "mrs  x0, CurrentEL",
        "lsr  x0, x0, #2",
        "cmp  x0, #2",
        "b.ne 4f",                           // already EL1 (QEMU): skip the drop
        "mov  x0, #(1 << 31)",               // HCR_EL2.RW = 1: EL1 is AArch64, not AArch32
        "msr  hcr_el2, x0",
        "mrs  x0, cnthctl_el2",              // let EL1 read the physical counter/timer without trapping
        "orr  x0, x0, #3",                   //   EL1PCTEN | EL1PCEN
        "msr  cnthctl_el2, x0",
        "msr  cntvoff_el2, xzr",             // no virtual-time offset
        "mov  x0, #0x0800",                  // SCTLR_EL1: MMU off, caches off, res1 bits set
        "movk x0, #0x30d0, lsl #16",
        "msr  sctlr_el1, x0",
        "mov  x0, #0x3c5",                   // SPSR_EL2: D|A|I|F masked, M[3:0] = 0b0101 (EL1h)
        "msr  spsr_el2, x0",
        "adr  x0, 4f",                       // return to the next instruction, but at EL1
        "msr  elr_el2, x0",
        "eret",
        "4:",
        // --- From here on we are at EL1 on either board --------------------------------------
        "mov  x0, #(3 << 20)",               // CPACR_EL1.FPEN = 0b11: DON'T trap FP/SIMD at EL1.
        "msr  cpacr_el1, x0",                //   (Rust emits NEON for memcpy/byte-copy; default EL1
        "isb",                               //    traps it -> ESR 0x07 undefined-instruction fault.)
        "mrs  x1, mpidr_el1",                // only the primary core boots
        "and  x1, x1, #0xff",
        "cbnz x1, 2f",
        "adrp x1, __stack_top",              // sp = __stack_top
        "add  x1, x1, :lo12:__stack_top",
        "and  x1, x1, #0xfffffffffffffff0",   // SP must be 16-byte aligned (EL1 SP-align check)
        "mov  sp, x1",
        "adrp x1, __bss_start",              // zero [__bss_start, __bss_end)
        "add  x1, x1, :lo12:__bss_start",
        "adrp x2, __bss_end",
        "add  x2, x2, :lo12:__bss_end",
        "1:",
        "cmp  x1, x2",
        "b.hs 3f",
        "str  xzr, [x1], #8",
        "b    1b",
        "3:",
        "mov  x0, x19",                      // hand the DTB pointer to Rust as the first argument
        "bl   {main}",                       // -> aarch64_boot_main (never returns)
        "2:",
        "wfe",
        "b    2b",
        main = sym aarch64_boot_main,
    )
}

/// Rust side of boot. Milestone 1: write a line to the PL011 and halt. Later this grows into the real
/// init (MMU, EL1 exceptions, GIC, generic timer, PSCI) and finally calls the neutral `kernel_main`.
extern "C" fn aarch64_boot_main(dtb: u64) -> ! {
    // Report the exception level we ended up in, not just that we printed something. On the Pi 4 the
    // armstub hands the primary core over at EL2 and `_start` drops to EL1; on QEMU `virt` we arrive at
    // EL1 already. Either way this must say EL1 - if it says EL2 the drop silently did not happen, and
    // the next thing to touch FP/SIMD would fault for a reason that has nothing to do with the code
    // that faulted.
    // Bring the UART up before trying to use it. On QEMU `virt` the UART needs no init; on the Pi the
    // firmware may or may not have configured it, and assuming is how you get silence with no clue.
    #[cfg(feature = "pi4")]
    pl011_init();

    let el: u64;
    // SAFETY: reading CurrentEL is a side-effect-free system-register read valid at any EL.
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)) };
    let el = (el >> 2) & 0x3;

    put_str(b"\r\nGodspeedOS aarch64: PL011 alive at EL");
    put_byte(b'0' + el as u8);
    put_str(if cfg!(feature = "pi4") {
        b" (Raspberry Pi 4 / BCM2711, PL011 @ 0xFE201000)\r\n" as &[u8]
    } else {
        b" (QEMU virt, PL011 @ 0x09000000)\r\n" as &[u8]
    });
    if el != 1 {
        put_str(b"aarch64: WARN not at EL1 - the EL2 drop did not happen; FP/SIMD will trap\r\n");
    }

    // Ask the firmware about the machine BEFORE the MMU and caches come on. The GPU reads the request
    // buffer straight out of RAM, so a cacheable buffer would need explicit maintenance; asking now
    // makes that a non-question. See `mailbox` for the full reasoning.
    #[cfg(feature = "pi4")]
    let boot_map = {
        let info = mailbox::query();
        mailbox::report(&info);

        // The device tree is the authoritative map; the mailbox tag is the fallback. Both are read
        // here, with caches off, and the result is kept for the allocator.
        put_str(b"aarch64: device tree pointer (x0 at entry) = ");
        put_hex(dtb);
        put_str(b"\r\n");
        let (map, source) = memmap::build(dtb, &info);
        memmap::report(map, source);
        map
    };

    // Turn translation on. The map is identity, so the PC, the stack and the UART all keep the
    // addresses they already had - which is the only reason the line after this can print.
    #[cfg(feature = "pi4")]
    {
        put_str(b"aarch64: enabling MMU (4 KiB granule, 39-bit VA, 2 MiB blocks over the low 4 GiB)\r\n");
        let ttbr = mmu::enable();
        // Read SCTLR_EL1.M back rather than assume the write took. Without this we would still be
        // printing with the MMU off, and the first fault later would be blamed on whatever touched
        // memory next rather than on translation never having been enabled.
        if mmu::is_enabled() {
            put_str(b"aarch64: MMU ON, TTBR0_EL1=");
            put_hex(ttbr);
            put_str(b" - this line is running translated\r\n");
        } else {
            put_str(b"aarch64: WARN MMU did NOT enable - SCTLR_EL1.M still clear\r\n");
        }
    }

    // Prove the high half translates while BOTH maps are live - the only window in which "these two
    // addresses are the same memory" can be checked at all.
    #[cfg(feature = "pi4")]
    match mmu::selftest_high_half() {
        Some((phys, high)) => {
            put_str(b"aarch64: TTBR1 high half LIVE - phys ");
            put_hex(phys);
            put_str(b" reads/writes as ");
            put_hex(high);
            put_str(b"
");
        }
        None => put_str(b"aarch64: WARN high half did NOT translate - check TG1/EPD1/TTBR1_EL1
"),
    }

    // --- Move the kernel into the high half --------------------------------------------------
    // Done here, while BOTH halves translate, and never later: from this point the kernel executes
    // from TTBR1 and TTBR0 belongs to whatever task is running. Everything past this line lives in
    // `boot_high`, because the jump abandons this stack along with every local on it.
    #[cfg(feature = "pi4")]
    // SAFETY: `mmu::enable` installed both TTBR0 (identity) and TTBR1 (high); `boot_high` is a
    // no-mangle extern "C" fn that never returns.
    unsafe { mmu::jump_high(boot_high) };

    put_str(b"aarch64: neutral kernel linked; arch/aarch64 stubs pending real bodies. halting.\r\n");
    loop {
        // SAFETY: WFI is always valid; wait for an interrupt that never comes (halt).
        unsafe { core::arch::asm!("wfi"); }
    }
}

/// Transmit one byte, waiting for FIFO space with a **bounded** spin.
///
/// Every hardware wait in this kernel is bounded (invariant 12): a dead or absent UART must never hang
/// the boot. On a board where the firmware left the UART off, the FIFO never drains - dropping the byte
/// and continuing is right, because the alternative is a machine that appears bricked with no output at
/// all to say why.
pub(crate) fn put_byte(b: u8) {
    let mut spins = 0u32;
    // SAFETY: PL011_FR/PL011_DR are the flag and data registers of the board's UART, identity-mapped
    // MMIO. The MMU is off at this point, so these are physical addresses.
    unsafe {
        while (mmio(PL011_FR as usize) as *const u32).read_volatile() & PL011_FR_TXFF != 0 {
            spins += 1;
            if spins > 1_000_000 { return; } // drop the byte rather than hang forever
        }
        (mmio(PL011_DR as usize) as *mut u8).write_volatile(b);
    }
}

/// Where the kernel reaches a physical address: the high direct map.
///
/// Since the kernel moved to `TTBR1`, a physical address is not a pointer - `TTBR0` belongs to a task.
#[cfg(feature = "pi4")]
#[inline]
fn hhdm(pa: u64) -> u64 { mmu::KERNEL_VA_BASE + pa }

/// Prove the neutral frame allocator actually works on this board, rather than merely returning.
///
/// A count printed by `memory::init` shows the map was parsed; it does not show that a returned frame
/// is real, distinct, writable, or reusable. Each of those is checked:
///
/// 1. **Distinct and aligned** - a batch of frames, no repeats, all 4 KiB aligned and inside RAM the
///    map called usable. A double-hand-out is the failure that corrupts everything downstream.
/// 2. **Backed by real memory** - each frame is written with a value derived from its own address and
///    read back. Identity-mapped, so the physical address IS the pointer. This catches a map that
///    describes RAM the board does not have, which is exactly what a mis-parsed device tree produces.
/// 3. **Reusable** - free them all, and the free count must return to where it started. A leak here
///    would be invisible until the machine ran out much later.
#[cfg(feature = "pi4")]
fn allocator_selftest() {
    use crate::memory::allocator::{alloc_frame, free_frame, free_frame_count};

    const N: usize = 64;
    let before = free_frame_count();
    let mut frames = [0u64; N];
    let mut got = 0usize;

    while got < N {
        match alloc_frame() {
            Some(f) => {
                frames[got] = f.phys_addr().0;
                got += 1;
            }
            None => break,
        }
    }

    let mut bad_align = 0;
    let mut dupes = 0;
    let mut bad_readback = 0;

    for i in 0..got {
        let pa = frames[i];
        if pa & 0xFFF != 0 {
            bad_align += 1;
        }
        if frames[..i].contains(&pa) {
            dupes += 1;
        }
    }

    // Write EVERY frame first, then verify every frame - deliberately two passes rather than
    // write-then-read each in turn. A single pass cannot detect ALIASING: two distinct physical
    // addresses backed by the same RAM would each read back correctly the instant after being
    // written. Separating the passes means an alias overwrites the earlier frame's value and the
    // verify catches it. That matters here specifically, because a memory map claiming RAM the board
    // does not have usually shows up as address wrap or aliasing on real silicon, not as a fault.
    // The value is derived from the frame's own address so no two frames expect the same bytes.
    for i in 0..got {
        // SAFETY: `frames[i]` is a frame the allocator just handed us, reached through the kernel's
        // direct map (a physical address is not a pointer now that TTBR0 belongs to tasks). Nothing
        // else holds these frames.
        unsafe { (hhdm(frames[i]) as *mut u64).write_volatile(frames[i] ^ 0x5A5A_A5A5_DEAD_C0DE) };
    }
    for i in 0..got {
        // SAFETY: as above - the frame is still owned by this function until it is freed below.
        let got_val = unsafe { (hhdm(frames[i]) as *const u64).read_volatile() };
        if got_val != frames[i] ^ 0x5A5A_A5A5_DEAD_C0DE {
            bad_readback += 1;
        }
    }

    for i in 0..got {
        // SAFETY: each frame came from `alloc_frame` above and is freed exactly once.
        unsafe { free_frame(crate::memory::frame::Frame::from_phys(crate::memory::frame::PhysAddr(frames[i]))) };
    }

    let after = free_frame_count();
    let ok = got == N && bad_align == 0 && dupes == 0 && bad_readback == 0 && after == before;

    if ok {
        crate::kprintln!(
            "aarch64: frame allocator OK - {} frames distinct, aligned, read-back verified, all returned ({} free)",
            got, after
        );
    } else {
        crate::kprintln!(
            "aarch64: WARN allocator selftest FAILED - got {}/{}, {} misaligned, {} duplicate, {} bad read-back, free {} -> {}",
            got, N, bad_align, dupes, bad_readback, before, after
        );
    }
}

/// Prove a per-task address space works, including the `TTBR0_EL1` swap that had no exercise until now.
///
/// The checks, in the order they would fail:
///
/// 1. **A private space is built** from the kernel L1, with its own L2 copies.
/// 2. **A page maps at a VA the task owns outright** - above the low 4 GiB the kernel identity-maps,
///    so the walk allocates a fresh L2 and L3 rather than colliding with a kernel block. See `ptables`
///    for why a VA *below* 4 GiB cannot be used yet: it would shadow the kernel's identity mapping.
/// 3. **The switch actually takes**: install the new TTBR0, and read back a value written through the
///    new mapping. This is the first time the address-space branch of `switch_context` is exercised.
/// 4. **The kernel survived the switch** - it is still executing, printing, and its own data reads
///    back correctly under the task's table.
/// 5. **Switching back and reclaiming** returns every table frame, checked against the free count.
#[cfg(feature = "pi4")]
fn page_table_selftest() {
    use crate::memory::allocator::{alloc_frame, free_frame, free_frame_count};

    // ABOVE the kernel's identity map (which covers the low 4 GiB), so this address space genuinely
    // owns the address: no kernel block to collide with, no shadowing. Choosing a VA below 4 GiB is
    // what surfaced the TTBR1 finding recorded in `ptables` - a task page there would displace the
    // kernel's identity mapping of the same physical range.
    const TEST_VA: u64 = 0x1_0000_0000;
    const MAGIC: u64 = 0xC0FF_EE00_1234_5678;

    let before = free_frame_count();

    // A kernel-owned frame, written before the switch and read back through its IDENTITY address while
    // the task's table is installed. That is the real question about a copied kernel map: does kernel
    // memory stay reachable under a task's address space? If the L2 copies were wrong this read comes
    // back garbage, or the machine dies before printing at all.
    let canary_frame = match alloc_frame() {
        Some(f) => f,
        None => { crate::kprintln!("aarch64: WARN page-table selftest could not allocate"); return; }
    };
    let canary_pa = canary_frame.phys_addr().0;
    // SAFETY: a frame the allocator just handed us, reached through the kernel's direct map.
    unsafe { (hhdm(canary_pa) as *mut u64).write_volatile(MAGIC ^ 0xFFFF) };

    let mut pt = match ptables::PageTable::new() {
        Ok(p) => p,
        Err(_) => { crate::kprintln!("aarch64: WARN page-table selftest could not build a space"); return; }
    };

    let data = match alloc_frame() {
        Some(f) => f,
        None => { crate::kprintln!("aarch64: WARN page-table selftest could not allocate"); return; }
    };
    let data_pa = data.phys_addr().0;
    // SAFETY: ours, reached through the direct map.
    unsafe { (hhdm(data_pa) as *mut u64).write_volatile(MAGIC) };

    if pt.map(TEST_VA, data_pa, false, true, false).is_err() {
        crate::kprintln!("aarch64: WARN page-table selftest map failed");
        return;
    }

    let new_ttbr = pt.ttbr();
    let old_ttbr: u64;
    // SAFETY: reading TTBR0_EL1 is side-effect free.
    unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) old_ttbr, options(nomem, nostack)) };

    // --- the switch itself -------------------------------------------------------------------
    // SAFETY: `new_ttbr` maps the kernel (copied from the boot L1), so the instruction stream, the
    // stack and the UART all stay translated across the change. The invalidate is required: writing
    // TTBR0 flushes nothing on AArch64 (SEC-26).
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {t}", "dsb ish", "tlbi vmalle1", "dsb ish", "isb",
            t = in(reg) new_ttbr, options(nostack),
        );
    }

    // Read through the NEW mapping, and re-read the canary through the split block.
    // SAFETY: TEST_VA is mapped by the table now installed; the canary is read through the kernel's
    // high direct map, which every address space shares because it lives in TTBR1.
    let through_va = unsafe { (TEST_VA as *const u64).read_volatile() };
    let canary_back = unsafe { (hhdm(canary_pa) as *const u64).read_volatile() };

    // --- back to the kernel space ------------------------------------------------------------
    // SAFETY: `old_ttbr` is the boot L1 that was live a moment ago.
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {t}", "dsb ish", "tlbi vmalle1", "dsb ish", "isb",
            t = in(reg) old_ttbr, options(nostack),
        );
    }

    let mapped_ok = through_va == MAGIC;
    let canary_ok = canary_back == (MAGIC ^ 0xFFFF);

    // Reclaim: unmap gives the data frame back, dropping the table frees every table frame.
    let unmapped = pt.unmap(TEST_VA).is_ok();
    drop(pt);
    // SAFETY: both frames came from the allocator and are freed exactly once.
    unsafe { free_frame(data); free_frame(canary_frame); }

    let after = free_frame_count();
    if mapped_ok && canary_ok && unmapped && after == before {
        crate::kprintln!(
            "aarch64: per-task page table OK - own space built, TTBR0 swapped and read through, \
             kernel reachable under it, all frames reclaimed ({} free)",
            after
        );
    } else {
        crate::kprintln!(
            "aarch64: WARN page-table selftest FAILED - mapped={} canary={} unmapped={} frames {} -> {}",
            mapped_ok, canary_ok, unmapped, before, after
        );
    }
}


/// The boot, continued from the high half.
///
/// Split from `aarch64_boot_main` because [`mmu::jump_high`] relocates `SP` as well as `PC`: every
/// frame pointer and return address on the old stack names a low address, so the jump cannot return and
/// the remaining work has to live in a function it branches to instead. Nothing here differs in kind
/// from what ran before - only the addresses it runs at.
#[cfg(feature = "pi4")]
#[no_mangle]
extern "C" fn boot_high() -> ! {
    if mmu::running_high() {
        put_str(b"aarch64: kernel RUNNING FROM THE HIGH HALF (TTBR1), PC above ");
        put_hex(mmu::KERNEL_VA_BASE);
        put_str(b"\r\n");
    } else {
        put_str(b"aarch64: WARN jump_high did not take - PC is still low\r\n");
    }

    // Report SP, not just PC. The jump relocates BOTH, and the stack is the half that fails silently:
    // a PC that did not move stops the machine instantly, whereas an SP that did not move keeps
    // working perfectly until the low map is retired and then dies somewhere unrelated. That is not
    // hypothetical - it is exactly what happened here, and this line is what settled it.
    {
        let sp: u64;
        // SAFETY: reading SP is side-effect free.
        unsafe { core::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack)) };
        if sp >= mmu::KERNEL_VA_BASE {
            put_str(b"aarch64: stack relocated too, SP = ");
            put_hex(sp);
            put_str(b"\r\n");
        } else {
            put_str(b"aarch64: WARN SP is STILL LOW after the jump - it will die when TTBR0 is dropped\r\n");
        }
    }

    // Peripherals move first. The kernel's code and stack are already high, but a device register is
    // still named by its physical address, and the UART is the one thing that must keep working across
    // this step or the next failure reports nothing at all.
    mmio_go_high();

    // Retire the low identity map IMMEDIATELY, not eventually. Everything below then runs with TTBR0
    // empty, so any surviving reliance on "a physical address is a pointer" faults HERE, in a boot that
    // is still printing, rather than surviving until the first real task installs its own address space
    // and turns it into a mystery.
    // SAFETY: we are executing from the high half (checked just above), so nothing the kernel needs is
    // reached through TTBR0 any more.
    unsafe { mmu::drop_low_map() };
    put_str(b"aarch64: low identity map RETIRED - TTBR0_EL1 is free for a task\r\n");

    // Vectors AFTER the MMU, because VBAR_EL1 holds a virtual address and installing it before
    // translation is live would point at an address that is about to mean something else. From here on
    // a fault reports itself instead of wandering off into whatever sits at the reset vector.
    #[cfg(feature = "pi4")]
    {
        let vbar = exceptions::init();
        if exceptions::installed() == vbar {
            put_str(b"aarch64: exception vectors installed, VBAR_EL1=");
            put_hex(vbar);
            put_str(b"\r\n");
        } else {
            put_str(b"aarch64: WARN VBAR_EL1 did not take the write\r\n");
        }
        // The deliberate `brk #0` that proved this table lived here. It has served its purpose - the
        // vectors reported it correctly on hardware (ESR EC 0b111100, vector 4) - and it is removed
        // rather than left behind, because it halts the boot and everything past this point needs to
        // run. The synchronous path it exercised is unchanged.
    }

    // --- The neutral frame allocator ---------------------------------------------------------
    //
    // The first arch-neutral kernel code to run on this board. Everything above is `arch/aarch64/`;
    // `crate::memory` is the same code the x86 build has used since v1, reached through the same
    // `BootInfo` the seam defines. If it runs here unmodified, the demarcation claim holds on a
    // second ISA in the place it is easiest to get wrong.
    //
    // AFTER the MMU (the bitmaps are written through the identity map, and a cacheable write of
    // 128 KiB with caches off would be needlessly slow) and AFTER the vectors, so a fault in the
    // allocator reports itself instead of wandering.
    #[cfg(feature = "pi4")]
    let boot_info = {
        let bi = memmap::boot_info(memmap::current_map());
        crate::memory::init(&bi);
        allocator_selftest();
        page_table_selftest();
        bi
    };

    // --- The neutral scheduler, preempting spinning tasks ------------------------------------
    // Placed after the GIC + timer come up (below) via a second gate: the demo needs a live tick, and
    // it never returns, so it must be the last thing the boot does.

    // --- GIC-400 + generic timer -------------------------------------------------------------
    #[cfg(feature = "pi4")]
    {
        gic::init();
        let freq = timer::frequency();
        put_str(b"aarch64: generic timer CNTFRQ_EL0=");
        put_dec(freq);
        put_str(b" Hz (read from the register, never assumed)\r\n");

        const TICK_HZ: u64 = 100; // 10 ms, matching §9.1's quantum
        let interval = timer::start(TICK_HZ);
        TIMER_INTERVAL.store(interval, Ordering::Relaxed);
        put_str(b"aarch64: tick armed at 100 Hz (reload ");
        put_dec(interval);
        put_str(b" counter ticks), PPI 30 enabled at the GIC\r\n");

        // Nothing is delivered until PSTATE.I is cleared, however well the GIC is programmed.
        timer::unmask_irqs();

        // Wait for the tick to prove itself, BOUNDED - a timer that never fires must report that
        // rather than hang the boot looking busy (invariant 12).
        let mut spins: u64 = 0;
        while exceptions::TICKS.load(Ordering::Relaxed) < 10 && spins < 500_000_000 {
            spins += 1;
            core::hint::spin_loop();
        }
        let ticks = exceptions::TICKS.load(Ordering::Relaxed);
        if ticks >= 10 {
            put_str(b"aarch64: timer IRQs DELIVERING - ");
            put_dec(ticks);
            put_str(b" ticks taken through the GIC and the IRQ vector\r\n");
        } else {
            put_str(b"aarch64: WARN no timer IRQ after a bounded wait (ticks=");
            put_dec(ticks);
            put_str(b") - GIC or CNTP configuration is wrong\r\n");
        }
    }

    // --- Context switch ----------------------------------------------------------------------
    // Proven on its own before anything is built on it: a wrong offset or a dropped callee-saved
    // register does not fault, it resumes a task holding someone else's value and the damage surfaces
    // somewhere unrelated later.
    #[cfg(feature = "pi4")]
    {
        if ctxdemo::run() {
            put_str(b"aarch64: context switch OK - callee-saved integer AND d8-d15 state preserved\r\n");
        } else {
            put_str(b"aarch64: WARN context switch LOST register state - see the CORRUPT lines above\r\n");
        }
        put_str(b"aarch64: ticks since boot = ");
        put_dec(exceptions::TICKS.load(Ordering::Relaxed));
        put_str(b" (the timer kept running throughout)\r\n");
    }

    // --- EL0 + the svc syscall path ------------------------------------------------------------
    //
    // RETIRED BY THE TTBR1 SPLIT, deliberately, and worth understanding rather than just skipping.
    //
    // The milestone-6 demo put its EL0 code and stack in a linker-placed `.el0` region and reached them
    // through the kernel's own identity map, granting EL0 access to those 2 MiB blocks. That worked
    // only because kernel and user shared one address space. Now the kernel lives in TTBR1, which EL0
    // cannot reach **at all** - the architecture gives EL0 no access to the high half - so running the
    // demo unchanged takes an instruction abort from the lower EL the instant it `eret`s (observed:
    // `ESR 0x8200000e`, `FAR 0xffffff8000200000`, reported cleanly by the vectors).
    //
    // That is the split working, not breaking. What replaces it is a real EL0 task: its code and stack
    // in frames mapped into its OWN TTBR0 address space, at a low VA it now genuinely owns because the
    // kernel is no longer sitting there. Every piece of that exists - `ptables` builds the space,
    // `switch_context` installs it - so this is the next milestone rather than a gap.
    //
    // The mechanism it proved (EL0 entry, `svc` decode from `ESR_EL1.imm16`, the return value, the
    // clean exit) is unchanged and hardware-verified; only where the code LIVES changes.
    #[cfg(feature = "pi4")]
    put_str(
        b"aarch64: EL0 demo retired by the TTBR1 split - EL0 cannot reach the high half; \
          a real task needs its own TTBR0 space (next milestone)\r\n",
    );

    // --- The neutral scheduler (gated) -------------------------------------------------------
    // LAST, and last for two reasons. `scheduler::run` IS the idle loop and never returns, so anything
    // placed after it is dead code the linker removes - including, when this sat higher up, the entire
    // `.el0` region, which silently turned the image into one that could not exercise milestones 5 and
    // 6 at all. Running it here means a single boot re-verifies everything above and then hands the
    // machine to the scheduler.
    #[cfg(all(feature = "pi4", feature = "pi4-sched-demo"))]
    sched_demo::run(&boot_info);

    put_str(b"aarch64: neutral kernel linked; arch/aarch64 stubs pending real bodies. halting.
");
    loop {
        // SAFETY: WFE is always valid.
        unsafe { core::arch::asm!("wfe") };
    }
}

/// Print a u64 as `0x...` - enough diagnostics to report a register without pulling in formatting.
pub(crate) fn put_hex(v: u64) {
    put_str(b"0x");
    let mut started = false;
    for shift in (0..16).rev() {
        let nib = ((v >> (shift * 4)) & 0xF) as u8;
        if nib != 0 { started = true; }
        if started || shift == 0 {
            put_byte(if nib < 10 { b'0' + nib } else { b'a' + nib - 10 });
        }
    }
}

/// Print a u64 in decimal. Frequencies and tick counts read as nonsense in hex, and a reader should
/// not have to convert to check whether 54 MHz is what the board reports.
pub(crate) fn put_dec(v: u64) {
    if v == 0 {
        put_byte(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut n = v;
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    put_str(&buf[i..]);
}

pub(crate) fn put_str(s: &[u8]) {
    for &b in s {
        put_byte(b);
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

/// USB mass-storage block device (the ARM DWC2 Bulk-Only bridge). Only the Pi's ARM port has an
/// in-kernel USB stack; elsewhere disks are userspace drivers, so there is no device here.
pub fn usb_disk_sectors() -> u64 { 0 }
pub fn usb_disk_read(_lba: u64, _dst: &mut [u8]) -> bool { false }
pub fn usb_disk_write(_lba: u64, _src: &[u8]) -> bool { false }
pub fn usb_disk_flush() -> bool { false }
/// Counter ticks a core may make NO forward progress before the liveness watchdog panics.
///
/// Ten seconds of `CNTPCT_EL0`, from the frequency the hardware reports in `CNTFRQ_EL0` rather than a
/// constant - the Pi 4 runs the generic timer at 54 MHz where QEMU says 62.5 MHz, so a hardcoded rate
/// would make the margin wrong on one of them.
///
/// **This is answered rather than left at `0` deliberately.** `0` disables the check, and the 32-bit
/// ARM port spent its entire bring-up silently undefended that way - the watchdog was inert because it
/// keyed off an unrelated stubbed constant, so a real wedge produced no diagnostic at all. A safety net
/// that is absent should at least be absent loudly (§26.4); here it is simply present.
#[cfg(feature = "pi4")]
pub fn liveness_deadline_cycles() -> u64 { timer::frequency().saturating_mul(10) }
#[cfg(not(feature = "pi4"))]
pub fn liveness_deadline_cycles() -> u64 { 0 }

/// Hook called when the scheduler commits a **user** task. x86 ignores it; ARM records the slot so the
/// timer runs its syscalls atomically. Nothing to do here yet.
pub fn note_user_task(_slot: usize) {}

// --- Framebuffer console backend (`crate::fbcon`) ---
//
// The neutral console owes each arch two things (see `crate::fbcon`'s module header). Until this port
// maps the Pi's framebuffer there is nothing to publish, so `fb_commit` is a no-op and the console
// simply never initialises - `fbcon::ready()` stays false and every entry point no-ops.

/// No framebuffer is mapped yet, so the scroll strategy is moot. `false` is the conservative choice:
/// it selects the repaint-from-shadow-grid path, which never reads the framebuffer back.
pub const FB_READBACK_CHEAP: bool = false;

/// Publish a written rectangle. Nothing to do until a framebuffer exists; when one does, this becomes
/// either a cache clean (if it is mapped cacheable, as on the Pi 2) or a barrier.
pub fn fb_commit(
    _base: usize, _pitch: usize, _bpp: usize,
    _x: usize, _y: usize, _w: usize, _h: usize,
) {}

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

/// The ELF `e_machine` and `EI_CLASS` this arch's service binaries carry (AArch64, ELFCLASS64).
/// The neutral loader checks a candidate ELF against these, so it can parse a 32-bit ARM
/// service ELF or a 64-bit one without any arch-specific code in the loader itself.
pub const ELF_MACHINE: u16 = 183;
pub const ELF_CLASS: u8 = 2; // 1 = ELFCLASS32, 2 = ELFCLASS64

pub fn halt_all_cores() -> ! { loop { core::hint::spin_loop(); } }
pub fn hardware_reset() -> ! { loop { core::hint::spin_loop(); } }

// ---- Serial / console ----
// These go through `put_byte`, which POLLS the TX FIFO before writing. They previously wrote straight
// to the data register: correct-looking, and fine while nothing called them (this file was a boundary
// stub). The moment neutral kernel code logs through `kprintln!` that becomes byte loss the instant
// the 32-entry FIFO fills, presenting as garbled or truncated kernel output rather than as a UART
// problem. `put_byte`'s wait is bounded, so a wedged UART still cannot hang the log path.
//
// `\n` is expanded to `\r\n`: the neutral log emits bare LF, and a serial terminal needs the CR or
// every line starts where the last one ended.
pub fn serial_write_byte(b: u8) {
    if b == b'\n' { put_byte(b'\r'); }
    put_byte(b);
}
pub fn serial_write_bytes_lockfree(s: &[u8]) { for &b in s { serial_write_byte(b); } }
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
pub mod page_tables {
    use crate::memory::frame::{Frame, PhysAddr};

    pub const PAGE_SIZE: usize = 4096;

    /// Arch hook run once a service's address space is built. x86 needs nothing; ARM clones the kernel
    /// identity mapping into it. No address spaces exist on this port yet.
    ///
    /// # Safety
    /// `_root` must be a page-table root this task owns.
    pub unsafe fn finalize_service_address_space(_root: u64) {}

    /// Free a task's page-table root and the structure below it, at task death.
    ///
    /// # Safety
    /// `_root` must belong to a task already marked Dead, after a TLB shootdown, so no page-walker can
    /// still reach it.
    pub unsafe fn free_page_table_root(_root: u64) {}

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

    /// A physical address is NOT directly addressable on the Pi 4 any more.
    ///
    /// It was, while the kernel ran identity-mapped from `TTBR0`. Now the kernel lives in the high half
    /// and `TTBR0` belongs to a task, so physical memory is reached only through the direct map at
    /// `KERNEL_VA_BASE` - the same arrangement as x86's Limine HHDM, and `false` for the same reason.
    #[cfg(feature = "pi4")]
    pub const PHYS_IS_IDENTITY: bool = false;
    #[cfg(not(feature = "pi4"))]
    pub const PHYS_IS_IDENTITY: bool = true;

    /// No bootloader placed page tables for this port - the kernel builds its own, in `.bss` inside
    /// the kernel image, which the memory map already excludes from usable RAM. So there is nothing
    /// for `protect_kernel_page_table_frames` to protect, and its x86-format PML4 walk must not run.
    ///
    /// This constant exists because that walk was previously gated on `hhdm == 0`, which is a
    /// different claim that merely happened to hold here. Giving this port a real direct-map offset
    /// turned the walk loose on the ARM tables and it took a data abort on hardware.
    pub const BOOTLOADER_PLACED_TABLES: bool = false;
    /// The direct-map base the neutral allocator was handed. Stored rather than assumed so a caller
    /// that forgot `set_hhdm_offset` reads zero and fails loudly instead of quietly translating wrong.
    static HHDM: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    pub fn get_hhdm_offset() -> u64 { HHDM.load(core::sync::atomic::Ordering::Acquire) }
    /// # Safety
    /// Called once by `memory::init` before any physical address is dereferenced through the map.
    pub unsafe fn set_hhdm_offset(offset: u64) {
        HHDM.store(offset, core::sync::atomic::Ordering::Release);
    }
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
    /// The free-running physical counter. Monotonic, never reset, shared by all cores - which is what
    /// the neutral liveness watchdog needs to compare a timestamp taken on one core against a deadline
    /// checked on another.
    #[cfg(feature = "pi4")]
    pub fn read_cycle_counter() -> u64 {
        let v: u64;
        // SAFETY: CNTPCT_EL0 is readable at EL1 (EL2 granted EL1 access during the boot drop via
        // CNTHCTL_EL2). `isb` first because the counter read is otherwise free to be speculated
        // earlier, which would make a duration measured across it come out short.
        unsafe { core::arch::asm!("isb", "mrs {}, cntpct_el0", out(reg) v, options(nomem, nostack)) };
        v
    }
    #[cfg(not(feature = "pi4"))]
    pub fn read_cycle_counter() -> u64 { 0 }                 // CNTPCT_EL0
}

// ---------------------------------------------------------------------------
pub mod interrupts {
    pub const XHCI_MSI_VECTOR: u8 = 0x28;
    pub const EHCI_MSI_VECTOR: u8 = 0x29;
    /// `DAIF.I` is the IRQ mask. It is a MASK, so **clearing** it enables interrupts and setting it
    /// disables them - the opposite polarity to x86's `IF`, and an easy place to write a plausible
    /// inversion that deadlocks the machine at the first critical section.
    #[cfg(feature = "pi4")]
    pub fn enable_interrupts() {
        // SAFETY: unmasking IRQs at EL1 is architecturally always valid.
        unsafe { core::arch::asm!("msr daifclr, #2", options(nomem, nostack)) };
    }
    #[cfg(feature = "pi4")]
    pub fn disable_interrupts() {
        // SAFETY: masking IRQs at EL1 is architecturally always valid.
        unsafe { core::arch::asm!("msr daifset, #2", options(nomem, nostack)) };
    }
    /// Returns whether IRQs were ENABLED, matching the neutral `local_irq_restore(was_enabled)`
    /// contract. Note the inversion: `DAIF.I` set means masked, so "enabled" is the bit being clear.
    #[cfg(feature = "pi4")]
    pub fn local_irq_save() -> bool {
        let daif: u64;
        // SAFETY: reading DAIF is a side-effect-free system-register read.
        unsafe { core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack)) };
        disable_interrupts();
        daif & (1 << 7) == 0 // I bit clear == IRQs were enabled
    }
    #[cfg(feature = "pi4")]
    pub fn local_irq_restore(was_enabled: bool) {
        if was_enabled { enable_interrupts(); }
    }
    #[cfg(feature = "pi4")]
    pub fn wait_for_interrupt() {
        // SAFETY: WFI at EL1 is always valid. It returns on any pending interrupt (or spuriously),
        // so every caller must re-check its condition rather than assume a wake means progress.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
    /// The idle loop may `wfi`: the generic timer keeps ticking through it, so a halted core is woken
    /// by its own 100 Hz tick even if nothing else ever targets it.
    #[cfg(feature = "pi4")]
    pub fn idle_can_halt() -> bool { true }

    #[cfg(not(feature = "pi4"))]
    pub fn enable_interrupts() {}                            // msr daifclr
    #[cfg(not(feature = "pi4"))]
    pub fn disable_interrupts() {}                           // msr daifset
    #[cfg(not(feature = "pi4"))]
    pub fn local_irq_save() -> bool { false }                // mrs DAIF
    #[cfg(not(feature = "pi4"))]
    pub fn local_irq_restore(was_enabled: bool) {}
    #[cfg(not(feature = "pi4"))]
    pub fn wait_for_interrupt() {}                           // wfi
    #[cfg(not(feature = "pi4"))]
    pub fn idle_can_halt() -> bool { false }
    pub fn send_eoi() {}                                     // GIC EOIR
    pub fn fire_test_irq(irq: u8) {}
}

// ---------------------------------------------------------------------------
/// On the Pi 4 the neutral scheduler's context switch IS the real, hardware-proven one in `context`
/// (milestone 5: callee-saved integers, `d8`-`d15`, SP, plus the `TTBR0_EL1` swap SEC-26 requires).
/// The stub below remains for the QEMU `virt` boundary-demarcation build, which only has to compile.
#[cfg(feature = "pi4")]
pub use context as context_switch;

#[cfg(not(feature = "pi4"))]
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
