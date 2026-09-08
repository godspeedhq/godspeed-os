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
/// The system clock/reset generator, and the video-out one. Both from the tree.
static SYSCRG_BASE: AtomicU64 = AtomicU64::new(0);
static VOUTCRG_BASE: AtomicU64 = AtomicU64::new(0);

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

pub(super) fn set_crg_bases(syscrg: u64, voutcrg: u64) {
    SYSCRG_BASE.store(syscrg, Ordering::Relaxed);
    VOUTCRG_BASE.store(voutcrg, Ordering::Relaxed);
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

// ============================ stage two: clocks and resets ============================
//
// Facts from Linux, read as an executable datasheet (§26.14): `clk-starfive-jh71x0.h` for the
// register shape, `starfive,jh7110-crg.h` for the indices, and `reset-starfive-jh7110.c` for where
// each generator keeps its reset registers. All of it is what the silicon requires. What is not
// borrowed is the shape - Linux models this as a clock provider with parents, muxes and dividers
// and a reset controller behind a framework; here it is "turn on the handful the display needs, in
// the one order that works", because the kernel either gets a picture or it does not.

/// One 32-bit register per clock, at `base + index * 4`, and bit 31 enables it.
const CLK_ENABLE: u32 = 1 << 31;

/// Reset registers: id `n` is bit `n % 32` of the word at `offset + (n / 32) * 4`, and DEASSERTING
/// is clearing that bit, then waiting for the matching status bit to follow.
const SYSCRG_RESET_ASSERT: usize = 0x2f8;
const SYSCRG_RESET_STATUS: usize = 0x308;
const VOUTCRG_RESET_ASSERT: usize = 0x48;
const VOUTCRG_RESET_STATUS: usize = 0x4c;

/// The system clocks the video-out block hangs off.
///
/// **Exactly the four the DEVICE TREE names, and no more.** The first version also enabled index 59
/// (`VOUT_AXI`), which exists in the SoC's clock list and is not among the ones the display node asks
/// for - and the board said so on the first boot: `system clock did not enable: vout_axi`, before
/// anything else could be learned. The header tells you which clocks the CHIP has; only the device
/// tree says which ones THIS block needs, and a plausible extra turned a working stage into a
/// failing one.
const SYSCLK_VOUT_SRC: usize = 58;
const SYSCLK_NOC_BUS_DISP_AXI: usize = 60;
const SYSCLK_VOUT_TOP_AHB: usize = 61;
const SYSCLK_VOUT_TOP_AXI: usize = 62;

/// System resets the device tree names for the display: `rst_vout_src` and `rst_noc_disp`.
const SYSRST_VOUT_SRC: u32 = 0x2b;
const SYSRST_NOC_DISP: u32 = 0x1a;

/// Video-out clocks. The display controller needs its four; the HDMI transmitter needs its three.
const VOUTCLK_APB: usize = 0;
const VOUTCLK_DC8200_PIX: usize = 1;
const VOUTCLK_DC8200_AXI: usize = 4;
const VOUTCLK_DC8200_CORE: usize = 5;
const VOUTCLK_DC8200_AHB: usize = 6;
const VOUTCLK_DC8200_PIX0: usize = 7;
const VOUTCLK_DC8200_PIX1: usize = 8;
const VOUTCLK_DOM_VOUT_TOP_LCD: usize = 9;
const VOUTCLK_HDMI_TX_MCLK: usize = 15;
const VOUTCLK_HDMI_TX_BCLK: usize = 16;
const VOUTCLK_HDMI_TX_SYS: usize = 17;

/// Video-out resets, which the device tree names `rst_axi`, `rst_ahb` and `rst_core`.
const VOUTRST_AXI: u32 = 0;
const VOUTRST_AHB: u32 = 1;
const VOUTRST_CORE: u32 = 2;

fn mmio_read(base: u64, off: usize) -> u32 {
    // SAFETY: a register inside a block the device tree described, in the identity map, whose power
    // domain and parent clocks the caller has already brought up - which is the ordering this whole
    // file exists to get right.
    unsafe { ((base as usize + off) as *const u32).read_volatile() }
}

fn mmio_write(base: u64, off: usize, val: u32) {
    // SAFETY: as above.
    unsafe { ((base as usize + off) as *mut u32).write_volatile(val) };
}

/// Turn one clock on, and report whether the enable bit stuck.
///
/// READ BACK, because a write into an unpowered or unclocked block is not an error - it is silence,
/// and the next stage would then fail somewhere else entirely.
fn clk_enable(base: u64, index: usize) -> bool {
    let off = index * 4;
    let v = mmio_read(base, off);
    mmio_write(base, off, v | CLK_ENABLE);
    mmio_read(base, off) & CLK_ENABLE != 0
}

/// Take one reset off, and wait - bounded - for the hardware to say so.
///
/// **Bounded because the reference driver says this can hang forever**: deasserting a reset whose
/// clock is still gated never completes. That is the whole reason clocks are enabled before this is
/// called, and the bound is what turns a mistake in that ordering into a reported failure instead of
/// a dead machine with no output.
fn reset_deassert(base: u64, assert_off: usize, status_off: usize, id: u32) -> bool {
    let word = (id / 32) as usize * 4;
    let mask = 1u32 << (id % 32);
    let v = mmio_read(base, assert_off + word);
    mmio_write(base, assert_off + word, v & !mask);

    let hz = super::timebase_hz() as u64;
    let deadline = super::sbi::time().wrapping_add(if hz == 0 { 100_000 } else { hz / 100 });
    while super::sbi::time() < deadline {
        if mmio_read(base, status_off + word) & mask == 0 {
            return true;
        }
    }
    false
}

/// Bring the display block's clocks and resets up, in the only order that works.
///
/// The order is not a preference. The video-out generator's own registers live INSIDE the block its
/// parent clocks feed, so they cannot be touched until the system generator has released them; and a
/// reset cannot be deasserted until its clock runs. So: system clocks, system resets, then the
/// video-out generator's clocks, then its resets. Each stage prints before it acts, so if the machine
/// stops, the last line names the stage that did it.
pub fn clocks_on() -> bool {
    let sys = SYSCRG_BASE.load(Ordering::Relaxed);
    let vout = VOUTCRG_BASE.load(Ordering::Relaxed);
    if sys == 0 || vout == 0 {
        super::print_str("riscv64: display - no syscrg/voutcrg in the device tree\n");
        return false;
    }

    // TRY THEM ALL, THEN DECIDE. Stopping at the first failure costs a whole board boot to learn
    // about one clock, and a boot is the expensive thing here - so every result is reported and
    // the verdict comes after. An enable is a write and a read-back; once the domain is powered,
    // attempting the rest costs nothing and tells us everything.
    super::print_str("riscv64: display - system clocks:");
    let mut ok = true;
    for (i, name) in [
        (SYSCLK_VOUT_SRC, "vout_src"),
        (SYSCLK_NOC_BUS_DISP_AXI, "noc_disp"),
        (SYSCLK_VOUT_TOP_AHB, "vout_ahb"),
        (SYSCLK_VOUT_TOP_AXI, "vout_top_axi"),
    ] {
        super::print_str(" ");
        super::print_str(name);
        if clk_enable(sys, i) {
            super::print_str("=on");
        } else {
            super::print_str("=FAIL");
            ok = false;
        }
    }
    super::print_str("\n");
    if !ok {
        return false;
    }

    super::print_str("riscv64: display - releasing system resets\n");
    for (id, name) in [(SYSRST_VOUT_SRC, "vout_src"), (SYSRST_NOC_DISP, "noc_disp")] {
        if !reset_deassert(sys, SYSCRG_RESET_ASSERT, SYSCRG_RESET_STATUS, id) {
            super::print_str("riscv64: display - system reset did not release: ");
            super::print_str(name);
            super::print_str("\n");
            return false;
        }
    }

    // Only NOW is the video-out generator reachable: its registers are inside the block the clocks
    // and resets above just brought up.
    super::print_str("riscv64: display - video-out clocks:");
    let mut enabled = 0;
    for (i, name) in [
        (VOUTCLK_APB, "apb"),
        (VOUTCLK_DC8200_PIX, "pix"),
        (VOUTCLK_DC8200_AXI, "axi"),
        (VOUTCLK_DC8200_CORE, "core"),
        (VOUTCLK_DC8200_AHB, "ahb"),
        (VOUTCLK_DC8200_PIX0, "pix0"),
        (VOUTCLK_DC8200_PIX1, "pix1"),
        (VOUTCLK_DOM_VOUT_TOP_LCD, "lcd"),
        (VOUTCLK_HDMI_TX_MCLK, "hdmi_mclk"),
        (VOUTCLK_HDMI_TX_BCLK, "hdmi_bclk"),
        (VOUTCLK_HDMI_TX_SYS, "hdmi_sys"),
    ] {
        super::print_str(" ");
        super::print_str(name);
        if clk_enable(vout, i) {
            super::print_str("=on");
            enabled += 1;
        } else {
            super::print_str("=FAIL");
        }
    }
    super::print_str("\n");
    if enabled == 0 {
        // Not one enable bit stuck. The block is not answering, which means it is not really powered
        // or not really clocked - and going on to write display timings into it would be writing
        // into nothing.
        super::print_str("riscv64: display - the video-out block is not answering; stopping here\n");
        return false;
    }

    super::print_str("riscv64: display - releasing video-out resets\n");
    for (id, name) in [(VOUTRST_AXI, "axi"), (VOUTRST_AHB, "ahb"), (VOUTRST_CORE, "core")] {
        if !reset_deassert(vout, VOUTCRG_RESET_ASSERT, VOUTCRG_RESET_STATUS, id) {
            super::print_str("riscv64: display - video-out reset did not release: ");
            super::print_str(name);
            super::print_str("\n");
            return false;
        }
    }

    super::print_str("riscv64: display - VOUT powered, clocked and out of reset\n");
    true
}
