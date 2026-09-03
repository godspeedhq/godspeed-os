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
pub mod bootcon;
/// The one primitive the kernel's boot/panic console floor (`crate::bootcon`) calls through
/// `arch::imp`: publish a written rectangle. See `crate::bootcon`'s module header for its contract.
pub use bootcon::fb_commit;
// arm32 slice 5: the in-kernel HID decoder is gone with the rest of the USB stack. Keystroke decoding
// belongs to whoever owns the keyboard, and that is the `dwc2` SERVICE now (services/dwc2/src/hid.rs).

/// USB-net bridge backends, kept as STUBS that say no.
///
/// The syscalls above them (42-44) are shared with aarch64, whose GENET driver still uses them, so the
/// syscalls stay and only ARM's implementation leaves. On this port the kernel no longer has a network
/// device: `nic-driver` talks to the `dwc2` service over IPC instead (slice 4b). Answering "no device"
/// is the truth, and a caller that ignores the answer fails loudly rather than reading stale bytes.
pub fn net_frame_tx(_frame: &[u8]) -> bool { false }
pub fn net_frame_rx(_dst: &mut [u8]) -> usize { 0 }
pub fn net_info() -> Option<([u8; 6], bool)> { None }
pub mod usermode;
pub mod loadtest;
pub mod spawn;
pub mod sched_demo;
pub mod sched_ipc;
pub mod sched_supervisor;

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
const PL011_ECR:       *mut u32 = (PL011_BASE + 0x04) as *mut u32;     // +0x04 error clear
const PL011_FR_TXFF:   u32 = 1 << 5;                                   // transmit FIFO full

// ---- Console TX ring ------------------------------------------------------------------------------
//
// Writing to the UART inside a syscall is what stalls a core for ~9 ms per log line, and this port
// cannot preempt that syscall (preempting SVC corrupts the banked SPSR/sp). So writers APPEND here and
// return; the timer tick drains whatever the FIFO will take without waiting.
//
// 8 KiB, fixed: about 0.7 s of output at 115200, which covers any realistic burst, and a hard ceiling
// readable straight off the source (§26.6.1). No allocation, no growth.
const TX_RING_LEN: usize = 8192;
static mut TX_RING: [u8; TX_RING_LEN] = [0; TX_RING_LEN];
/// Producer index (bytes ever queued) and consumer index (bytes ever sent). Both free-running; the
/// occupancy is their difference, so neither needs clamping and the wrap is arithmetic, not a branch.
static TX_HEAD: AtomicU32 = AtomicU32::new(0);
static TX_TAIL: AtomicU32 = AtomicU32::new(0);

/// Until the tick is running there is nobody to drain the ring, so writes must go out synchronously.
/// Boot output therefore behaves exactly as it always has, which also keeps a panic before the first
/// tick readable.
static TX_RING_LIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Start using the ring. Called once the timer tick is running and can drain it.
pub fn tx_ring_enable() {
    TX_RING_LIVE.store(true, Ordering::Release);
}

/// Queue one byte. False if the ring is full (the caller then blocks, so output is never lost
/// silently - a dropped byte is counted and reported instead).
/// How many drain-and-retry rounds a byte gets before it is discarded. Each round moves at most one
/// FIFO-load (16 bytes) and never waits on the UART, so this is bounded work, not a spin: at 115200 a
/// full 16-byte FIFO clears in ~1.4 ms and the tick keeps draining regardless. Four rounds is enough
/// to ride out a burst without ever becoming a wait.
const TX_MAKE_ROOM_TRIES: u32 = 4;

/// Two producers were inside `tx_push` at once. COUNTED, because the ring's correctness depends on
/// there being exactly one and that was previously only asserted in a comment.
static TX_PRODUCER_COLLISIONS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static TX_PRODUCER_ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Bytes discarded because the byte ring stayed full. Genuinely dropped now - the old code wrote them
/// straight to the FIFO instead, which is worse (see below).
static TX_DISCARDED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Reserve the next slot and store one byte. **Single producer.**
///
/// `head` is read, used, then stored back. That is a read-modify-write with no atomicity, so two cores
/// inside this function both read the same `head`, both write the same slot, and both store `head + 1`:
/// one byte overwrites the other and the stream is corrupted at an arbitrary character position -
/// `kernel: supervisor died` arriving as `kernel: supervisor dieds`, or `min 2 us` as `min 2 u` with
/// somebody else's line spliced in after the `u`.
///
/// The comment that used to sit here said the single-producer property was "serialised by the same
/// best-effort serial flag that already orders every write to this UART". That flag is BOUNDED and a
/// writer that times out proceeds anyway - so it never provided the property this code depends on, and
/// the claim was the bug rather than the justification. It is true now for a different reason: at
/// runtime the ring drainer is the only caller, and before SMP there is only one core.
///
/// `TX_PRODUCER_COLLISIONS` measures that rather than trusting it. It is reported with the other loss
/// counters, so "the assumption holds" is a zero in the log instead of a sentence in a comment.
fn tx_push(b: u8) -> bool {
    let reentered = TX_PRODUCER_ACTIVE.swap(true, Ordering::Acquire);
    if reentered { TX_PRODUCER_COLLISIONS.fetch_add(1, Ordering::Relaxed); }

    let head = TX_HEAD.load(Ordering::Relaxed);
    let tail = TX_TAIL.load(Ordering::Acquire);
    let ok = if head.wrapping_sub(tail) as usize >= TX_RING_LEN {
        false
    } else {
        // SAFETY: single producer (measured by the collision counter above). The index is masked into
        // the array, so the write is always in bounds.
        unsafe {
            let p = core::ptr::addr_of_mut!(TX_RING) as *mut u8;
            p.add((head as usize) & (TX_RING_LEN - 1)).write_volatile(b);
        }
        TX_HEAD.store(head.wrapping_add(1), Ordering::Release);
        true
    };

    if !reentered { TX_PRODUCER_ACTIVE.store(false, Ordering::Release); }
    ok
}

/// Push as many queued bytes into the TX FIFO as it will take WITHOUT WAITING. Called from the timer
/// tick; returns having done a bounded amount of work, never blocking on the UART.
pub fn tx_ring_drain() {
    // SAFETY: PL011 flag and data registers, Device-mapped. Reads/writes are volatile MMIO, and the
    // loop stops the moment the FIFO reports full - so this can never spin on a wedged UART.
    unsafe {
        let mut n = 0u32;
        loop {
            let head = TX_HEAD.load(Ordering::Acquire);
            let tail = TX_TAIL.load(Ordering::Relaxed);
            if head == tail {
                return;                        // nothing queued
            }
            if PL011_FR.read_volatile() & PL011_FR_TXFF != 0 {
                return;                        // FIFO full - leave the rest for the next tick
            }
            let p = core::ptr::addr_of!(TX_RING) as *const u8;
            let b = p.add((tail as usize) & (TX_RING_LEN - 1)).read_volatile();
            PL011_DR.write_volatile(b as u32);
            TX_TAIL.store(tail.wrapping_add(1), Ordering::Release);
            // A bound on work per tick, so a service logging in a tight loop cannot turn the tick
            // itself into the stall this exists to remove.
            // The bound is on WORK PER CALL, not throughput, so it has to sit above what the
            // hardware can absorb in that time or it becomes the throttle. The FIFO is 16 bytes and
            // drains at ~11.5 KB/s, so a few hundred is comfortably past any real burst while still
            // being a hard stop against a service logging in a tight loop.
            n += 1;
            if n >= 512 {
                return;
            }
        }
    }
}

/// Drain everything, BLOCKING. For the panic path only: a panic message must reach the wire even
/// though no tick will ever run again.
pub fn tx_ring_flush_blocking() {
    // SAFETY: as above, plus the bounded TXFF poll `pl011_write_byte` already uses.
    unsafe {
        while TX_HEAD.load(Ordering::Acquire) != TX_TAIL.load(Ordering::Relaxed) {
            let tail = TX_TAIL.load(Ordering::Relaxed);
            let mut t: u32 = 0;
            while PL011_FR.read_volatile() & PL011_FR_TXFF != 0 {
                t += 1;
                if t > 1_000_000 { return; }   // wedged UART: give up rather than hang the panic
            }
            let p = core::ptr::addr_of!(TX_RING) as *const u8;
            PL011_DR.write_volatile(p.add((tail as usize) & (TX_RING_LEN - 1)).read_volatile() as u32);
            TX_TAIL.store(tail.wrapping_add(1), Ordering::Release);
        }
    }
}

/// Bytes lost to a full ring, for reporting.
///
/// Reads `TX_DISCARDED`, the counter the ring-full path actually increments. It used to read
/// `TX_DROPPED`, which nothing incremented after the out-of-order fallback was removed - a statistic
/// that would have read a confident zero forever while bytes were genuinely being lost. A counter
/// nothing feeds is worse than no counter, because it looks like evidence.
pub fn tx_dropped() -> u32 {
    TX_DISCARDED.load(Ordering::Relaxed)
}
const PL011_FR_BUSY:   u32 = 1 << 3;                                   // transmitting
/// Error flags the PL011 returns **in the data register itself**, alongside the byte: framing (8),
/// parity (9), break (10), overrun (11). A byte arriving with any of these is line noise, not data.
const PL011_DR_ERR:    u32 = 0xF00;
const PL011_LCRH_8N1:  u32 = (3 << 5) | (1 << 4);                      // WLEN=8 bits, FIFOs on
const PL011_CR_ON:     u32 = (1 << 0) | (1 << 8) | (1 << 9);           // UARTEN | TXE | RXE

// GPIO controller (BCM2836) sits at +0x200000. UART0 uses GPIO14 (TXD0) + GPIO15 (RXD0), both ALT0.
const GPIO_BASE:  usize = PERIPHERAL_BASE + 0x20_0000;
const GPFSEL1:    *mut u32 = (GPIO_BASE + 0x04) as *mut u32; // function select for GPIO10-19
const GPPUD:      *mut u32 = (GPIO_BASE + 0x94) as *mut u32; // pull up/down enable
const GPPUDCLK0:  *mut u32 = (GPIO_BASE + 0x98) as *mut u32; // pull up/down clock (GPIO0-31)
const GPFSEL4:    *mut u32 = (GPIO_BASE + 0x10) as *mut u32; // function select for GPIO40-49
const GPFSEL5:    *mut u32 = (GPIO_BASE + 0x14) as *mut u32; // function select for GPIO50-53
const GPPUDCLK1:  *mut u32 = (GPIO_BASE + 0x9C) as *mut u32; // pull up/down clock (GPIO32-53)

/// Route the SD-card pins (GPIO 48-53) to the **Arasan EMMC** controller and report what the firmware
/// had them set to.
///
/// The BCM283x wires the card slot to EITHER controller, selected by the pins' alternate function:
/// **ALT0 (fsel 4) = the Broadcom `sdhost` block**, **ALT3 (fsel 7) = the Arasan EMMC** (which is the
/// controller `block-driver` drives, and the one bare-metal projects use - a Pi 2 has no SDIO WiFi to
/// need the other). If the firmware left them on sdhost, the Arasan is electrically disconnected from
/// the card: its registers answer perfectly while no command ever completes.
///
/// The read-back is logged BEFORE the write, because it is the one fact that distinguishes "the card was
/// muxed away from us" from "the card is ours and something else is wrong" - worth a line of boot log
/// forever. This is board-level pin muxing, so it belongs here and not in the driver, which is granted
/// only its own controller's registers (§12.3).
fn sd_route_to_emmc() {
    // SAFETY: GPFSEL4/5 and the pull registers are the BCM2835 GPIO block, inside the Device-mapped
    // peripheral window; volatile reads/writes of ordinary MMIO on the single-threaded boot path.
    unsafe {
        let (f4, f5) = (GPFSEL4.read_volatile(), GPFSEL5.read_volatile());
        // Pins 48,49 are fields 8,9 of GPFSEL4; pins 50-53 are fields 0..3 of GPFSEL5.
        let mut fsel = [0u32; 6];
        fsel[0] = (f4 >> 24) & 7; // 48
        fsel[1] = (f4 >> 27) & 7; // 49
        for i in 0..4 { fsel[2 + i] = (f5 >> (i * 3)) & 7; } // 50..53
        pl011_write(b"arm32: SD pins 48-53 fsel=");
        for v in fsel.iter() { pl011_write(&[b'0' + (*v as u8 & 7)]); }
        pl011_write(if fsel.iter().all(|v| *v == 7) {
            b" (ALT3 = Arasan EMMC, already ours)\r\n" as &[u8]
        } else if fsel.iter().all(|v| *v == 4) {
            b" (ALT0 = sdhost - the card was muxed AWAY from the EMMC; routing it back)\r\n"
        } else {
            b" (mixed/unexpected - routing to ALT3 = Arasan EMMC)\r\n"
        });

        // ALT3 (7) for 48,49 in GPFSEL4 and 50-53 in GPFSEL5.
        let mut r4 = GPFSEL4.read_volatile();
        r4 = (r4 & !((7 << 24) | (7 << 27))) | (7 << 24) | (7 << 27);
        GPFSEL4.write_volatile(r4);
        let mut r5 = GPFSEL5.read_volatile();
        for i in 0..4 { r5 = (r5 & !(7 << (i * 3))) | (7 << (i * 3)); }
        GPFSEL5.write_volatile(r5);

        // Pull-ups on CLK/CMD (48,49) and DAT0-3 (50-53). GPPUDCLK1 bit N = GPIO 32+N, so 48-53 are
        // bits 16-21. The BCM2835 pull sequence is: set GPPUD, wait, strobe the clock, wait, clear both.
        GPPUD.write_volatile(2); // 2 = pull-up
        for _ in 0..150 { core::arch::asm!("nop", options(nomem, nostack)); }
        GPPUDCLK1.write_volatile((1 << 16) | (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 21));
        for _ in 0..150 { core::arch::asm!("nop", options(nomem, nostack)); }
        GPPUD.write_volatile(0);
        GPPUDCLK1.write_volatile(0);
    }
}

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
        // PULL-UP on GPIO15 (RX). This used to disable the pull on both pins, reasoning that "they are
        // externally driven" - true of GPIO14 (TX), which WE drive, and false of GPIO15, which is an
        // INPUT and is only driven when an adapter is actually connected, powered, and transmitting.
        //
        // An undriven input floats, and a floating pin beside switching signals picks up noise. Each
        // glitch looks to the UART like a start bit; the line then reads high for the rest of the frame
        // and delivers a perfectly well-framed 0xFF - no framing, parity or break error, so nothing
        // downstream can tell it from a real byte. Measured on a Pi 2 with a GPIO HAT fitted: ~115
        // spurious 0xFF per second, error-free, which saturated the 256-byte input ring and made every
        // full-screen app repaint continuously on phantom keystrokes.
        //
        // A pull-up gives the pin the UART's own idle level (high) whenever nothing drives it, so an
        // absent or passive peer reads as silence instead of noise. This is what a UART RX pin should
        // always have had; the firmware sets it when it configures the UART itself, and we were undoing
        // that. GPIO14 keeps no pull - we drive it, so a pull would only fight the driver.
        //
        // BCM2835 pull sequence: write GPPUD, wait, strobe the pin clock, wait, clear both.
        let spin = |n: u32| { for _ in 0..n { core::arch::asm!("nop", options(nomem, nostack)); } };
        GPPUD.write_volatile(2); // 2 = pull-up
        spin(150);
        GPPUDCLK0.write_volatile(1 << 15); // RX only
        spin(150);
        GPPUD.write_volatile(0);
        GPPUDCLK0.write_volatile(0);
        // Then explicitly clear any pull on TX, so the state is set rather than inherited.
        GPPUD.write_volatile(0); // 0 = no pull
        spin(150);
        GPPUDCLK0.write_volatile(1 << 14);
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
        // Bounded: a present-but-wedged UART must not hang the boot on the BUSY bit (invariant 12;
        // kernel-audit Audit 6 - same class as the x86 THRE-poll K1 fix). Best-effort proceed on timeout.
        let mut t = 0u32;
        while PL011_FR.read_volatile() & PL011_FR_BUSY != 0 { t += 1; if t > 1_000_000 { break; } }
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
    hires_timer_selftest();
    ipi_selftest();
    // The shared sentence, so this port and every other say it identically (`smp::core`).
    crate::smp::core::report_cores_ready();
}

/// Does the microsecond one-shot fire AT ALL, and does its interrupt reach us?
///
/// Two different failures wear the same symptom - a sleep that returns late - and they need
/// different fixes, so they are separated here rather than guessed at afterwards:
///   - the COMPARE never matches: the timer hardware is not doing its job (or the emulator does not
///     model it), and no amount of interrupt plumbing will help.
///   - the compare matches but no INTERRUPT arrives: the hardware is fine and the routing is wrong,
///     which is ours to fix.
///
/// So: arm it, poll the match bit directly, and report which of the two happened.
fn hires_timer_selftest() {
    const WANT_US: u32 = 1000;
    timer::arm_oneshot_us(WANT_US);
    let t0 = timer::systimer_lo();
    let mut matched = false;
    // Bounded by the COUNTER, not by a spin count - a spin count means a different duration on every
    // machine, which is exactly the mistake this timer exists to stop making.
    while timer::systimer_lo().wrapping_sub(t0) < WANT_US * 20 {
        if timer::take_oneshot_match() {
            matched = true;
            break;
        }
    }
    let elapsed = timer::systimer_lo().wrapping_sub(t0);
    if matched {
        crate::kprintln!(
            "arm32: hi-res timer selftest PASS - compare 3 matched after {} us (asked {})",
            elapsed, WANT_US);
    } else {
        crate::kprintln!(
            "arm32: hi-res timer selftest FAIL - compare 3 never matched in {} us; sub-tick sleeps              will fall back to the 10 ms tick (correct, just coarse)",
            elapsed);
    }
}

/// Prove the cross-core doorbell actually reaches the other core.
///
/// `send_ipi_to_lapic` was an empty stub on this port, and nothing noticed for the port's entire
/// life, because every service that talks to another service was pinned to core 0. A wake that goes
/// nowhere is invisible until something depends on it, and then it presents as sluggishness rather
/// than as a missing feature - which is how it was eventually found: an operator reporting that `ls`
/// felt slow after services were spread across cores.
///
/// So this rings each AP and waits for that core's OWN handler to count it. It exercises the whole
/// path - write-set, the target's IRQ, its dispatch, the write-high-to-clear - on the machine
/// actually running, and reports either way.
fn ipi_selftest() {
    let (mut tested, mut ok) = (0u32, 0u32);
    for core in 1..4u32 {
        if !crate::smp::core::is_ready(core) {
            continue;
        }
        tested += 1;
        let before = irq::doorbells_received(core);
        // SAFETY: ringing a ready core's mailbox. The write-set register is Device-mapped and the
        // target's handler clears it; idempotent, since a doorbell already pending stays pending.
        unsafe { boot::send_ipi_to_lapic(core, 0) };
        // Bounded wait. A doorbell is an interrupt, so it lands in microseconds on a healthy core:
        // generous enough that a slow core is not called broken, short enough that three dead cores
        // cannot add a visible pause to boot.
        let mut landed = false;
        for _ in 0..2_000_000u32 {
            if irq::doorbells_received(core) != before {
                landed = true;
                break;
            }
            core::hint::spin_loop();
        }
        if landed {
            ok += 1;
        }
    }
    if tested == 0 {
        crate::kprintln!("arm32: IPI selftest SKIPPED - no APs came up (nothing to wake across cores)");
    } else if ok == tested {
        crate::kprintln!(
            "arm32: IPI selftest PASS ({}/{} cores took a doorbell - cross-core wakes are immediate, not tick-delayed)",
            ok, tested);
    } else {
        crate::kprintln!(
            "arm32: IPI selftest FAIL - only {}/{} cores took a doorbell; cross-core IPC waits for a 10 ms tick",
            ok, tested);
    }
}

/// Write one byte to the PL011, waiting for room in the transmit FIFO.
///
/// The firmware has already configured the UART (115200 8N1 - the same line Linux uses), so no
/// baud/line setup is needed for this milestone. We poll TXFF rather than writing blind, or a burst
/// longer than the 16-byte FIFO would silently drop characters.
pub(super) fn pl011_write_byte(b: u8) {
    // QUEUE IT, once there is a tick to drain it. This is the change that stops a log line holding
    // its core: the writer returns immediately instead of waiting ~87 us per byte for the FIFO.
    //
    // A full ring falls through to the blocking path below rather than dropping the line. Output that
    // vanishes silently is worse than output that is slow, and the counter says when it happens.
    if TX_RING_LIVE.load(Ordering::Acquire) {
        if tx_push(b) {
            // MOVE BYTES NOW, don't wait for the tick.
            //
            // Draining only from the timer tick throttled the console to about 1.6 KB/s: the drain
            // stops the moment the TX FIFO is full, the FIFO is 16 bytes, and the tick is 100 Hz -
            // so one FIFO-load per tick, against a line that carries 11.5 KB/s. Under storm-volume
            // logging the ring filled, every writer fell back to the blocking path, and the machine
            // went silent for ~15 s while the backlog trickled out.
            //
            // This still never waits: it pushes only while the FIFO reports room and returns the
            // instant it does not.
            tx_ring_drain();
            return;
        }
        // RING FULL. Make room by draining, then retry - and if it is STILL full, DISCARD the byte.
        //
        // What this replaces was the last stream-corrupting path on this UART, and it needed no second
        // core to do it: the old code wrote the byte straight to PL011_DR while other bytes were still
        // queued in the ring, so that byte JUMPED AHEAD of everything waiting and landed in the middle
        // of whatever the tick drained next. A message could therefore interleave with ITSELF. Which is
        // why `kernel: supervisor died` - one kprintln, one write, one producer - still came out as
        // `kernel: supervisor dieds` after every writer above this layer had been made line-atomic.
        //
        // It is the same rule as `serial_emit`, one layer down: a writer that cannot get into the ring
        // does not write. Order is the property the whole path exists to preserve, and a byte written
        // out of order destroys a line just as thoroughly as a second writer does - it just does it
        // without needing one. A discarded byte is counted and reported; an out-of-order byte is
        // silent corruption (§26.7).
        for _ in 0..TX_MAKE_ROOM_TRIES {
            tx_ring_drain();
            if tx_push(b) { return; }
        }
        TX_DISCARDED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: PL011_FR/PL011_DR are the BCM2836 UART0 flag and data registers, identity-mapped with
    // the MMU off. Volatile MMIO: poll until the TX FIFO has room, then write one byte to transmit.
    unsafe {
        // Bounded: a wedged/absent UART with a permanently-full TX FIFO must never hang the console
        // output path (invariant 12 / §26.6). Matches the bounded BUSY poll in `pl011_init` (Audit 6);
        // this one was missed. On a healthy UART the FIFO drains far faster than the cap.
        let mut t: u32 = 0;
        while PL011_FR.read_volatile() & PL011_FR_TXFF != 0 {
            t += 1;
            if t > 1_000_000 { break; } // drop the byte rather than hang forever
        }
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

/// Cap on the acquire wait, in MICROSECONDS of real time. A waiter that exceeds it writes anyway rather
/// than block forever - the only correct choice for a UART also used by the fault handler.
///
/// This must outlast a full line, or a waiter gives up mid-line and its bytes land INSIDE the holder's
/// output: `fs: block-driver did not report capacity ... (dhelli read; (typO 'hel ')` is a real capture of
/// the shell's `(F1=help or type 'help')` interleaved into an fs log line. At 115200 baud a byte is ~87 us,
/// so a 200-character line takes ~17 ms - and the old 50,000-SPIN cap was nowhere near that, because a
/// spin count is not a duration (the same trap as the USB waits earlier in this port). 40 ms of real time
/// covers any line this console writes.
const SERIAL_ACQUIRE_US: u32 = 40_000;

/// Write a whole message to the UART in order, waiting for the hardware rather than discarding.
/// **Only the line-ring drainer calls this**, and that is what makes waiting correct here.
///
/// The byte ring exists so an ORDINARY writer never waits on a 115200 line. That is still right, and
/// it is why every runtime writer queues into the line ring and returns. But the drainer is not an
/// ordinary writer: it is the single agent whose whole job is to put bytes on the wire, and there is
/// nothing behind it to starve. Making IT discard was the mistake - it turned "the wire is slower than
/// we are talking" into corrupted output, because a byte dropped from the middle of a line leaves a
/// line that reads exactly like a spliced one.
///
/// The previous run measured that precisely: 0 lines dropped, 0 bypassed, 0 producer collisions - the
/// whole structure above held - and ~95 BYTES discarded per drain, forty-nine times. The bytes were
/// coming out of the middles of lines.
///
/// So the granularity of loss has to match the granularity of meaning. A LINE may be dropped, loudly
/// and counted, because a missing line is visibly missing. Part of a line may not, because a line with
/// a hole in it still looks like a line and lies to whoever reads it. §26.7: loud failure over silent
/// corruption.
///
/// Waiting is bounded twice over: the FIFO poll gives up rather than hanging on a wedged UART, and the
/// drainer writes at most `SERIAL_DRAIN_BUDGET` lines before handing on.
fn pl011_write_blocking(s: &[u8]) {
    // Order first: anything already queued by the boot path or a panic must reach the wire before this
    // message, or writing directly here would be the very queue-jump this fixes.
    for _ in 0..TX_MAKE_ROOM_TRIES { tx_ring_drain(); }
    // SAFETY: PL011_FR/PL011_DR are the BCM2836 UART0 flag and data registers in the Device-mapped
    // peripheral window. Volatile MMIO: poll until the TX FIFO has room, then write one byte.
    unsafe {
        for &b in s {
            // Bounded: a wedged or absent UART with a permanently full TX FIFO must never hang the
            // output path (invariant 12 / §26.6). On a healthy UART the FIFO drains far faster.
            let mut t: u32 = 0;
            while PL011_FR.read_volatile() & PL011_FR_TXFF != 0 {
                t += 1;
                if t > 1_000_000 { break; } // give up on this byte rather than hang forever
            }
            PL011_DR.write_volatile(b as u32);
        }
    }
}

/// Put a message on the wire NOW, under the claim, whole. The bottom of the stack: everything else
/// either queues into the line ring or ends up here.
///
/// The claim is BOUNDED and a writer that times out writes anyway - which is the right trade for a
/// path that must never deadlock, and also the reason this cannot be the whole answer. A bounded claim
/// does not guarantee exclusivity; it guarantees progress. Two rounds of measurement on hardware said
/// so plainly: adding the line ring left the splice rate at 0.36%, and adding the claim to the runtime
/// path left it at 0.41%. Whoever times out writes into the middle of whoever holds it.
///
/// So exclusivity comes from having ONE writer at runtime - the ring's drainer - not from a lock that
/// everyone politely waits on. This function is what that one writer calls, plus the fallbacks that
/// genuinely cannot queue.
pub(super) fn pl011_write_raw(s: &[u8], fb: bool) {
    if !SERIAL_SMP.load(Ordering::Relaxed) {
        // Boot keeps the queueing byte path: there is one core, so nothing can interleave, and the
        // ring is what stops early output from waiting on the wire a byte at a time.
        for &b in s { pl011_write_byte(b); }
        // Mirror to the TV once the console is up (no-op before that). `mirror` tests a plain
        // `AtomicBool` BEFORE any exclusive access, which is load-bearing on this path: the first boot
        // messages run with the MMU off, where LDREX/STREX is UNPREDICTABLE on real silicon. A `mirror`
        // that locked or CAS'd here hangs the Pi on the firmware's rainbow splash with no serial output
        // at all - and QEMU, being permissive, does not reproduce it.
        if fb { bootcon::mirror(s); }
        return;
    }
    let held = claim_serial();
    pl011_write_blocking(s);
    // Mirror ONLY as the claim HOLDER. The boot floor has shared cursor/scroll state and assumes a
    // single writer; a contended writer that could not claim must not also render, or two writers
    // corrupt its position and the TV shows overlapping text. Its bytes still reached serial above,
    // which is the source of truth.
    if fb && held { bootcon::mirror(s); }
    release_serial(held);
}

/// Kernel and console output, mirrored to the display. QUEUED at runtime.
///
/// Every caller of this - and there are 186 of them across `arch/arm/` - used to write the wire
/// directly under a bounded claim, which is how a message got cut in half by a core that had waited
/// 40 ms and given up. Now they hand the message to the ring and return, and one drainer puts it on
/// the wire whole.
///
/// WHAT THIS STILL DOES NOT FIX, said plainly rather than discovered later: a message ASSEMBLED from
/// several calls is several ring entries, and another core's line can land between them.
/// `exceptions.rs` builds its fault report from about ten `pl011_write` fragments, so that report can
/// still interleave. Those sites want to become one `kprintln!` each; this change is what makes that
/// worth doing, because until now a single write was not atomic either.
pub(super) fn pl011_write(s: &[u8]) {
    if !SERIAL_SMP.load(Ordering::Relaxed) { pl011_write_raw(s, true); return; }
    serial_emit(s, true);
}

/// Write to the serial console WITHOUT mirroring to the TV. Used when a full-screen app owns the
/// display (`console_write_bytes_gated(.., false)`): the bytes still reach serial, which is the source
/// of truth and where a captured log comes from, but they do not paint over the app's screen.
pub(super) fn pl011_write_no_fb(s: &[u8]) {
    if !SERIAL_SMP.load(Ordering::Relaxed) { pl011_write_raw(s, false); return; }
    serial_emit(s, false);
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
    // Power on the USB HCD via the VideoCore mailbox BEFORE the MMU/caches (this exchange, like the
    // framebuffer one above, needs caches off). Circle does this before DWC2 init: the DWC2's AXI DMA
    // master is in a separate power/clock domain the firmware may leave off even though register reads
    // work - the leading suspect for "the master never dispatches" on the Pi 2 (dwc2.rs).
    if video::set_usb_power_on() {
        pl011_write(b"arm32: USB HCD powered on via VideoCore mailbox\r\n");
    } else {
        pl011_write(b"arm32: WARN USB HCD power-on mailbox failed (firmware may already have it on)\r\n");
    }
    // Read the board Ethernet MAC while caches are still off (mailbox coherency); the LAN9514 driver picks
    // it up during USB enumeration. The Pi 2 has no EEPROM, so this is the only source of the real MAC.
    video::read_board_mac();
    // SD card, same caches-off window: power the card's domain (the EMMC registers answer even when it is
    // off - the same trap as the USB HCD) and learn the EMMC base clock from the GPU. The base clock MUST
    // come from here: the Arasan's CAPS field is garbage on this SoC, and a guessed divider runs the
    // identification clock at the wrong speed, which no card answers.
    if video::set_sd_power_on() {
        pl011_write(b"arm32: SD card powered on via VideoCore mailbox\r\n");
    } else {
        pl011_write(b"arm32: WARN SD power-on mailbox failed (firmware may already have it on)\r\n");
    }
    video::read_emmc_clock();
    pl011_write(b"arm32: EMMC base clock = ");
    timer::write_dec_pub(video::emmc_clock_hz());
    pl011_write(b" Hz\r\n");
    mmu::enable();
    // Map the framebuffer and bring up the text console over it, so the boot log + shell prompt appear
    // on the TV (mirrored from serial). Everything logged from here on shows on the display.
    if let Some(fb) = fb {
        video::map(&fb);
        bootcon::init(fb.base, fb.pitch, fb.width, fb.height);
        pl011_write(b"arm32: framebuffer console up - this line should appear on the TV\r\n");
    }
    // Which image, which machine (see `crate::banner`). OUTSIDE the framebuffer check on purpose: a
    // headless Pi is exactly the case whose log is read over the serial cable. This port does not
    // reach `kernel_main`, so the call is here rather than there.
    crate::banner();
    // Route the SD-card pins to the Arasan EMMC (and report what the firmware left them as). After
    // `mmu::enable` because it touches the Device-mapped peripheral window, unlike the mailbox calls
    // above which must run caches-off.
    sd_route_to_emmc();
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
    #[cfg(feature = "arm-sched-demo")]
    sched_demo::run(ram_end, reserve_end);
    #[cfg(feature = "arm-sched-ipc")]
    sched_ipc::run(ram_end, reserve_end);
    #[cfg(feature = "arm-supervisor")]
    sched_supervisor::run(ram_end, reserve_end);
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

/// The VA at which a driver service's fixed-peripheral MMIO window is mapped in ITS address space. ARM
/// is 32-bit, so the x86 `XHCI_MMIO_VA` (0x1_0000_0000, 4 GiB) is out of range; this sits in the free
/// gap between a service's binary/heap and its user stack (`USER_STACK_TOP = 0x8000_0000`). The service
/// reads it back through `ctx.mmio()`.
pub const DRIVER_MMIO_VA: u32 = 0x6000_0000;

/// Map a fixed-physical peripheral MMIO window into a driver SERVICE's page table and return
/// `(va, byte_len)`, or `None` if this service needs no fixed MMIO. This is the §12.3 MMIO-cap grant for
/// ARM's non-PCI peripherals (x86 grants PCI BARs from the scan; the Pi's peripherals are at fixed
/// physical addresses). Mapped Device (`PCD`, uncached) + `USER` so the userspace driver can reach the
/// registers through the SDK `Mmio` wrapper - no `unsafe` in the service (§18.2). Called from the
/// neutral spawn path when a service's PCI BAR is 0 (always, on ARM).
pub fn map_fixed_driver_mmio(pt: &mut page_tables::PageTable, name: &str) -> Option<(u64, u64)> {
    use crate::memory::frame::PhysAddr;
    use page_tables::{PageFlags, VirtAddr};
    // `block-driver` on the Pi drives the Arasan EMMC (SD/EMMC) at peripheral + 0x30_0000.
    let (phys, pages): (u32, u32) = match name {
        "block-driver" => (PERIPHERAL_BASE as u32 + 0x30_0000, 1),
        // The DWC2 OTG core at peripheral + 0x98_0000. ONE page covers every register the driver
        // touches: the global block starts at 0, and the highest is host channel 15 at
        // 0x500 + 15*0x20 = 0x6E0. The data FIFOs live at 0x1000 and beyond and are NOT mapped,
        // because this controller is driven in DMA mode - the CPU never reads or writes a FIFO, so
        // granting that window would hand the driver reach it has no use for.
        "dwc2" => (PERIPHERAL_BASE as u32 + 0x98_0000, 1),
        _ => return None,
    };
    let flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE
        | PageFlags::NO_EXEC | PageFlags::PCD;
    for i in 0..pages {
        let off = i * 0x1000;
        pt.map(VirtAddr((DRIVER_MMIO_VA + off) as u64), PhysAddr((phys + off) as u64), flags).ok()?;
    }
    Some((DRIVER_MMIO_VA as u64, (pages * 0x1000) as u64))
}

/// Timer init - the generic timer + BCM2836 tick are already up (`timer::init` / `irq::start_tick`).
pub fn init_timer() {}

/// AP init - the secondary A7s are parked in `_start`; SMP bring-up (firmware mailboxes) is later
/// work. Never reached while `ap_count() == 0`.
pub fn ap_init(_core_id: u32) {}

pub use interrupts::{disable_interrupts, enable_interrupts, wait_for_interrupt, local_irq_save, local_irq_restore};
pub use page_tables::{read_page_table_base, write_page_table_base, invalidate_tlb_page};
pub use syscall_entry::{read_cycle_counter, read_user_bytes, validate_user_ptr, write_user_bytes, copy_user_to_kernel};

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

/// A11-2: a panic must stop this machine too. The Pi 2 runs four cores on real hardware.
///
/// `halt_all_cores` was `loop { spin_loop() }` - it masked nothing, so the panicking core kept taking
/// the timer IRQ and was scheduled away, and the other three never learned a panic had happened. Same
/// defect A10-1 found on aarch64 and SEC-18 fixed on x86; this port was simply never revisited. It is
/// worse here than on aarch64, because the arm liveness watchdog is armed off a measured timer and
/// there is no second backstop if this fails.
///
/// Same two halves as aarch64: the panicking core masks interrupts immediately, and `PANIC_HALT` is
/// published for the others, which check it on the tick every core takes.
pub static PANIC_HALT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Park THIS core forever with IRQ+FIQ masked.
pub fn park_core_forever() -> ! {
    // SAFETY: `cpsid if` masks IRQ and FIQ in CPSR, and `wfi` halts until an unmasked event; both are
    // always valid in a privileged mode, and this never returns, so nothing is left inconsistent.
    unsafe {
        core::arch::asm!("cpsid if", options(nomem, nostack));
        loop { core::arch::asm!("wfi", options(nomem, nostack)); }
    }
}

/// Called from the timer tick on EVERY core - see `PANIC_HALT`.
pub fn panic_halt_check() {
    if PANIC_HALT.load(core::sync::atomic::Ordering::Acquire) {
        park_core_forever();
    }
}

pub fn halt_all_cores() -> ! {
    PANIC_HALT.store(true, core::sync::atomic::Ordering::Release);
    park_core_forever()
}

/// Reset the machine via the BCM2835 power-management watchdog (the shell `reboot` command + Ctrl+Alt+Del).
/// Arm a short watchdog timeout and request a FULL reset in `PM_RSTC`; the SoC resets when the watchdog
/// fires. Every PM write is gated by the `0x5A` password. (The prior stub just spun, so `reboot` hung.)
pub fn hardware_reset() -> ! {
    const PM_RSTC: usize = PERIPHERAL_BASE + 0x10_001c;
    const PM_RSTS: usize = PERIPHERAL_BASE + 0x10_0020;
    const PM_WDOG: usize = PERIPHERAL_BASE + 0x10_0024;
    const PM_PASSWORD: u32 = 0x5A00_0000;
    const PM_RSTC_WRCFG_FULL_RESET: u32 = 0x0000_0020;
    const PM_RSTC_WRCFG_CLR: u32 = 0xffff_ffcf; // clear the WRCFG field before setting FULL_RESET
    // The BOOT PARTITION the firmware reads out of PM_RSTS after the watchdog fires, encoded across the
    // scattered bits of `0xfffffaaa`. Clearing them selects partition 0 - the normal boot. This step was
    // MISSING, and its absence matches the symptom exactly: the SoC did reset, the firmware then read a
    // partition it could not boot, and the board sat dark until it was power-cycled by hand - which reads
    // as "reboot does nothing". Every bare-metal Pi reset does this; Linux gets away without it only
    // because its own power-off path is what would have set the field.
    const PM_RSTS_PARTITION: u32 = 0xffff_faaa;
    /// Watchdog countdown in PM ticks (~15 us each). Ten is what every bare-metal Pi reset uses; the
    /// value never mattered here, because the old loop restarted the count before it could elapse.
    const WDOG_TICKS: u32 = 10;
    // SAFETY: PM_RSTS/PM_WDOG/PM_RSTC are the BCM2835 power-management registers, in the already
    // Device-mapped peripheral window; volatile 32-bit writes gated by the 0x5A password - the documented
    // reset poke.
    unsafe {
        let rstc = PM_RSTC as *mut u32;
        let rsts = PM_RSTS as *mut u32;
        let wdog = PM_WDOG as *mut u32;
        // Boot partition 0 on the way back up.
        let rsts_val = PM_PASSWORD | (rsts.read_volatile() & !PM_RSTS_PARTITION);
        rsts.write_volatile(rsts_val);
        let rstc_val = PM_PASSWORD | (rstc.read_volatile() & PM_RSTC_WRCFG_CLR) | PM_RSTC_WRCFG_FULL_RESET;
        // POKE ONCE, THEN LET THE WATCHDOG RUN OUT.
        //
        // This re-issued the poke on EVERY loop iteration, to retry a write that might not have taken
        // (kernel-audit Audit 6, N1). The intent was sound and the effect was its opposite: PM_WDOG is a
        // COUNTDOWN, and writing it RESTARTS it. Ten ticks is about 150 us; a tight spin makes millions
        // of passes in that time, so the counter was slammed back to 10 long before it could reach 0.
        // The hardening kept petting the dog it was waiting on to bite - so the board never reset and
        // `reboot` had to be finished by pulling the power. Hardware said so exactly:
        //
        //   rebooting...
        //   reboot: hardware reset
        //   reset: the SoC did NOT reset - the watchdog poke had no effect.
        //
        // Arm it once and wait. The retry the audit asked for is still here, but at a cadence LONGER
        // than the timeout it drives - a retry that outruns its own mechanism is not a retry.
        wdog.write_volatile(PM_PASSWORD | WDOG_TICKS);
        rstc.write_volatile(rstc_val);

        // BOUNDED, because "this never returns" is an assumption about hardware, and assumptions are
        // what invariant 12 exists for. If the SoC has not reset after a generous window, say so rather
        // than leaving the operator to guess whether to wait or pull the plug.
        let mut n: u32 = 0;
        let mut said = false;
        loop {
            n = n.saturating_add(1);
            if n % 20_000_000 == 0 {
                if !said {
                    said = true;
                    serial_write_bytes_lockfree(b"

reset: the SoC did NOT reset - the watchdog poke had no effect.

");
                    serial_write_bytes_lockfree(b"reset: power-cycle the board. (re-arming slowly in case it takes late)

");
                }
                // A write that genuinely did not land gets another chance; one that did has fired long
                // before this point.
                wdog.write_volatile(PM_PASSWORD | WDOG_TICKS);
                rstc.write_volatile(rstc_val);
            }
            core::hint::spin_loop();
        }
    }
}


/// The SD/EMMC controller's base clock in Hz, read from the VideoCore mailbox at boot (0 = the GPU
/// never reported one). Exposed to `block-driver` through InspectKernel query 20 because the Arasan's
/// own capability register reports this wrongly on the BCM283x, and the driver is granted only its
/// controller's registers - it cannot ask the mailbox itself (§12.3).
pub fn emmc_base_clock_hz() -> u32 { video::emmc_clock_hz() }
/// The board's own MAC address, read from the VideoCore mailbox at boot. See query 23.
pub fn board_mac_packed() -> Option<u64> {
    video::board_mac().map(|m| {
        (m[0] as u64) | ((m[1] as u64) << 8) | ((m[2] as u64) << 16)
            | ((m[3] as u64) << 24) | ((m[4] as u64) << 32) | ((m[5] as u64) << 40)
    })
}

/// USB mass-storage block device, served by the in-kernel DWC2 Bulk-Only stack (`dwc2`). Exposed to the
/// userspace `block-driver` through the USB_DISK-gated syscalls 46-48, the same shape as the USB-net
/// bridge: the kernel owns the controller and the transport, the driver owns the block protocol above it.

// --- The in-kernel USB stack is GONE (arm32 slice 5) ------------------------------------------------
//
// `arch/arm/dwc2.rs` (3,981 lines) and `arch/hid.rs` (241) are deleted. They were ring-0 code parsing
// descriptors supplied by whatever was plugged in, and a TCB member by construction (§6.4 amendment
// 2026-07-23). The `dwc2` SERVICE owns the controller now - hub, keyboard, mass storage and networking,
// each verified on hardware - and reaches it through an MMIO window, a DMA arena and the USB vector
// granted at spawn.
//
// These backends remain as stubs that say NO rather than vanishing, because the syscalls above them are
// shared with other ports: `net_frame_*` still serves aarch64's GENET. On arm they now answer "no
// device", which is the truth - the kernel no longer has one - and a client that ignores the answer
// fails loudly rather than reading stale bytes.
pub fn usb_disk_sectors() -> u64 { 0 }
pub fn usb_disk_read(_lba: u64, _dst: &mut [u8]) -> bool { false }
pub fn usb_disk_write(_lba: u64, _src: &[u8]) -> bool { false }
/// Make prior writes durable (SCSI SYNCHRONIZE CACHE) - see `dwc2::msc_sync_cache`.
pub fn usb_disk_flush() -> bool { false }
/// Did the last USB-disk transfer fail only because the device was BUSY (NAK)? Then it is not a
/// failure at all - the caller should re-ask, with interrupts enabled in between.
/// Counter ticks a core may make NO forward progress before the liveness watchdog panics. Same units as
/// [`boot::read_cycle_counter`] (both are `CNTPCT`), which is the whole reason this lives in the arch.
///
/// Derived, not picked: `timer::timer_hz()` is the **measured** counter rate, cross-checked against the
/// Pi's independent 1 MHz system timer - deliberately NOT `CNTFRQ`, which overstates it by 19.2x on this
/// board (see `timer.rs`). Taking `CNTFRQ` on faith would have made this deadline 19.2x too short and
/// turned a safety net into a source of spurious panics. It reads `0` until calibration completes, and
/// `0` disables the check, so the watchdog cannot arm on a rate we have not verified.
///
/// **10 s, not x86's ~3 s**, and the asymmetry is architectural rather than arbitrary: on ARM the USB
/// stack runs IN-KERNEL from syscall and idle context (`arch/arm/CLAUDE.md`), so a legitimate
/// interrupt-masked stretch here is a device wait - port reset, enumeration retries - measured in
/// hundreds of milliseconds, where x86's longest kernel critical section is a TLB shootdown measured in
/// microseconds. 10 s leaves ~20x headroom over the worst legitimate case while still catching decisively
/// the 20-30 s wedges a chaos run produced. A watchdog that panics a healthy machine is worse than none,
/// so the margin is deliberately generous; it can tighten once the ARM worst case is measured rather
/// than reasoned about.
/// (interrupts dispatched, last IRQ source) for `core` - what the liveness panic reports.
/// No-op: this arch counts every IRQ in `irq::arm_irq_dispatch`, which sees them all.
pub fn note_irq(_vector: u32) {}

pub fn core_irq_debug(core: u32) -> (u32, u32) {
    irq::core_irq_debug(core)
}

pub fn liveness_deadline_cycles() -> u64 {
    const LIVENESS_SECS: u64 = 10;
    (timer::timer_hz() as u64).saturating_mul(LIVENESS_SECS)
}

pub fn usb_disk_busy() -> bool { false }
/// Is there no USB disk attached at all? Answered from PRESENT state (`MSC_READY`), not from the last
/// transfer's outcome - which is exactly why it is a separate question. See `USB_DISK_ABSENT` in the
/// syscall dispatch for what conflating the two cost.
pub fn usb_disk_absent() -> bool { true }

/// A hardware-random u32 from the BCM2835 SoC RNG, or None if it never produced (absent/wedged - loud, not
/// a fallback). Ungated (InspectKernel query 19); the `random` shell utility consumes it. Best-effort under
/// concurrent callers (an unlocked FIFO pop) - fine for a diagnostic, not fed to crypto.
pub fn hw_random() -> Option<u32> {
    use core::sync::atomic::{AtomicBool, Ordering};
    const RNG_CTRL:   usize = PERIPHERAL_BASE + 0x10_4000;
    const RNG_STATUS: usize = PERIPHERAL_BASE + 0x10_4004;
    const RNG_DATA:   usize = PERIPHERAL_BASE + 0x10_4008;
    static INIT: AtomicBool = AtomicBool::new(false);
    // SAFETY: the BCM2835 RNG registers are in the already-Device-mapped peripheral window; volatile
    // 32-bit accesses. One-time enable (a warm-up count, then CTRL=1); read once a word is available.
    unsafe {
        if !INIT.swap(true, Ordering::Relaxed) {
            (RNG_STATUS as *mut u32).write_volatile(0x40000);        // warm-up cycles before the first read
            (RNG_CTRL as *mut u32).write_volatile(1);                // enable
        }
        // Status bits [31:24] = words available. Bounded so an absent/wedged RNG reports None, not a hang.
        let mut n = 0u32;
        while ((RNG_STATUS as *const u32).read_volatile() >> 24) == 0 {
            n += 1;
            if n > 2_000_000 { return None; }
        }
        Some((RNG_DATA as *const u32).read_volatile())
    }
}

/// Drive a BCM2835 GPIO pin (the shell `gpio` command, via the gated `Gpio` syscall). `op` = 0 input /
/// 1 output / 2 high / 3 low / 4 read; `pin` = 0..53. Returns the level (0/1) for a read, 0 on success,
/// -1 on a bad pin. GPIO carries the UART/SD lines, so this is gated by GPIO_DEVICE - the operator's call.
pub fn gpio_op(op: u32, pin: u32) -> i64 {
    if pin > 53 { return -1; }
    const GPFSEL0: usize = GPIO_BASE + 0x00; // function select (10 pins/reg, 3 bits each)
    const GPSET0:  usize = GPIO_BASE + 0x1c; // set output high (1 bit/pin, 2 banks)
    const GPCLR0:  usize = GPIO_BASE + 0x28; // set output low
    const GPLEV0:  usize = GPIO_BASE + 0x34; // read pin level
    let reg = (pin / 10) as usize;
    let shift = (pin % 10) * 3;
    let bank = (pin / 32) as usize;
    let bit = pin % 32;
    // SAFETY: BCM2835 GPIO registers in the already-Device-mapped peripheral window; volatile 32-bit
    // accesses. Read-modify-write of the function-select is single-core-serialised at the syscall layer.
    unsafe {
        match op {
            0 | 1 => {
                let fsel = (GPFSEL0 + reg * 4) as *mut u32;
                let mut v = fsel.read_volatile();
                v &= !(0b111 << shift);                 // clear the 3-bit function field
                if op == 1 { v |= 0b001 << shift; }     // 0b001 = output; 0b000 = input
                fsel.write_volatile(v);
                0
            }
            2 => { ((GPSET0 + bank * 4) as *mut u32).write_volatile(1 << bit); 0 }
            3 => { ((GPCLR0 + bank * 4) as *mut u32).write_volatile(1 << bit); 0 }
            4 => (((GPLEV0 + bank * 4) as *const u32).read_volatile() >> bit & 1) as i64,
            _ => -1,
        }
    }
}

/// Claim the UART for one whole message, bounded. `true` if the claim was taken (and must be
/// released); `false` if it timed out, in which case write anyway - interleaving beats deadlock, and
/// beats silence.
///
/// Factored out of `pl011_write`, which had this inline. The line ring's drainer needs the SAME claim,
/// and that is the fix to a bug in the ring's first version: it wrote through `pl011_write_no_fb`,
/// which by design takes no claim, so the drainer put a whole line onto the wire a BYTE at a time with
/// nothing stopping another core interleaving into it. The line ring made writers hand over whole
/// lines and then handed them to a byte-level path that shredded them anyway - which is why the first
/// version measured a splice rate identical to no ring at all (0.36% before, 0.36% after).
fn claim_serial() -> bool {
    // ARMv7 does NOT guarantee the local exclusive monitor is cleared on exception entry/return, so a
    // core that took an interrupt mid-`ldrex`/`strex` can leave it reserved to a foreign address and
    // every later `strex` here fails - forever. Same `clrex` `pl011_write` already does, for the same
    // reason, on the same silicon.
    // SAFETY: `clrex` clears the local exclusive monitor; no memory effect.
    unsafe { core::arch::asm!("clrex", options(nomem, nostack)); }
    let start = timer::systimer_us();
    loop {
        if SERIAL_BUSY
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
        if timer::systimer_us().wrapping_sub(start) > SERIAL_ACQUIRE_US { return false; }
        core::hint::spin_loop();
    }
}

/// Release a claim taken by `claim_serial`. A no-op when the claim timed out, so the caller can pass
/// the flag straight back without branching.
fn release_serial(held: bool) {
    if held { SERIAL_BUSY.store(false, Ordering::Release); }
}

// ---------------------------------------------------------------------------
// LINE-ATOMIC SERIAL OUTPUT.
//
// Four cores wrote this UART a BYTE at a time with nothing serialising a line, so lines shredded each
// other: `rvisor' spawned OK on ctask: 'time' spawned OKdwc2-svc: =7 'shell' freed 161 frames` is one
// real line from one real run, carrying fragments of three messages and the whole of none.
//
// That is not cosmetic. A spliced line matches no pattern, so it is invisible to any count made over
// the log - and counting task spawns against task kills is how a duplicate-instance bug is found. It
// produced three wrong readings of the same Pi 2 run in a row, and one of them was reported as fact
// before the arithmetic gave it away (52 spawns, 52 kills, and a filesystem demonstrably working
// afterwards - so at least one spawn line had been destroyed outright). An instrument that silently
// deletes evidence is worse than no instrument, because its output still looks like data.
//
// WHY THE EXISTING MACHINERY DID NOT COVER THIS, both parts of it:
//
//   - `SERIAL_BUSY` (the bounded claim in `pl011_write`) is only taken while `BOOT_LOG_TO_FB` is set,
//     which stops at the shell's first prompt. Every runtime log line after that goes through
//     `pl011_write_no_fb`, which by design takes no claim at all. So the lock existed and the traffic
//     that splices never went near it.
//   - `TX_RING` (the byte ring under `pl011_write_byte`) stops a writer WAITING on the FIFO, which is
//     a different problem and a real fix. It cannot help here: two cores pushing bytes into it
//     interleave in the ring exactly as they interleaved on the wire.
//
// This is the aarch64 design (`arch/aarch64/mod.rs`), ported, including the three attempts recorded
// there that came before it: spinning for the claim cost 104-second boots; a bounded spin still
// spliced at 10 ms and made chaos rounds 6x slower at 150 ms - waiting is never worth it, because an
// interleaving core at least makes progress; and routing EVERY writer through the ring, early boot
// included, hung the machine before its first line. So the ring serves the RUNTIME writers only, and
// `pl011_write`'s pre-SMP path is left exactly as it is, because that is the code that boots.
// ---------------------------------------------------------------------------

/// Longest line the ring will take. Matches `log::SERIAL_STAGE`, the largest single flush the neutral
/// log produces; anything longer goes straight to the wire rather than being truncated.
const SERIAL_LINE_MAX: usize = 512;

/// Lines the ring holds. 12 KiB of `.bss`, fixed - no heap (§26.6.1), bound readable off the constant.
const SERIAL_RING_LINES: usize = 24;

/// Lines one core puts on the wire before letting another take over.
///
/// A WRITER BECOMES THE DRAINER, so this is work conscripted from whoever happened to log next. At a
/// full ring over two passes that would be 48 lines - roughly a third of a second of wire time at
/// 115200 - and a core vanishing for 340 ms to do everyone else's logging is not bounded behaviour in
/// any useful sense (§26.6); it is the same "one core pays for everyone" shape that made the spin
/// design unusable, relocated. Four is enough to keep the wire busy: the next writer picks up where
/// this one stopped, and under any load worth draining there is always a next writer.
const SERIAL_DRAIN_BUDGET: usize = 4;

struct SerialRing {
    buf: [[u8; SERIAL_LINE_MAX]; SERIAL_RING_LINES],
    len: [u16; SERIAL_RING_LINES],
    /// Does this line belong on the TV as well as the wire? Carried PER LINE because the two runtime
    /// writers disagree: kernel log output stops mirroring at the shell's first prompt, and console
    /// output is gated on whether a full-screen app owns the display. The drainer must not have to
    /// guess, and must not answer from whatever the flags happen to say by the time it runs.
    fb:  [bool; SERIAL_RING_LINES],
    head: usize,
    count: usize,
    /// Lines lost to a full ring. REPORTED on the next drain, never silently discarded (invariant 12).
    dropped: u32,
}

impl SerialRing {
    const fn new() -> Self {
        SerialRing {
            buf: [[0; SERIAL_LINE_MAX]; SERIAL_RING_LINES],
            len: [0; SERIAL_RING_LINES],
            fb:  [false; SERIAL_RING_LINES],
            head: 0,
            count: 0,
            dropped: 0,
        }
    }
}

/// The queue.
///
/// **Every acquisition is `try_lock`, never `lock`.** This path is reachable from ordinary code, from
/// interrupt handlers, and from the panic path, and it must not be able to spin or to touch interrupt
/// state. A failed try writes straight to the wire instead - so a same-core ISR that logs while the
/// task it interrupted holds the lock DEGRADES to an interleaved line rather than deadlocking, and no
/// caller can ever be parked here. The hold is a memcpy, so a failed try is rare.
static SERIAL_RING: crate::smp::SpinLock<SerialRing> = crate::smp::SpinLock::new(SerialRing::new());

/// Set while a core is putting queued lines on the wire. Not a lock anyone waits on: a core that finds
/// it taken has already handed its line over and simply returns.
static SERIAL_DRAINING: AtomicBool = AtomicBool::new(false);

/// Once a panic starts the ring is bypassed entirely: a machine that is stopping may never drain a
/// queue, and the last thing it says is the thing worth saying.
static SERIAL_PANIC_MODE: AtomicBool = AtomicBool::new(false);

/// Stop queueing; write straight to the wire from here on. Called before the panic's first line.
pub fn serial_enter_panic_mode() {
    SERIAL_PANIC_MODE.store(true, Ordering::Release);
}

/// Straight onto the wire, whole. `fb` also mirrors to the display, which needs the `SERIAL_BUSY`
/// claim because the boot floor has shared cursor state and assumes one writer at a time.
fn serial_write_direct(s: &[u8], fb: bool) {
    // The RAW bottom, never `pl011_write`: those queue now, and a fallback that queued would recurse
    // straight back into the ring it is the fallback for.
    pl011_write_raw(s, fb);
}

/// How long a writer will wait to get INTO the ring. Microseconds, not milliseconds - the hold is a
/// memcpy of at most 512 bytes, which is about a microsecond on this core. This is nothing like the
/// 40 ms `SERIAL_ACQUIRE_US` wire claim; that one waits for the UART, this one waits for a memcpy.
const RING_ACQUIRE_US: u32 = 250;

/// Messages that never reached the ring, and so were never written at all. REPORTED, never silent.
static SERIAL_BYPASSED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Hand one whole message to the ring. At runtime this NEVER writes the wire itself.
///
/// **THE RULE: a writer that cannot get into the ring does not write.** It is counted and the message
/// is dropped.
///
/// That is the correction two hardware runs forced. Every earlier version had a "write it anyway"
/// fallback on each failure path, which guarantees progress and destroys the only property that
/// matters here. A bounded claim is not mutual exclusion: whoever times out writes into the middle of
/// whoever holds it. The measurements were flat because of it - the line ring left the splice rate at
/// 0.36%, adding a claim to the runtime path left it at 0.41%, and the log then named the mechanism
/// directly: `kernel: supervisor died` is ONE `kprintln!`, one write, and it appears 92 times whole
/// and twice cut in half. A single write cannot be cut by fragmentation. It can only be cut by a
/// second writer, which is exactly what "write it anyway" creates.
///
/// This is also how Linux draws the line, and for the same reason. `printk` formats a record into a
/// per-CPU buffer and commits it to the ring whole or not at all; consoles are drained by ONE owner
/// (`console_lock`, or `nbcon`'s explicit ownership handover in 6.7+, which exists precisely because
/// a spin cannot work in NMI). Linux will happily LOSE a record to ring overwrite. What it will not do
/// is let a second agent write the device. The borrowed thing is that ordering of priorities, not any
/// of its machinery (§26.14).
///
/// A dropped line is honest and counted. A spliced line is corruption that takes its neighbours with
/// it - it destroyed three separate readings of the same run, and one was reported as fact before the
/// arithmetic caught it. §26.7 ranks these: loud failure beats silent corruption.
fn serial_emit(s: &[u8], fb: bool) {
    // A halting machine may never drain a queue, so the last thing it says goes straight out. Splices
    // are a fair price for output that exists.
    if SERIAL_PANIC_MODE.load(Ordering::Acquire) {
        serial_write_direct(s, fb);
        return;
    }
    // Before SMP there is exactly one writer, so direct IS single-writer. The exclusive monitor the
    // ring's lock needs is also UNPREDICTABLE with the MMU off on this silicon - routing early boot
    // through the ring hung the aarch64 port before its first line, which cost a boot to learn.
    if !SERIAL_SMP.load(Ordering::Relaxed) {
        serial_write_direct(s, fb);
        return;
    }

    // Wait BRIEFLY for the ring - the holder is doing a memcpy, not talking to hardware.
    // SAFETY: `clrex` clears the local exclusive monitor before the lock's compare-exchange; ARMv7
    // does not clear it on exception entry/return, so a core interrupted mid-`ldrex`/`strex` would
    // otherwise fail every later `strex` on this path forever.
    unsafe { core::arch::asm!("clrex", options(nomem, nostack)); }
    let start = timer::systimer_us();
    loop {
        if let Some(mut r) = SERIAL_RING.try_lock() {
            if r.count == SERIAL_RING_LINES {
                // The system is talking faster than 115200 can carry. Counted and reported on the next
                // drain rather than pretending the line was written.
                r.dropped = r.dropped.saturating_add(1);
            } else {
                let idx = (r.head + r.count) % SERIAL_RING_LINES;
                // TRUNCATED, not bypassed. An over-long message loses its tail and says so; the old
                // code wrote it directly, which is the second writer this function exists to forbid.
                let n = if s.len() > SERIAL_LINE_MAX { SERIAL_LINE_MAX } else { s.len() };
                r.buf[idx][..n].copy_from_slice(&s[..n]);
                r.len[idx] = n as u16;
                r.fb[idx]  = fb;
                r.count += 1;
            }
            break;
        }
        if timer::systimer_us().wrapping_sub(start) > RING_ACQUIRE_US {
            // Never written. Counted instead - see the rule above.
            SERIAL_BYPASSED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        core::hint::spin_loop();
    }
    serial_drain();
}

/// Render the loss report into a caller-supplied buffer. Bounded, on the stack, no heap (§26.6.1);
/// written by hand rather than through `log_fmt` because this runs INSIDE the drainer and must not
/// re-enter the log path it is reporting on.
fn fmt_lost(buf: &mut [u8], dropped: u32, bypassed: u32, discarded: u32, collisions: u32) -> usize {
    let mut n = 0usize;
    n = fmt_bytes(buf, n, b"serial: ");
    n = fmt_u32(buf, n, dropped);
    n = fmt_bytes(buf, n, b" line(s) dropped, ");
    n = fmt_u32(buf, n, bypassed);
    n = fmt_bytes(buf, n, b" bypassed, ");
    n = fmt_u32(buf, n, discarded);
    n = fmt_bytes(buf, n, b" byte(s) discarded, ");
    n = fmt_u32(buf, n, collisions);
    n = fmt_bytes(buf, n, b" producer collision(s) - the log is INCOMPLETE by that much");
    n = fmt_bytes(buf, n, &[13, 10]);
    n
}

/// Append, stopping at the buffer's end rather than panicking on it.
fn fmt_bytes(buf: &mut [u8], mut n: usize, src: &[u8]) -> usize {
    for &b in src { if n < buf.len() { buf[n] = b; n += 1; } }
    n
}

/// Decimal, no padding.
fn fmt_u32(buf: &mut [u8], mut n: usize, v: u32) -> usize {
    if v == 0 { if n < buf.len() { buf[n] = b'0'; n += 1; } return n; }
    let mut d = [0u8; 10];
    let mut i = 0;
    let mut v = v;
    while v > 0 { d[i] = b'0' + (v % 10) as u8; v /= 10; i += 1; }
    while i > 0 { i -= 1; if n < buf.len() { buf[n] = d[i]; n += 1; } }
    n
}

/// Put queued lines on the wire. At most one core at a time; the rest have handed their line over.
fn serial_drain() {
    // Two passes: the second closes the window where a line is queued between the last pop and the
    // release, which would otherwise sit until some later write happened to drain it.
    for _ in 0..2 {
        if SERIAL_DRAINING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return; // someone else has the wire; our line is queued and they will write it
        }

        let mut line = [0u8; SERIAL_LINE_MAX];
        let mut lost = 0u32;
        let mut wrote = 0usize;
        while wrote < SERIAL_DRAIN_BUDGET {
            let (n, fb) = match SERIAL_RING.try_lock() {
                None => break, // contended; leave the rest for the next writer
                Some(mut r) => {
                    if r.dropped > 0 {
                        lost = lost.saturating_add(r.dropped);
                        r.dropped = 0;
                    }
                    if r.count == 0 { break; }
                    let idx = r.head;
                    let n = r.len[idx] as usize;
                    line[..n].copy_from_slice(&r.buf[idx][..n]);
                    let fb = r.fb[idx];
                    r.head = (r.head + 1) % SERIAL_RING_LINES;
                    r.count -= 1;
                    (n, fb)
                }
            };
            // ONE CLAIM PER LINE, not one across the pass. A line is the unit that has to be atomic,
            // and holding the claim across four framebuffer renders is the aarch64 port's recorded
            // failure #1 - the render is far more expensive than the wire write it is protecting.
            pl011_write_raw(&line[..n], fb);
            wrote += 1;
        }

        // BOTH numbers, in band. A run whose log is being used as evidence needs to say how much of
        // itself is missing - "0 dropped, 0 bypassed" is what makes a count over the log trustworthy,
        // and any other figure says exactly how far to trust it.
        let bypassed = SERIAL_BYPASSED.swap(0, Ordering::Relaxed);
        let discarded = TX_DISCARDED.swap(0, Ordering::Relaxed);
        let collisions = TX_PRODUCER_COLLISIONS.swap(0, Ordering::Relaxed);
        if lost > 0 || bypassed > 0 || discarded > 0 || collisions > 0 {
            let mut note = [0u8; 96];
            let n = fmt_lost(&mut note, lost, bypassed, discarded, collisions);
            pl011_write_raw(&note[..n], false);
        }

        SERIAL_DRAINING.store(false, Ordering::Release);

        let pending = match SERIAL_RING.try_lock() {
            Some(r) => r.count > 0,
            None => false,
        };
        if !pending { return; }
    }
}

// ---- Serial / console (PL011: output = pl011_write; input = the PL011 RX FIFO drained into a ring) ----
pub fn serial_write_byte(b: u8) { pl011_write_byte(b); }
pub fn serial_write_bytes_lockfree(s: &[u8]) {
    // Kernel log output. Mirrored to the TV only while booting: afterwards the display belongs to the
    // shell, and a log line landing mid-prompt is what made the prompt look absent.
    // QUEUED, so the line reaches the wire WHOLE. Four cores logging concurrently used to shred each
    // other's lines byte by byte - and this is the path that did it, because `pl011_write_no_fb` takes
    // no claim by design. The fb destination is decided HERE and carried with the line: by the time
    // the drainer runs, `BOOT_LOG_TO_FB` may say something different.
    serial_emit(s, BOOT_LOG_TO_FB.load(Ordering::Acquire));
}
/// The shell's console output. `to_fb` is the CONSOLE FOREGROUND gate: false means a full-screen app
/// owns the display, so this text belongs on the serial console only and must NOT reach the TV.
///
/// It used to be ignored, with the comment "no framebuffer on this port" - true when it was written,
/// false since the framebuffer console landed. So every log line from a dying or restarting service painted itself over
/// whatever full-screen app was running, which is why `chaos max-carnage` never held the screen: the
/// carnage it creates is precisely a flood of service log traffic, all of it un-gated.
pub fn console_write_bytes_gated(s: &[u8], to_fb: bool) {
    // Output from the foreground owner RENEWS its claim: drawing is the evidence that it is still
    // doing the job it took the console for. `to_fb` is true exactly when the writer is the owner (or
    // the console is unclaimed), which is the same predicate the gate uses.
    if to_fb {
        CONSOLE_FG_RENEWED_US.store(timer::systimer_us(), Ordering::Relaxed);
    }
    // Same ring as the kernel log, which is the point: the two writers that used to interleave now
    // take turns a whole line at a time. Renewing the claim stays HERE rather than in the drainer -
    // it is the writer that did the drawing, and the lease is about the writer, not the wire.
    serial_emit(s, to_fb);
}
pub fn set_console_echo(on: bool) {}

// Console FOREGROUND. `u32::MAX` = unclaimed (anyone may read, everything reaches the TV); otherwise
// the task slot of the full-screen app that owns the console. These were empty stubs with
// `console_foreground_allows` hardwired to `true`, which had two consequences on the TV: log output
// was never gated (above), and - because the same predicate gates console READS - the shell kept
// consuming input while a full-screen app was running, so `q` never reached the app that was waiting
// for it. Same semantics as `arch/x86_64/mod.rs`.
static CONSOLE_FOREGROUND: AtomicU32 = AtomicU32::new(u32::MAX);

/// How long a console claim survives without the owner producing output. A full-screen app renews by
/// DRAWING - which is what such an app does, every frame - so a working one never notices this. One
/// that has stopped drawing has stopped being a full-screen app, whatever its task state says.
/// Generous enough to cover the slowest legitimate gap observed (a `chaos` round on this board runs
/// ~17 s between repaints).
const CONSOLE_FG_LEASE_US: u32 = 45_000_000;
static CONSOLE_FG_RENEWED_US: AtomicU32 = AtomicU32::new(0);

/// Claim exclusive console input, for a BOUNDED term.
///
/// The claim used to be perpetual, which made it the one authority here that a task could hold forever
/// after it had stopped using it: the shell stayed muted, the screen kept the dead app's last frame,
/// and on this board serial goes through the same gate - so a live, healthy machine had no usable
/// console and the only way back was the power switch.
///
/// The first attempt at a fix was a magic key the kernel watched for. Wrong twice over. It imported a
/// habit from other systems, which this one owes nothing to (§2.2, Appendix B.3) - and worse, it put
/// POLICY IN THE KERNEL (§26.10), deciding what a keystroke MEANS. That is exactly what the SEC-2
/// amendment took out of the USB driver, leaving the driver to signal and the principal holding the
/// authority to decide. It was also undiscoverable, and the chord chosen did not even decode on this
/// port's USB keyboard, so it worked only from serial - not where an operator at the TV is sitting.
///
/// Bounding the authority needs no keystroke and no interpretation. The kernel enforces a limit, which
/// is mechanism; nothing guesses what the operator wants, and there is no chord to discover, document,
/// or find out does not work on the keyboard in front of you. §26.6 lists bounded authority as a
/// requirement; a perpetual claim was simply an unbounded one that had not bitten yet.
pub fn claim_console_foreground(task_slot: u32) {
    CONSOLE_FG_RENEWED_US.store(timer::systimer_us(), Ordering::Relaxed);
    CONSOLE_FOREGROUND.store(task_slot, Ordering::Release);
}

/// Has the current claim lapsed? Released and reported once, at the moment it happens (invariant 12).
fn console_fg_lapsed() -> bool {
    if CONSOLE_FOREGROUND.load(Ordering::Acquire) == u32::MAX { return false; }
    let since = timer::systimer_us().wrapping_sub(CONSOLE_FG_RENEWED_US.load(Ordering::Relaxed));
    if since < CONSOLE_FG_LEASE_US { return false; }
    CONSOLE_FOREGROUND.store(u32::MAX, Ordering::Release);
    wake_console_waiter();
    pl011_write_no_fb(b"console: the foreground app stopped drawing - its claim lapsed, console returned
");
    true
}
pub fn release_console_foreground() {
    CONSOLE_FOREGROUND.store(u32::MAX, Ordering::Release);
    wake_console_waiter();
}
pub fn release_console_foreground_if_owner(task_slot: u32) {
    if CONSOLE_FOREGROUND
        .compare_exchange(task_slot, u32::MAX, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        wake_console_waiter();
    }
}
/// May `task_slot` read console input, and does its output reach the TV? True when the foreground is
/// unclaimed or this task holds it.
pub fn console_foreground_allows(task_slot: u32) -> bool {
    let owner = CONSOLE_FOREGROUND.load(Ordering::Acquire);
    if owner == u32::MAX || owner == task_slot { return true; }
    console_fg_lapsed() // a claim nobody is renewing does not keep the console
}
/// Wake a task parked in a muted blocking console read, so releasing the foreground resumes it at once
/// instead of leaving it asleep until the next keystroke - which is why the prompt did not come back
/// on its own when a full-screen app exited.
fn wake_console_waiter() {
    let waiter = CONSOLE_READ_WAITER.load(Ordering::Acquire);
    if waiter != u32::MAX {
        crate::task::scheduler::wake_by_slot(waiter as usize, 0);
    }
}

/// Whether KERNEL LOG output is still mirrored to the TV. True during boot so the init sequence is
/// visible on the display; the shell flips it false the moment boot completes.
static BOOT_LOG_TO_FB: AtomicBool = AtomicBool::new(true);
static BOOT_DISMISSED: AtomicBool = AtomicBool::new(false);

/// Boot is over: clear the boot screen and stop mirroring LOG output to it.
///
/// This was an empty stub, and the visible consequence was the prompt never appearing until a key was
/// pressed. The shell prints `gsh> ` and then a late service log - `fs: journal recovered 5 block(s)`
/// arriving after the shell is up - lands on the same line, leaving the cursor after the log text with
/// no prompt at the start of a fresh line. Pressing Enter simply redrew it.
///
/// x86 has always done this (`BOOT_LOG_TO_FB` + `fb::clear_and_home`), and the shell's own comment
/// states the contract it expects: "dismiss the boot screen on the TV (clear + stop mirroring logs to
/// it) and present a clean prompt. Serial keeps the full stream." ARM simply never implemented its
/// half. Serial is unchanged - it still receives everything, which is what a captured log needs.
pub fn console_boot_complete() {
    if BOOT_DISMISSED.swap(true, Ordering::AcqRel) { return; }
    BOOT_LOG_TO_FB.store(false, Ordering::Release);
    bootcon::clear_and_home();
}

// PL011 receive FIFO -> a single-producer/single-consumer input ring. The producer is `pl011_rx_drain`
// (polled from the timer tick and by a blocked `console_read` itself); the consumer is `uart_rx_pop`
// (the ConsoleRead syscall). PL011 FR bit 4 = RXFE (RX FIFO empty).
const PL011_FR_RXFE: u32 = 1 << 4;
const RX_BUF_SIZE: usize = 256;
static mut RX_BUF: [u8; RX_BUF_SIZE] = [0; RX_BUF_SIZE];
static RX_HEAD: AtomicU32 = AtomicU32::new(0);
static RX_TAIL: AtomicU32 = AtomicU32::new(0);
static INPUT_READY: AtomicBool = AtomicBool::new(false);

/// Drain every byte currently in the PL011 RX FIFO into the input ring.
///
/// **Must be a single producer IN FACT, and it was not.** The old comment said "single producer" and
/// trusted "the single-core port" to serialise callers - but the port is SMP now, and this is reached
/// from the core-0 timer tick, from the AP IDLE loops (`uart_rx_poll` - its MPIDR gate protected only
/// `dwc2::poll`, not the drain), and from a blocked `console_read`. Two racing drains DUPLICATE input:
/// both read FR as non-empty for the same byte, the first DR read empties the FIFO, and the second DR
/// read - the PL011 data register on an empty FIFO returns the stale last byte - pushes the SAME byte
/// again. Observed as `kkkill` / `chaos maxchaos max-carnage...` - typed commands garbled whenever
/// output was streaming (the idle cores drain most eagerly exactly then), on QEMU and on the Pi alike.
///
/// So the drain must have EXACTLY ONE producer at a time. It used to get that by refusing to run
/// anywhere but core 0 - which is single-producer, but far more than single-producer, and the excess
/// cost real function: `uart_rx_drain_now` exists precisely so a blocked `console_read` can collect its
/// own input without waiting for the timer tick, and that self-drain was a silent no-op whenever the
/// shell was not on core 0. Services are round-robin placed and re-placed on restart, so a shell that
/// respawned onto core 1-3 lost its own input path and depended entirely on core 0 idling. The USB
/// keyboard was unaffected (it pushes through `console_push_byte`), so the signature was the odd one of
/// a live keyboard beside a dead serial line.
///
/// Exclusion is now by PROTOCOL rather than by core, the same discipline `UsbExclusive` uses next door:
/// a CAS claims the drain, any core may win it, and a loser returns immediately instead of waiting -
/// correct because the winner is draining that very FIFO right now, so the bytes reach the ring either
/// way. IRQs stay masked inside so this core's own tick cannot interleave between the FR check and the
/// DR read - the same-core variant of the identical stale-DR duplication.
static RX_DRAIN_CLAIMED: AtomicBool = AtomicBool::new(false);

/// RX bytes discarded because the UART flagged them (framing/parity/break/overrun), and whether that
/// has been reported yet.
///
/// Discarding line noise is right, but discarding it **silently** is the failure mode invariant 12
/// exists to forbid: to an operator, "serial input does not work" and "serial input is being flooded
/// with break errors and thrown away" look identical, and only one of them tells you to check the
/// wiring. So the condition is announced once, naming the likely cause. Once, not per byte, because a
/// held line produces thousands per second and a log storm would be its own denial of service.
static RX_LINE_ERRORS: AtomicU32 = AtomicU32::new(0);

/// Bytes the RX line produced that had nowhere to go, because the input ring was already full.
static RX_OVERRUN: AtomicU32 = AtomicU32::new(0);
/// Keystrokes lost the same way, and whether that has been reported. Counted separately from
/// `RX_OVERRUN` because the two mean opposite things: a dropped noise byte is the system working, a
/// dropped KEYSTROKE is the user's input vanishing. That must never be silent (§26.7).
static KBD_DROPPED: AtomicU32 = AtomicU32::new(0);
static KBD_DROP_REPORTED: AtomicBool = AtomicBool::new(false);
/// Set once the receiver has been shut off because the line is faulty. Latches until reboot.
static RX_SHUT_OFF: AtomicBool = AtomicBool::new(false);

/// Microsecond timestamp at which the ring began overflowing without pause; 0 = not overflowing.
static RX_OVERFLOW_SINCE: AtomicU32 = AtomicU32::new(0);

/// How long the ring must overflow WITHOUT PAUSE before the line is judged faulty.
///
/// **A duration, not a count, and that distinction cost three attempts.** Each earlier version picked a
/// magic number and none of them fired in the very scenario they were written for:
///
/// - 2000 discarded *error* bytes: the fault carries no error flags, so the counter never moved.
/// - 512 *identical consecutive* bytes: occasional 0x00s among the 0xFFs reset the run every time.
/// - 4096 *dropped* bytes: right quantity, wrong units. At the measured ~141 drops/s that is 29 seconds
///   of an unusable machine before the self-heal engages - longer than anyone waits before giving up, so
///   in practice it never engaged either.
///
/// The failure's actual signature is not "many drops", it is "the ring has been full CONTINUOUSLY".
/// A burst (a paste, a fast typist) overflows briefly and then drains, resetting the timer; a faulty
/// line never lets it drain. Two seconds is far longer than any burst survives and short enough that the
/// machine heals before a user concludes it is dead.
const RX_OVERFLOW_FAULT_US: u32 = 2_000_000;
static RX_LINE_ERRORS_REPORTED: AtomicBool = AtomicBool::new(false);

/// Discarded-byte count at which the line is judged genuinely faulty rather than momentarily glitched.
///
/// A handful of framing errors is normal - a cable hot-plugged mid-stream, or a terminal attaching at
/// the wrong baud and resyncing. Sustained hundreds means the line is held.
///
/// **Set from measurement, not from a guess.** The first cut used 2,000 and never fired: the Pi 2
/// session that motivated this discarded roughly 945 bytes over eight minutes (966 spurious repaints
/// before the fix, 21 after), so the threshold sat above the very condition it was written to catch.
/// A check that cannot fire in its own worked example is worse than no check - it reads as "the line is
/// fine". 128 clears connect-time glitches by a wide margin and still trips within seconds of a held
/// line.
const RX_LINE_ERROR_REPORT_AT: u32 = 128;

fn pl011_rx_drain() {
    // Clear any stale exclusive-monitor reservation before the compare-exchange, for exactly the reason
    // `pl011_write` does: ARMv7 does NOT guarantee the local monitor is cleared on exception entry or
    // return, so a task interrupted mid-`ldrex`/`strex` can leave it reserved to a foreign address -
    // after which EVERY `strex` here fails, forever. A CAS that can never succeed makes this drain
    // return immediately every time, which is serial input silently dead. Omitting it is what broke
    // serial on the first cut of this change; QEMU's TCG does not model the monitor strictly enough to
    // show it, so it looked fine in emulation and failed on the Pi.
    if RX_SHUT_OFF.load(Ordering::Relaxed) {
        return; // receiver shut off: the line is stuck and we are no longer listening to it
    }
    // SAFETY: `clrex` clears the local exclusive monitor; no memory effect.
    unsafe { core::arch::asm!("clrex", options(nomem, nostack)); }
    // Claim, or stand aside. Acquire/Release pair the ring writes with the next claimant's view.
    if RX_DRAIN_CLAIMED
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let mut stuck: Option<(u8, u32)> = None;
    let mut overflowed = false;
    let saved = interrupts::local_irq_save();
    // SAFETY: reading the PL011 FR/DR (Device-mapped MMIO) and appending to the ring; core-0-only and
    // IRQ-masked (above), so this is the only producer executing.
    unsafe {
        loop {
            if PL011_FR.read_volatile() & PL011_FR_RXFE != 0 { break; } // RX FIFO empty
            let dr = PL011_DR.read_volatile();
            // DISCARD a byte the UART flagged as framing/parity/break/overrun. The flags arrive in the
            // SAME read as the data (bits 11:8), so masking them off - which this used to do - silently
            // promotes line noise to input.
            //
            // This is not a theoretical concern: a GPIO HAT sitting on the UART pins holds GPIO15 (RX)
            // low, which the PL011 reports as a CONTINUOUS break condition. Each one enqueues a
            // spurious 0x00, so the input ring fills with nulls forever. That does not merely make
            // typing dead - it makes every reader spin. A full-screen app blocked in `ConsoleRead`
            // wakes on each null, discards it as unprintable, and repaints: `edit` on the Pi 2 issued
            // 966 full-screen repaints while the document changed twice. Dropping the flagged byte
            // costs nothing when the line is healthy and turns a flood back into silence when it is not.
            if dr & PL011_DR_ERR != 0 {
                PL011_ECR.write_volatile(0); // clear the sticky error status (any write clears)
                RX_LINE_ERRORS.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let b = (dr & 0xFF) as u8;
            // Stuck-line detection. A faulty RX line does not merely deliver junk - it STARVES the
            // machine's other input. The ring is shared with the USB keyboard, and a producer that fills
            // it faster than anything drains it means every keystroke is dropped for want of space. That
            // is how a Pi 2 with a GPIO HAT became unquittable: `edit` repainted on phantom bytes and
            // Ctrl+Q never got in, so the only way out was the power switch.
            //
            // So once the line is provably not a console, stop listening to it. Serial OUTPUT is
            // untouched (it is a different pin and this only clears RXE), and the keyboard gets the ring
            // to itself. Latches until reboot: if the cause is fixed, reboot to re-enable - deliberate,
            // because silently resuming a line we just declared faulty would be the fallback §26.7
            // forbids.

            let tail = RX_TAIL.load(Ordering::Relaxed) as usize;
            let head = RX_HEAD.load(Ordering::Acquire) as usize;
            let next = (tail + 1) % RX_BUF_SIZE;
            if next == head {
                // Ring full: drop this byte, but COUNT it. A line that persistently has nowhere to put
                // its bytes is out-producing every consumer, and the ring it is filling is shared with
                // the USB keyboard - so the real cost is not the junk, it is that genuine keystrokes are
                // discarded for want of space. That is how a Pi 2 became unquittable: `edit` repainted on
                // phantom bytes and Ctrl+Q never got in, leaving the power switch as the only way out.
                let over = RX_OVERRUN.fetch_add(1, Ordering::Relaxed) + 1;
                overflowed = true;
                let now = timer::systimer_us();
                let since = RX_OVERFLOW_SINCE.load(Ordering::Relaxed);
                if since == 0 {
                    RX_OVERFLOW_SINCE.store(now | 1, Ordering::Relaxed); // never store 0 ("not overflowing")
                } else if now.wrapping_sub(since) > RX_OVERFLOW_FAULT_US
                    && !RX_SHUT_OFF.swap(true, Ordering::AcqRel)
                {
                    let cr = PL011_CR.read_volatile();
                    PL011_CR.write_volatile(cr & !(1 << 9)); // clear RXE - stop receiving
                    stuck = Some((b, over));
                }
                continue; // keep draining the FIFO either way
            }
            RX_BUF[tail] = b;
            RX_TAIL.store(next as u32, Ordering::Release);
        }
    }
    // A drain that placed every byte means the consumer is keeping up: the overflow was a burst, not a
    // fault, so restart the clock. Only UNBROKEN overflow counts.
    if !overflowed {
        RX_OVERFLOW_SINCE.store(0, Ordering::Relaxed);
    }
    interrupts::local_irq_restore(saved);
    // Released on every path: the critical section above is bounded (it ends when the FIFO reads empty),
    // masked, and cannot block, so there is no path that leaves the claim held.
    RX_DRAIN_CLAIMED.store(false, Ordering::Release);

    // Announce a persistently bad RX line, ONCE. Deliberately after the claim is released and IRQs are
    // restored: this logs, and logging goes back out through `pl011_write`.
    if let Some((b, over)) = stuck {
        pl011_write(b"pl011: RX line FAULTY - ");
        timer::write_dec_pub(over);
        pl011_write(b" bytes dropped for lack of ring space (last value ");
        timer::write_dec_pub(b as u32);
        pl011_write(
            b") back to back. This is not a console, and left alone it STARVES the USB keyboard: they               share one input ring, so a line filling it faster than anything drains it means every               keystroke is dropped. Serial RECEIVE is now off (output is unaffected - you are reading               this over it) and the keyboard has the ring to itself. On a Pi this is usually a GPIO HAT               on the UART pins GPIO14/15, or an unconnected/floating RX pin. Reboot to re-enable after               fixing it.
",
        );
    }
    let errs = RX_LINE_ERRORS.load(Ordering::Relaxed);
    if errs >= RX_LINE_ERROR_REPORT_AT && !RX_LINE_ERRORS_REPORTED.swap(true, Ordering::AcqRel) {
        // Report the COUNT, not just the condition: it is the difference between "something is wrong
        // with your serial line" and a number an operator can act on.
        pl011_write(b"pl011: RX line errors - discarded ");
        timer::write_dec_pub(errs);
        pl011_write(
            b" bytes flagged framing/parity/break/overrun. The receive line is held or noisy, so \
              serial INPUT is being dropped as noise; serial OUTPUT is unaffected (you are reading \
              this over it). On a Pi this is usually a GPIO HAT sitting on the UART pins GPIO14/15 - \
              remove it, or use a USB keyboard.\r\n",
        );
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

// The hands-off KEY STORM lived here and has been REMOVED, not pinned.
//
// It typed into the console ring from the timer tick so a wedge could be reproduced by booting
// instead of by hammering a keyboard, and it did its job: the doorbell fix survived ~38,000 injected
// characters where manual typing had wedged the machine in under a minute.
//
// The commandment checker then flagged it as an unpinned kernel feature, and pinning would have been
// the wrong answer. §4.4 forbids developer tooling in the kernel by name, and a keystroke injector is
// exactly that. The right home is userspace: any service holding CONSOLE_PUSH can do the identical
// thing, which is precisely how `dwc2` delivers real keystrokes - so the kernel gains nothing by
// hosting it except a responsibility no pin was watching.
//
// If it is wanted again it should come back as a SERVICE, which would also cover more of the path
// than this did (it would exercise the CONSOLE_PUSH syscall, which injecting kernel-side skipped).

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
        // STAND DOWN when a userspace service owns the controller (Phase 3, Slice 0).
        //
        // These are the in-kernel driver's periodic hooks. The controller has exactly ONE owner: two
        // drivers programming the same channels would corrupt each other's transfers, and the failure
        // would look like flaky hardware rather than two owners. Gating them on the same predicate the
        // IRQ dispatch uses means ownership is decided in one place from one fact.
        if mpidr & 3 == 0 && !irq::usb_owned_by_userspace() {
        }
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
    } else {
        // The ring is full and a KEYSTROKE is being discarded. This used to happen silently, and it is
        // how a Pi 2 became unquittable: a faulty UART line filled the shared ring faster than anything
        // drained it, so every key the user pressed vanished and Ctrl+Q could never arrive. The UART
        // side now shuts itself off before it gets this far, but the ring is shared and this is the
        // point where the USER's input is lost - so say it out loud, once, rather than let a keyboard
        // that appears dead give no account of itself.
        KBD_DROPPED.fetch_add(1, Ordering::Relaxed);
        if !KBD_DROP_REPORTED.swap(true, Ordering::AcqRel) {
            pl011_write(
                b"console: input ring FULL - keystrokes are being dropped. Something is producing                   input faster than it is consumed; a stuck serial RX line is the usual cause.
",
            );
        }
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

/// Task slot parked in a blocking console read, or `u32::MAX` for "nobody". The neutral code both
/// clears it with `u32::MAX` and tests it with `!= u32::MAX`, so that is the sentinel - this was
/// initialised to 0, which reads as "task slot 0 is waiting" and would spuriously wake the supervisor
/// before any reader had ever registered. x86 has always used `u32::MAX`; this is now the same.
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
    /// Counter ticks per 10 ms scheduler quantum, in `read_cycle_counter` (CNTPCT) units.
    ///
    /// Was a `0` stub, and `0` is not merely "unknown" - `scheduler::cycles_to_ticks` reads it as a
    /// signal to fall back to exactly 1 tick, so EVERY `sleep` and `recv_timeout` on this port collapsed
    /// to one quantum regardless of what was asked for. A duration the caller chose, silently replaced.
    ///
    /// Derived from the MEASURED counter rate (`timer_hz`, cross-checked against the Pi's independent
    /// 1 MHz system timer), never `CNTFRQ` - which overstates it by 19.2x on this board and would make
    /// every timeout 19.2x too short. Reads 0 until calibration completes, which keeps the old
    /// one-quantum fallback for exactly that window rather than inventing a rate.
    ///
    /// **Fixing this was never safe on its own.** A cycle count is not a portable duration: userspace
    /// constants were written as x86 cycles at ~2 GHz, so `60_000_000` meaning "~30 ms" becomes ~60
    /// SECONDS here at ~1 MHz. Three paced loops held exactly that value; they now go through the SDK's
    /// `sleep_ms`, which converts via the kernel's own calibration. That is why the two changes land
    /// together - the stub was load-bearing for code that assumed it stayed broken.
    pub fn tsc_ticks_per_quantum() -> u64 {
        // TICK_HZ = 100 (10 ms quantum), so ticks-per-quantum = timer_hz / 100.
        (super::timer::timer_hz() as u64) / 100
    }
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
    /// Ring another core's doorbell (BCM2836 mailbox 0).
    ///
    /// This was an empty stub, and the cost stayed invisible while every service sharing an IPC path
    /// lived on core 0. The scheduler calls this to wake a task blocked on another core, so with
    /// nothing here the target did not notice until its next 10 ms timer tick: a file read is
    /// shell -> fs -> block-driver -> dwc2, which spread across cores is three quanta of pure latency
    /// per operation.
    ///
    /// `lapic_id` IS the core index on this SoC (the neutral layer resolves it from MPIDR). The
    /// vector is deliberately not encoded: every IPI this port sends means "there is work for you
    /// now", and the receiver reschedules.
    pub unsafe fn send_ipi_to_lapic(lapic_id: u32, _vector: u8) {
        super::irq::ring_doorbell(lapic_id);
    }
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

    /// True if EVERY page in `[ptr, ptr+len)` is accessible at PL0 for the requested access. Uses the
    /// CP15 unprivileged-translation probe (`translate_user`, non-faulting: the result lands in PAR.F,
    /// not an exception) under the service's own TTBR0 - the same table the copy will use.
    ///
    /// This is the fix for a userspace-reachable kernel wedge (kernel-audit Audit 5, (C) HIGH): a
    /// range-valid-but-unmapped (or, for a write, read-only) user pointer passed to any copying syscall
    /// would otherwise fault the raw copy below in SVC mode, which the abort handler classifies as a
    /// KERNEL bug and HALTS the core - any service could wedge the kernel with one bad-pointer syscall.
    /// Probing here rejects it with a defined error instead. Sound without a fault-recovery flag because
    /// a task cannot modify its own page tables during its own syscall (no such syscall) and is not
    /// running concurrently on another core, so there is no TOCTOU between probe and copy.
    fn user_range_accessible(ptr: u64, len: usize, write: bool) -> bool {
        if len == 0 { return true; }
        let first = ptr as u32 & !0xFFF;
        let last  = ((ptr as u32).wrapping_add((len - 1) as u32)) & !0xFFF;
        let mut page = first;
        loop {
            if super::usermode::translate_user(page, write).is_none() { return false; }
            if page == last { break; }
            page = page.wrapping_add(0x1000);
        }
        true
    }

    /// Borrow `len` bytes at user VA `ptr` as a slice, after range-checking AND confirming every page is
    /// user-readable. Returns `None` if the range escapes user space or is not fully mapped.
    pub fn read_user_bytes(ptr: u64, len: usize) -> Option<&'static [u8]> {
        if !validate_user_ptr(ptr, len) { return None; }
        if !user_range_accessible(ptr, len, false) { return None; }
        // SAFETY: the range is within user space, fully mapped user-readable (probed above), and the
        // kernel shares the service page table while handling the syscall, so `ptr` is addressable and
        // the copy cannot fault.
        Some(unsafe { core::slice::from_raw_parts(ptr as usize as *const u8, len) })
    }

    /// Copy `len` bytes from a USER range into kernel memory, bounded to ONE PAGE per call.
    ///
    /// The ELF loader uses this to move a service image out of the supervisor's address space a page at
    /// a time, so the kernel never holds more than a page of an untrusted image at once (26.6 - the
    /// bound is visible here rather than left to the caller's discipline). `read_user_bytes` cannot do
    /// this: it is capped at one message and lands in a shared per-core scratch slot.
    ///
    /// Returns false if the range is not valid user memory or `len` exceeds a page.
    pub fn copy_user_to_kernel(src: u64, dst: *mut u8, len: usize) -> bool {
        if len == 0 { return true; }
        if len > crate::arch::imp::page_tables::PAGE_SIZE { return false; }
        if !validate_user_ptr(src, len) { return false; }
        if !user_range_accessible(src, len, false) { return false; }
        // SAFETY: range-checked user VA, probed mapped-readable at PL0; the kernel shares the
        // service page table while handling the syscall, so `src` is addressable and cannot fault.
        unsafe { core::ptr::copy_nonoverlapping(src as usize as *const u8, dst, len); }
        true
    }

    /// Write `src` to user VA `dst`, after range-checking AND confirming every page is user-writable.
    /// Returns false if the range escapes user space or is not fully mapped writable.
    pub fn write_user_bytes(dst: u64, src: &[u8]) -> bool {
        if !validate_user_ptr(dst, src.len()) { return false; }
        if !user_range_accessible(dst, src.len(), true) { return false; }
        // SAFETY: range-checked user VA, probed mapped-writable at PL0 (see read_user_bytes); the copy
        // cannot fault the kernel.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst as usize as *mut u8, src.len()); }
        true
    }
    /// CNTPCT - the ARM generic timer's physical counter, the arm32 analogue of RDTSC.
    pub fn read_cycle_counter() -> u64 { super::timer::cntpct() }
}

// ---------------------------------------------------------------------------
/// No unlocked-fault-write counter on this arch: the x86 fault handlers are the ones that bypass the
/// serial lock (audits/kernel-audit.md Audit 10). Reports 0 rather than pretending to measure
/// something - a zero it HAS earned, because nothing here writes unlocked.
///
/// Lives at the ARCH TOP LEVEL, not inside `mod pci`, because `syscall::dispatch` reaches it as
/// `crate::arch::imp::serial_unlocked_emit_count` - which is where x86 defines it too.
/// THE PI 2 HAS NO PCI AT ALL - no port I/O, and no root complex either; its peripherals hang off a
/// memory-mapped bus and are reached through an MMIO grant. So this is not "unimplemented", it is
/// absent by construction: the gated `PciCfgRead` syscall exists on every arch for one dispatch
/// table, and on this one it can only REFUSE - which the caller is told, rather than being handed a
/// plausible zero it would read as an empty bus.
///
/// Safe, and performs no I/O - there is none to perform.
pub fn pci_cfg_read32(_sel: u32, _off: u16) -> Option<u32> { None }

/// Who made this CPU, and which one - written into a caller-supplied buffer, returning its length.
///
/// **A log has to say which MACHINE it came from.** The boot banner reports the ARCH, and an arch name
/// is not a board: this project runs two ARM boards whose behaviour has already diverged in ways that
/// mattered (one has PCIe and an SMMU-less DMA posture, the other has no PCI at all), and reading a
/// serial log while having to ASK which produced it is how a fact about one board becomes a claim
/// about the port.
///
/// `MIDR` carries the implementer in bits [31:24] and the part number in [15:4]. Only the parts this
/// project actually runs on are named; anything else prints its raw ids rather than a guess, because a
/// wrong name is worse than a number a reader can look up.
pub fn cpu_identity(buf: &mut [u8]) -> usize {
    let midr: u64;
    // SAFETY: reading MIDR is a side-effect-free system-register read, available at this exception level.
    unsafe { { let v: u32; core::arch::asm!("mrc p15, 0, {}, c0, c0, 0", out(reg) v, options(nomem, nostack)); midr = v as u64; } };
    let implementer = ((midr >> 24) & 0xFF) as u32;
    let part        = ((midr >> 4) & 0xFFF) as u32;

    let name: &[u8] = match (implementer, part) {
        (0x41, 0xD08) => b"ARM Cortex-A72",       // Raspberry Pi 4 (BCM2711)
        (0x41, 0xC07) => b"ARM Cortex-A7",        // Raspberry Pi 2 (BCM2836)
        (0x41, 0xD03) => b"ARM Cortex-A53",
        (0x41, 0xD0B) => b"ARM Cortex-A76",
        (0x42, _)     => b"Broadcom",
        (0x41, _)     => b"ARM",
        _             => b"",
    };

    let mut n = 0usize;
    for &b in name { if n < buf.len() { buf[n] = b; n += 1; } }
    // Always append the raw ids. A named part still benefits from them (two boards can share a core),
    // and an unnamed one has nothing else to go on.
    for &b in b" (impl 0x" { if n < buf.len() { buf[n] = b; n += 1; } }
    n = push_hex(buf, n, implementer);
    for &b in b" part 0x" { if n < buf.len() { buf[n] = b; n += 1; } }
    n = push_hex(buf, n, part);
    if n < buf.len() { buf[n] = b')'; n += 1; }
    n
}

/// Lower-case hex, no padding - enough for the two small ids above.
fn push_hex(buf: &mut [u8], mut n: usize, v: u32) -> usize {
    let mut started = false;
    for shift in (0..8).rev() {
        let nib = ((v >> (shift * 4)) & 0xF) as u8;
        if nib == 0 && !started && shift != 0 { continue; }
        started = true;
        let c = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 };
        if n < buf.len() { buf[n] = c; n += 1; }
    }
    n
}


pub fn serial_unlocked_emit_count() -> u64 { 0 }

pub mod interrupts {
    /// The MSI vector pool is x86-only (`arch/x86_64/interrupts.rs`, step D1b). Neither Pi has one:
    /// a pool hands vectors to devices found on a PCI bus, and there is no PCI bus here to find them
    /// on - `pci::find_by_class` returns `None`, so `task::pci_msi_vector` returns before it ever
    /// consults these. LEN 0 states that plainly ("the pool holds nothing") rather than naming a
    /// range of vectors this arch does not route.
    pub const MSI_POOL_BASE: u8 = 0;
    pub const MSI_POOL_LEN: usize = 0;
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
        // Watch the hub's ports so a replugged keyboard works without a reboot.
        //
        // This path is NOT atomic - the scheduler re-enables interrupts after every switch-back, so code
        // here is interruptible and preemptible. That was fatal for the first version of this, which
        // assumed otherwise: the timer's keyboard poll, the USB IRQ's net-RX re-arm and a preempting
        // storage command all rewrite the shared device selection a control transfer depends on.
        //
        // Masking here would be worse, not better: an enumeration runs ~100 ms, and suppressing the tick
        // that long stops core 0's liveness stamp and lets another core panic the machine. So the
        // exclusion is a PROTOCOL instead - `dwc2::hotplug_poll` takes `UsbExclusive` and every other
        // shared-selection path stands aside for the duration (storage answers BUSY and re-asks, which it
        // already knows how to do). Interrupts stay on, the tick keeps running, and nothing races.
        // NOTE: handing the vector back does NOT restore USB. Reboot to get the devices back.
        //
        // Releasing the route on death unmasks the line, but it cannot hand back DEVICE
        // STATE: while the service held the vector, the keyboard's completions went to a driver that
        // ignores them, so the in-kernel driver's channel sits mid-transfer waiting on an event it
        // never saw resolve, and the periodic hooks resume polling a channel that is already stuck.
        //
        // A re-init on the ownership edge was tried here and REMOVED. It never fired: this is
        // `wait_for_interrupt`, the idle primitive, and core 0 does not reach idle in that window. The
        // fix would be real work - somewhere that reliably runs, plus a ~600 ms bring-up that cannot
        // happen in a tick handler - spent on a recovery path for a driver that Slice 5 deletes.
        // Rebooting costs twenty seconds and no code. Left as a documented limitation rather than
        // dead code implying a capability that does not exist.
        // Both of these stand down for a userspace owner, for the reason above: one controller, one
        // driver. `hotplug_poll` in particular takes the exclusive bulk claim and rewrites the shared
        // device selection, which is precisely what must not happen underneath another driver.
        if !super::irq::usb_owned_by_userspace() {
        // Watch the ethernet cable for the same reason, on the same terms. The PHY read was already
        // written and already correct - but nothing CALLED it unless a service asked (`net`, `ping`), so
        // unplugging the cable on an idle machine was silent while unplugging the keyboard was not.
        // Polling it here makes the cable report itself live, like every other plug. It is a separate call
        // rather than folded into `hotplug_poll` because it must run OUTSIDE that function's exclusive
        // section: it takes the same bulk claim, and nesting would make it stand aside from itself. Both
        // are individually rate-limited to ~1 s and both yield to storage, so idle stays cheap.
        }
        // SAFETY: unmasking IRQs is always valid (vectors + handlers installed); WFI then waits for one.
        unsafe { core::arch::asm!("cpsie i", "wfi", options(nomem, nostack)) }
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
    pub fn idle_mask_before_halt() -> bool { false }

    pub fn idle_can_halt() -> bool { true } // ARM WFI wakes on the generic-timer IRQ; halting is safe
    pub fn send_eoi() {}                    // BCM2836 timer has no separate EOI (TVAL re-arm clears it)
    pub fn fire_test_irq(_irq: u8) {}
}

// ---------------------------------------------------------------------------
// The neutral context-switch surface is now a REAL implementation (`context_switch.rs`), not a stub:
// TaskContext + new_kernel/new_user + switch_context that the arch-neutral scheduler drives directly.

// ---------------------------------------------------------------------------
pub mod rtc {
    use core::sync::atomic::{AtomicI64, Ordering};
    pub use crate::clock::epoch_secs;
    /// The wall clock the Pi 2 lacks: set from the network (SNTP) via `set_wall_clock`. 0 = not yet set.
    /// Stored as the epoch at boot (monotonic == 0), so `read_datetime` = base + seconds-since-boot.
    static WALL_EPOCH_BASE: AtomicI64 = AtomicI64::new(0);
    pub fn capture_boot_time() {}
    pub fn boot_datetime() -> u64 { 0 }
    /// The packed wall-clock datetime (query 11). 0 until the wall clock is set from the network (there is
    /// no hardware RTC), then the real time = the SNTP base + monotonic seconds since boot.
    pub fn read_datetime() -> u64 {
        let base = WALL_EPOCH_BASE.load(Ordering::Relaxed);
        if base == 0 { return 0; }                                // no wall clock yet - `date` shows zeros
        crate::clock::packed_from_epoch(base + now_epoch_monotonic())
    }
    /// Set the wall clock: `epoch` is the real Unix time NOW (from SNTP). Store base = epoch - monotonic so
    /// every later `read_datetime` reconstructs the current time from the monotonic counter. The SetClock
    /// syscall calls this; it is cap-gated (SET_CLOCK), never ambient.
    pub fn set_wall_clock(epoch: i64) -> bool {
        WALL_EPOCH_BASE.store(epoch - now_epoch_monotonic(), Ordering::Relaxed);
        true                      // this board has no RTC, so a network time IS the authority here
    }
    /// Monotonic seconds since boot, from the generic timer (the Pi 2 has no wall-clock RTC, so this is
    /// NOT a real epoch - but it advances, which is all its callers need: bounding deadline waits and
    /// measuring TSC Hz. A `0` stub here made `calibrate_tsc_hz` spin ~100M yields and every
    /// deadline-based wait never expire, hanging net-stack before its serve loop.
    pub fn now_epoch_monotonic() -> i64 {
        let hz = super::timer::timer_hz() as u64;
        // Generic timer dead (CNTFRQ selftest failed): fall back to the 1 MHz System Timer so the clock
        // still ADVANCES rather than freezing at 0 (which would make a time-bounded wait never fire -
        // kernel-audit Audit 6, N2). Not reachable on QEMU/real Pi 2 (both set TIMER_HZ).
        if hz == 0 { return super::timer::systimer_secs(); }
        // Elapsed since the boot baseline, so this is seconds-since-boot on every board - not the raw
        // counter, which QEMU seeds at a large value (real HW starts near 0). See timer::BOOT_CNTPCT.
        (super::timer::cntpct().saturating_sub(super::timer::boot_cntpct()) / hz) as i64
    }
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
/// The neutral "mask/unmask an interrupt source by vector" seam.
///
/// Named `ioapic` because x86 named it that; on this board it is the BCM2835 legacy interrupt
/// controller. These were no-op stubs, which was harmless while every device interrupt was serviced
/// inside the kernel - nothing ever needed a line held off. Routing an interrupt to USERSPACE makes
/// them load-bearing: a level-triggered line that is not masked on delivery re-asserts the instant
/// the handler returns, and the core makes no further progress.
pub mod ioapic {
    pub fn init() {}
    /// Hold off the source behind `vector` until the userspace driver acknowledges its device.
    pub fn mask_vector(vector: u8) {
        if vector == super::irq::USB_VECTOR {
            super::irq::mask_usb_irq();
        }
        // Any other vector has no arm32 source behind it yet. Silently ignoring an unknown vector is
        // right HERE - this is the generic router asking about a line this board may simply not have
        // - and it is not a silent failure, because a device whose IRQ is never routed never reaches
        // this call at all.
    }
    /// Let the source behind `vector` fire again. Reached from the `IrqUnmask` syscall.
    pub fn unmask_vector(vector: u8) {
        if vector == super::irq::USB_VECTOR {
            super::irq::unmask_usb_irq();
        }
    }
}

// ---------------------------------------------------------------------------
pub mod ap_boot {
    pub unsafe fn start_all_aps(boot_info: &super::BootInfo) -> u32 { 0 }
}

/// Must a driver's DMA arena be mapped UNCACHED on this architecture?
///
/// ARMv7 DMA is not coherent either - same reasoning as the 64-bit port.
pub const DMA_ARENA_UNCACHED: bool = true;

/// Where a driver's DMA arena is mapped in ITS address space.
///
/// Per-arch because it is an ADDRESS, and an address is only meaningful in an address space that can
/// hold it. The shared constant was `0x2_0000_0000`, an x86_64 value: on ARMv7 that is above the 32-bit
/// ceiling, and the mapper truncated it to 0, laying the arena over the kernel's low megabyte.
pub const DRIVER_DMA_VA: u64 = 0x7000_0000;
