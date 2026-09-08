// SPDX-License-Identifier: GPL-2.0-only
//! Display bring-up on the JH7110 - starting with the power domain everything else needs.
//!
//! **The bootloader leaves the display switched off.** U-Boot on this board reports `In: serial /
//! Out: serial` - it has no video device at all - so unlike the Raspberry Pi ports, where the
//! firmware hands over a live framebuffer and the kernel only has to draw into it, here every stage
//! is ours: the power domain, the clocks, the resets, the display controller, and the HDMI PHY.
//!
//! This file is the first stage, and it is deliberately the smallest one that can fail. The VOUT
//! block sits in its own POWER DOMAIN, and reading a register in an unpowered domain is not a zero -
//! it is a bus transaction with nothing to answer it, which on this interconnect either faults or
//! never completes. So nothing here touches the display controller. It talks only to the PMU, which
//! lives in the always-on block at 0x1703_0000 and is therefore safe to read at any time, and asks
//! it one question: is VOUT powered, and can we power it?
//!
//! Everything after this - the clock tree in `voutcrg`, the DC8200 timings, the Inno HDMI PHY - is
//! reachable only once the answer is yes, which is why it is worth establishing on its own.
//!
//! Register facts are from Linux's `drivers/pmdomain/starfive/jh71xx-pmu.c`, read as an executable
//! datasheet (§26.14): the offsets, the encourage sequence and the domain bits are the SILICON's
//! requirements. What is not borrowed is how the driver is shaped - Linux models this as a generic
//! power-domain provider with runtime PM; here it is a handful of writes at boot, because the kernel
//! either brings the display up or it does not.

use core::sync::atomic::{AtomicU64, Ordering};

/// Where the PMU is, from the device tree. Zero until the boot finds it.
static PMU_BASE: AtomicU64 = AtomicU64::new(0);

/// Registers, from `jh71xx-pmu.c`.
const SW_TURN_ON_POWER: usize = 0x0c;
const SW_ENCOURAGE: usize = 0x44;
const CURR_POWER_MODE: usize = 0x80;

/// The "encourage" sequence: a fixed three-write knock that must follow the request, and exists so a
/// stray single write cannot switch a power domain by accident. Borrowed exactly, because it is a
/// property of the hardware and there is nothing to reason about - the values mean nothing except to
/// the PMU.
const ENCOURAGE_ON: u32 = 0xff;
const ENCOURAGE_EN_LO: u32 = 0x05;
const ENCOURAGE_EN_HI: u32 = 0x50;

/// Domain bits in `CURR_POWER_MODE`.
const DOMAIN_SYSTOP: u32 = 1 << 0;
const DOMAIN_CPU: u32 = 1 << 1;
const DOMAIN_GPUA: u32 = 1 << 2;
const DOMAIN_VDEC: u32 = 1 << 3;
const DOMAIN_VOUT: u32 = 1 << 4;
const DOMAIN_ISP: u32 = 1 << 5;
const DOMAIN_VENC: u32 = 1 << 6;

/// Tell this module where the PMU is. Called from the boot with what the device tree said.
pub(super) fn set_pmu_base(base: u64) {
    PMU_BASE.store(base, Ordering::Relaxed);
}

fn read(off: usize) -> Option<u32> {
    let base = PMU_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    // SAFETY: the PMU is in the ALWAYS-ON block, so this read is answered whatever else on the SoC
    // is powered down - which is the entire reason this module starts here rather than at the
    // display controller. The address came from the device tree and is inside the identity map.
    Some(unsafe { ((base as usize + off) as *const u32).read_volatile() })
}

fn write(off: usize, val: u32) {
    let base = PMU_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    // SAFETY: as above. The only registers written are the power request and its encourage
    // sequence, which is the PMU's documented interface for exactly this.
    unsafe { ((base as usize + off) as *mut u32).write_volatile(val) };
}

/// Print which power domains are currently on.
fn report(mode: u32) {
    for (bit, name) in [
        (DOMAIN_SYSTOP, "systop"),
        (DOMAIN_CPU, "cpu"),
        (DOMAIN_GPUA, "gpu"),
        (DOMAIN_VDEC, "vdec"),
        (DOMAIN_VOUT, "vout"),
        (DOMAIN_ISP, "isp"),
        (DOMAIN_VENC, "venc"),
    ] {
        if mode & bit != 0 {
            super::print_str(" ");
            super::print_str(name);
        }
    }
}

/// Turn the VOUT power domain on, and say whether it worked.
///
/// The bring-up cannot begin without this: the display controller, the video-out clock controller
/// and the HDMI transmitter are all inside VOUT, and a register read there while it is unpowered is
/// a transaction with nothing to answer it rather than a zero.
///
/// Verified by READ-BACK, not by the writes returning. A power request is asynchronous - the PMU
/// runs a sequence and reports the result in `CURR_POWER_MODE` - so "we wrote the magic values" is
/// not evidence of anything. The poll is bounded: a domain that never comes up is reported, which is
/// a fact the next stage needs, rather than a hang with no output.
pub fn power_on_vout() -> bool {
    let Some(mode) = read(CURR_POWER_MODE) else {
        super::print_str("riscv64: display - no PMU in the device tree; the display cannot be powered\n");
        return false;
    };

    super::print_str("riscv64: power domains on:");
    report(mode);
    super::print_str("\n");

    if mode & DOMAIN_VOUT != 0 {
        super::print_str("riscv64: display - VOUT already powered\n");
        return true;
    }

    write(SW_TURN_ON_POWER, DOMAIN_VOUT);
    write(SW_ENCOURAGE, ENCOURAGE_ON);
    write(SW_ENCOURAGE, ENCOURAGE_EN_LO);
    write(SW_ENCOURAGE, ENCOURAGE_EN_HI);

    // Bounded wait on the machine's own truth (the mode register), not on a delay. About a tenth of
    // a second at either machine's timebase, which is far longer than a domain takes and short
    // enough that a failure is reported rather than waited out.
    let hz = super::timebase_hz() as u64;
    let deadline = super::sbi::time().wrapping_add(if hz == 0 { 1_000_000 } else { hz / 10 });
    while super::sbi::time() < deadline {
        if let Some(m) = read(CURR_POWER_MODE) {
            if m & DOMAIN_VOUT != 0 {
                super::print_str("riscv64: display - VOUT powered on, domains now:");
                report(m);
                super::print_str("\n");
                return true;
            }
        }
    }

    super::print_str("riscv64: display - VOUT did NOT power on; the display stays dark\n");
    false
}
