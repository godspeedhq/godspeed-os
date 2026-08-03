// SPDX-License-Identifier: GPL-2.0-only
//! GIC-400 (GICv2) for the Raspberry Pi 4 (BCM2711).
//!
//! **This is the part of the Pi 4 that is easier than the Pi 2, not harder.** The older board has a
//! bespoke Broadcom interrupt controller that exists nowhere else; the Pi 4 has a standard ARM GICv2,
//! so this is spec-driven code rather than reverse-engineering, and it transfers to any other GICv2
//! board.
//!
//! Two register blocks:
//!
//! - **Distributor** (`GICD`, +0x1000): global. Which interrupts exist, their priority, routing, and
//!   enable state.
//! - **CPU interface** (`GICC`, +0x2000): per-core. Acknowledge, priority mask, end-of-interrupt.
//!
//! Interrupt IDs on a GICv2 are banked: 0-15 are SGIs (software-generated, for IPIs later), 16-31 are
//! PPIs (private per core - the generic timer lives here), 32+ are SPIs (shared peripherals).

/// GIC-400 base on BCM2711.
const GIC_BASE: usize = 0xFF84_0000;
const GICD: usize = GIC_BASE + 0x1000;
const GICC: usize = GIC_BASE + 0x2000;

// Distributor registers.
const GICD_CTLR: *mut u32 = GICD as *mut u32;
const GICD_ISENABLER: *mut u32 = (GICD + 0x100) as *mut u32; // 1 bit per interrupt
const GICD_IPRIORITYR: *mut u8 = (GICD + 0x400) as *mut u8; // 1 byte per interrupt
const GICD_ITARGETSR: *mut u8 = (GICD + 0x800) as *mut u8; // 1 byte per interrupt (CPU mask)

// CPU interface registers.
const GICC_CTLR: *mut u32 = GICC as *mut u32;
const GICC_PMR: *mut u32 = (GICC + 0x004) as *mut u32; // priority mask
const GICC_IAR: *mut u32 = (GICC + 0x00C) as *mut u32; // interrupt acknowledge
const GICC_EOIR: *mut u32 = (GICC + 0x010) as *mut u32; // end of interrupt

/// The acknowledge register returns this when there is nothing pending. A handler that treats it as a
/// real interrupt would EOI an ID that was never raised, which corrupts the controller's state.
pub const SPURIOUS: u32 = 1023;

/// Bring up the distributor and this core's CPU interface.
pub fn init() {
    // SAFETY: GIC-400 registers, mapped Device-nGnRnE by the MMU. Single-threaded boot, so no other
    // core is touching the distributor.
    unsafe {
        GICD_CTLR.write_volatile(0); // quiesce while we configure
        GICC_CTLR.write_volatile(0);

        // Accept every priority. A mask of 0xFF means "do not filter", which is what we want while
        // there is exactly one interrupt source; priority becomes interesting when there are several.
        GICC_PMR.write_volatile(0xFF);

        GICD_CTLR.write_volatile(1); // enable the distributor
        GICC_CTLR.write_volatile(1); // enable this core's CPU interface
    }
}

/// Enable one interrupt ID, at a middling priority, targeted at core 0.
///
/// Priority and target are only meaningful for SPIs (ID >= 32): PPIs and SGIs are banked per core, so
/// their target register is read-only and writing it is harmless but pointless. Setting both
/// unconditionally keeps the function honest for the SPI case that arrives with the first real
/// peripheral driver.
pub fn enable(id: u32) {
    // SAFETY: GIC-400 registers, Device-mapped. `id` indexes byte/bit arrays the controller defines to
    // be at least this large for any valid interrupt ID.
    unsafe {
        GICD_IPRIORITYR.add(id as usize).write_volatile(0xA0);
        if id >= 32 {
            GICD_ITARGETSR.add(id as usize).write_volatile(0x01); // CPU 0
        }
        let reg = (id / 32) as usize;
        let bit = id % 32;
        GICD_ISENABLER.add(reg).write_volatile(1 << bit);
    }
}

/// Acknowledge the pending interrupt, returning its ID.
///
/// The returned value must be handed back to [`eoi`] even if the handler ignores it - the CPU
/// interface tracks an active priority per acknowledged interrupt, and skipping the EOI leaves that
/// priority active forever, silently blocking every later interrupt of equal or lower priority. That
/// failure looks like "interrupts stopped happening" with nothing to point at.
pub fn acknowledge() -> u32 {
    // SAFETY: GICC_IAR is a Device-mapped read with the architectural side effect of acknowledging.
    unsafe { GICC_IAR.read_volatile() & 0x3FF }
}

/// Signal end-of-interrupt for an ID previously returned by [`acknowledge`].
pub fn eoi(id: u32) {
    // SAFETY: GICC_EOIR is Device-mapped; writing an acknowledged ID retires it.
    unsafe { GICC_EOIR.write_volatile(id) };
}
