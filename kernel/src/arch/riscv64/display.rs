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
/// The system controller that holds the PLL registers, and the display sub-system controller. Read
/// only, and only by the diagnostic at the end of this file.
static SYSCON_BASE: AtomicU64 = AtomicU64::new(0);
static DSSCTRL_BASE: AtomicU64 = AtomicU64::new(0);
/// The Inno HDMI transmitter.
static HDMI_BASE: AtomicU64 = AtomicU64::new(0);

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

pub(super) fn set_hdmi_base(base: u64) {
    HDMI_BASE.store(base, Ordering::Relaxed);
}

pub(super) fn set_syscon_bases(syscon: u64, dssctrl: u64) {
    SYSCON_BASE.store(syscon, Ordering::Relaxed);
    DSSCTRL_BASE.store(dssctrl, Ordering::Relaxed);
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
/// **These two are DIVIDERS, not gates.** `JH71X0__DIV(APB, "apb", 8, ...)` and
/// `JH71X0__DIV(DC8200_PIX, "dc8200_pix", 63, ...)`: their registers hold a divisor in the low bits
/// and have no enable bit at all, so writing bit 31 and reading it back reports nothing and always
/// would have. The board called them `apb=FAIL pix=FAIL` while the other nine came up, which is the
/// clock table saying "you have asked a divider whether it is switched on".
///
/// They are not cosmetic. `dc8200_pix` divides `vout_src` down to the PIXEL CLOCK, and the mode this
/// display eventually runs at is that divisor - so what they hold is read and reported here, and
/// setting them belongs with the stage that chooses a mode.
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
    let before = mmio_read(base, assert_off + word);
    mmio_write(base, assert_off + word, before & !mask);
    let after = mmio_read(base, assert_off + word);

    let hz = super::timebase_hz() as u64;
    let deadline = super::sbi::time().wrapping_add(if hz == 0 { 100_000 } else { hz / 100 });
    // DEASSERTED IS THE BIT SET, not clear. The reference driver computes its completion value as
    // `done = 0` and then, for a deassert, `done ^= mask` - so it waits for the status bit to be
    // ONE. Reading it as zero-means-released inverts the test, and the board said so precisely:
    // `assert 0xe7e7fe00->0xe7e7f600 status 0x180009ff` - the write had landed, the status bit was
    // set, and both resets had in fact released while this waited out its bound calling them stuck.
    //
    // The status words being mostly ONES is the same fact from the other side: nearly everything on
    // a running SoC is out of reset, which is impossible to read as "asserted" once you notice the
    // machine is running.
    let mut status = 0u32;
    while super::sbi::time() < deadline {
        status = mmio_read(base, status_off + word);
        if status & mask == mask {
            return true;
        }
    }

    // SAY WHAT THE HARDWARE HELD, not just that the wait expired. "Did not release" is a symptom
    // with several causes that look identical from here - the write not landing, the bit being the
    // wrong one, the status having the opposite polarity, the block being unclocked - and the three
    // register values separate them in one line. Guessing between them costs a board boot each time.
    super::print_str(" [id ");
    super::print_dec(id as u64);
    super::print_str(" mask ");
    super::print_hex(mask as u64);
    super::print_str(" assert ");
    super::print_hex(before as u64);
    super::print_str("->");
    super::print_hex(after as u64);
    super::print_str(" status ");
    super::print_hex(status as u64);
    super::print_str("]");
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

    // BOTH, then decide - same reason as the clocks. Both live in the system generator, which is
    // outside the VOUT domain and answering, so attempting the second after the first fails costs
    // nothing and doubles what one boot tells us.
    super::print_str("riscv64: display - system resets:");
    let mut ok = true;
    for (id, name) in [(SYSRST_VOUT_SRC, "vout_src"), (SYSRST_NOC_DISP, "noc_disp")] {
        super::print_str(" ");
        super::print_str(name);
        if reset_deassert(sys, SYSCRG_RESET_ASSERT, SYSCRG_RESET_STATUS, id) {
            super::print_str("=released");
        } else {
            super::print_str("=STUCK");
            ok = false;
        }
    }
    super::print_str("\n");
    if !ok {
        // Do NOT go on to touch the video-out generator. Its registers are behind the reset that
        // did not release, and a read there is a transaction with nothing to answer it.
        super::print_str("riscv64: display - stopping: the video-out block is still held in reset\n");
        return false;
    }

    // Only NOW is the video-out generator reachable: its registers are inside the block the clocks
    // and resets above just brought up.
    super::print_str("riscv64: display - video-out gates:");
    let mut enabled = 0;
    for (i, name) in [
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

    // The dividers, READ rather than enabled - what they already hold is what the pixel clock
    // currently is, and that is the number the mode stage has to work from.
    super::print_str("riscv64: display - dividers: apb=");
    super::print_hex(mmio_read(vout, VOUTCLK_APB * 4) as u64);
    super::print_str(" dc8200_pix=");
    super::print_hex(mmio_read(vout, VOUTCLK_DC8200_PIX * 4) as u64);
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

// ============================ stage three: does the DC8200 answer? ============================

/// Where the display controller's two register windows are, from the device tree.
static DC_BASE: AtomicU64 = AtomicU64::new(0);
static DC_REGS: AtomicU64 = AtomicU64::new(0);

pub(super) fn set_dc_bases(top: u64, regs: u64) {
    DC_BASE.store(top, Ordering::Relaxed);
    DC_REGS.store(regs, Ordering::Relaxed);
}

/// Read the first few words of each of the display controller's register windows.
///
/// **A deliberately ignorant probe, and that is the point.** Programming the DC8200 needs its
/// register map, which is in a vendor tree rather than mainline; but proving it ANSWERS needs no
/// register map at all. A block that is powered, clocked and out of reset returns varied values; one
/// that is not returns all-ones, all-zeros, or nothing at all. That is the question this stage asks,
/// and it is worth asking on its own because every later stage is written against the assumption.
///
/// Reads only. Nothing here can change the state of anything, which is what makes it safe to run
/// before the register map is understood.
pub fn probe_dc8200() {
    let top = DC_BASE.load(Ordering::Relaxed);
    let regs = DC_REGS.load(Ordering::Relaxed);
    if top == 0 || regs == 0 {
        super::print_str("riscv64: display - no dc8200 in the device tree\n");
        return;
    }

    for (base, name) in [(top, "top"), (regs, "regs")] {
        super::print_str("riscv64: display - dc8200 ");
        super::print_str(name);
        super::print_str(" @");
        super::print_hex(base);
        super::print_str(":");
        for i in 0..6usize {
            super::print_str(" ");
            super::print_hex(mmio_read(base, i * 4) as u64);
        }
        super::print_str("\n");
    }
}

// ============================ stage four: the mode set ============================
//
// Facts from StarFive's `vs_dc_hw.c` and `vs_dc.c`, read as an executable datasheet (§26.14). What
// is borrowed is the register map, the field layout and above all the ORDER. What is not borrowed is
// the driver's shape: Linux reaches this through DRM's atomic modeset - a plane state, a CRTC, an
// encoder chain and a clock framework that resolves parents by name at runtime. Here it is one mode,
// programmed once, at boot, because what the kernel needs from a display is a framebuffer to print
// into and nothing else.
//
// **Which display is the TV.** The controller has two, and the device tree settles it rather than a
// guess: the HDMI node's port endpoint and the controller's `endpoint@1` name each other, and the
// vendor driver's enable path parents display 0's pixel clock to `hdmitx0_pixelclk` while display 1
// drives the LCD path. So display 0 is the HDMI one, its register offset is zero, and it is bit 0 of
// the panel-start register.

/// The vendor driver quotes every register relative to a nominal base of 0x800 and subtracts it on
/// each access, because the controller's second window starts there. The numbering is kept exactly
/// as it appears in that driver, so a line here can be compared with the line it came from without
/// doing arithmetic first; `dc_write` does the subtraction the same way.
const DC_REG_BASE: usize = 0x800;

/// In the FIRST window (0x2940_0000, 0x100 long), not the one above.
const DC_HW_REVISION: usize = 0x0024;
const DC_HW_CHIP_CID: usize = 0x0030;

const DC_FRAMEBUFFER_ADDRESS: usize = 0x1400;
const DC_FRAMEBUFFER_STRIDE: usize = 0x1408;
const DC_FRAMEBUFFER_CONFIG: usize = 0x1518;
const DC_FRAMEBUFFER_U_ADDRESS: usize = 0x1530;
const DC_FRAMEBUFFER_V_ADDRESS: usize = 0x1538;
const DC_FRAMEBUFFER_U_STRIDE: usize = 0x1800;
const DC_FRAMEBUFFER_V_STRIDE: usize = 0x1808;
const DC_FRAMEBUFFER_SIZE: usize = 0x1810;
const DC_FRAMEBUFFER_WATER_MARK: usize = 0x1ce8;
const DC_FRAMEBUFFER_CONFIG_EX: usize = 0x1cc0;
const DC_FRAMEBUFFER_TOP_LEFT: usize = 0x24d8;
const DC_FRAMEBUFFER_BOTTOM_RIGHT: usize = 0x24e0;
const DC_FRAMEBUFFER_SRC_GLOBAL_COLOR: usize = 0x2500;
const DC_FRAMEBUFFER_DST_GLOBAL_COLOR: usize = 0x2508;
const DC_FRAMEBUFFER_BLEND_CONFIG: usize = 0x2510;

const DC_DISPLAY_DITHER_CONFIG: usize = 0x1410;
const DC_DISPLAY_PANEL_CONFIG: usize = 0x1418;
const DC_DISPLAY_H: usize = 0x1430;
const DC_DISPLAY_H_SYNC: usize = 0x1438;
const DC_DISPLAY_V: usize = 0x1440;
const DC_DISPLAY_V_SYNC: usize = 0x1448;
const DC_DISPLAY_CURRENT_LOCATION: usize = 0x1450;
const DC_DISPLAY_DPI_CONFIG: usize = 0x14b8;
const DC_DISPLAY_PANEL_START: usize = 0x1ccc;
const DC_DISPLAY_DP_CONFIG: usize = 0x1cd0;

/// Pixel format 5 in the controller's table: an IGNORED byte, then 8 bits each of red, green and
/// blue, so a little-endian `u32` written as `0xRRGGBB` puts blue at the low byte - the layout the
/// boot console already speaks on every other port.
///
/// **Format 6 is the same layout with the top byte read as ALPHA, and choosing it is why the screen
/// was black.** The kernel's console writes red, green and blue and leaves the fourth byte alone, so
/// every pixel it draws carries alpha zero - fully transparent. The controller composited exactly
/// what it was told to and showed the background colour, which is black, on a display that was
/// scanning perfectly at 60 Hz into a television that had been reporting itself connected all along.
/// Nothing was broken; the picture was invisible on purpose.
const FORMAT_X8R8G8B8: u32 = 5;
/// The pixel interface: 5 is 24-bit RGB, which is what an HDMI transmitter wants.
const DPI_RGB888: u32 = 5;
/// The SAME choice in the display-port config register, which numbers its formats differently: 2
/// rather than 5 for 24-bit RGB. Two registers, two encodings, one meaning - and the driver writes
/// both, so this does too.
const DP_RGB888: u32 = 2;

/// 1920x1080 at 60 Hz, the CEA-861 timing every television accepts.
const H_ACTIVE: u32 = 1920;
const H_SYNC_START: u32 = 2008;
const H_SYNC_END: u32 = 2052;
const H_TOTAL: u32 = 2200;
const V_ACTIVE: u32 = 1080;
const V_SYNC_START: u32 = 1084;
const V_SYNC_END: u32 = 1089;
const V_TOTAL: u32 = 1125;
/// 2200 x 1125 x 60 = 148.5 MHz, and `vout_src` is 1188 MHz, so the divisor is eight.
const PIXEL_DIVISOR: u32 = 8;

/// Four bytes per pixel, so the stride is the width times four and the whole thing is 8,294,400
/// bytes - exactly 2025 pages, with no remainder to round up.
const FB_STRIDE: u32 = H_ACTIVE * 4;
const FB_BYTES: usize = (H_ACTIVE * 4 * V_ACTIVE) as usize;

/// Where the framebuffer ended up, so the stage that hands it to the boot console can find it.
static FB_PHYS: AtomicU64 = AtomicU64::new(0);

fn dc_read(off: usize) -> u32 {
    mmio_read(DC_REGS.load(Ordering::Relaxed), off - DC_REG_BASE)
}

fn dc_write(off: usize, val: u32) {
    mmio_write(DC_REGS.load(Ordering::Relaxed), off - DC_REG_BASE, val);
}

/// Read, clear, set, write - the vendor driver's `dc_set_clear`, and the reason it exists is that
/// several unrelated things share one register, so a plain write would switch off whatever else was
/// in it.
fn dc_modify(off: usize, set: u32, clear: u32) {
    let v = (dc_read(off) & !clear) | set;
    dc_write(off, v);
}

/// Point the pixel clock at the controller's own divider and set it to the mode's rate.
///
/// **This is a deliberate divergence from the vendor driver, and it is what makes this stage
/// testable on its own.** For the HDMI display that driver parents the pixel clock to
/// `hdmitx0_pixelclk`, which the device tree models as a fixed 297 MHz clock but which is really
/// generated by the HDMI transmitter's own PLL - so with that parent selected there is no pixel clock
/// at all until the transmitter is up, and a controller with no pixel clock does not scan out and
/// cannot be told apart from one that was programmed wrongly.
///
/// The divider is a real alternative rather than a stand-in: it is parent 0 of the same mux, it
/// divides `vout_src` (1188 MHz off PLL2), and 1188/8 is 148.5 MHz - the exact pixel rate this mode
/// needs. The device tree's own 297 MHz for `hdmitx0_pixelclk` is 1188/4, which is where the 1188
/// comes from and makes it a cross-check rather than an assumption. So the controller runs at the
/// right rate now, and the transmitter stage becomes "re-parent it", not "make it work at all".
fn pixel_clock_on(vout: u64) {
    // The divider register holds a divisor and nothing else - no enable bit (see VOUTCLK_APB).
    mmio_write(vout, VOUTCLK_DC8200_PIX * 4, PIXEL_DIVISOR);

    // The two gated MUXes on the path. The parent index lives in bits 27:24; zero selects the first
    // parent, which for `dc8200_pix0` is the divider just set and for the LCD-domain clock is
    // `dc8200_pix0` itself. Bit 31 is the gate, already on from stage two - set again so this
    // function is correct read on its own rather than only in sequence.
    const MUX_MASK: u32 = 0x0f << 24;
    for i in [VOUTCLK_DC8200_PIX0, VOUTCLK_DOM_VOUT_TOP_LCD] {
        let v = mmio_read(vout, i * 4);
        mmio_write(vout, i * 4, (v & !MUX_MASK) | CLK_ENABLE);
    }

    super::print_str("riscv64: display - pixel clock: div=");
    super::print_hex(mmio_read(vout, VOUTCLK_DC8200_PIX * 4) as u64);
    super::print_str(" pix0=");
    super::print_hex(mmio_read(vout, VOUTCLK_DC8200_PIX0 * 4) as u64);
    super::print_str(" lcd=");
    super::print_hex(mmio_read(vout, VOUTCLK_DOM_VOUT_TOP_LCD * 4) as u64);
    super::print_str("\n");
}

/// Fill the framebuffer with eight colour bars.
///
/// Not decoration: it is the only thing that can tell us, from across the room, which byte of a pixel
/// is which. Text would prove the controller is scanning; bars in the wrong order would prove the
/// channel shifts are wrong, and that is a mistake the serial console cannot see.
fn paint_test_pattern(fb: &mut [u8]) {
    // Written with the top byte set even though the format now ignores it: a colour that is opaque
    // in its own bytes cannot be made invisible by a register somewhere else, and the first version
    // of these bars was invisible for exactly that reason.
    const BARS: [u32; 8] = [
        0xffff_ffff, // white
        0xffff_ff00, // yellow
        0xff00_ffff, // cyan
        0xff00_ff00, // green
        0xffff_00ff, // magenta
        0xffff_0000, // red
        0xff00_00ff, // blue
        0xff00_0000, // black
    ];
    let bar_w = H_ACTIVE as usize / BARS.len();
    for y in 0..V_ACTIVE as usize {
        let row = y * FB_STRIDE as usize;
        for x in 0..H_ACTIVE as usize {
            let c = BARS[(x / bar_w).min(BARS.len() - 1)].to_le_bytes();
            let p = row + x * 4;
            fb[p] = c[0];
            fb[p + 1] = c[1];
            fb[p + 2] = c[2];
            fb[p + 3] = c[3];
        }
    }
}

/// The controller's own interrupt latch, in the FIRST register window. Reading it clears it.
const AQ_INTR_ACKNOWLEDGE: usize = 0x0010;
const AQ_INTR_ENBL: usize = 0x0014;

/// Is the controller actually scanning out, and at what rate?
///
/// **This asks the frame-end latch, not the scan-position register, and the difference cost a board
/// boot.** `DC_DISPLAY_CURRENT_LOCATION` is documented, is at the offset the driver says, and reads
/// zero forever on this revision - so the first version of this function reported `0 frames per
/// second` about a controller that was scanning perfectly well. A register that reads zero is not
/// evidence of anything; a register that CHANGES is. The latch here sets a bit per display at the end
/// of every frame and clears when read, so counting reads that saw a bit counts frames.
///
/// The rate is computed from the machine's counter BETWEEN THE FIRST AND LAST EVENT, not from the
/// width of the polling window. Dividing a count by the window assumes the loop is fast enough to
/// have caught every event, which is exactly the assumption a wrong answer would hide; timing the
/// interval between two events it definitely saw does not.
///
/// Returns hundredths of a hertz, so 60.00 and 59.94 are different numbers rather than both "60".
fn scanout_rate(top: u64) -> (u32, u32, u32) {
    mmio_write(top, AQ_INTR_ENBL, 0xf);
    let hz = super::timebase_hz() as u64;
    let window = if hz == 0 { 10_000_000 } else { hz };

    let start = super::sbi::time();
    let mut first_at = 0u64;
    let mut last_at = 0u64;
    let mut frames = 0u32;
    let mut seen = 0u32;
    while super::sbi::time().wrapping_sub(start) < window {
        if mmio_read(top, AQ_INTR_ACKNOWLEDGE) & 0xf != 0 {
            let now = super::sbi::time();
            seen |= mmio_read(top, AQ_INTR_ACKNOWLEDGE) | 1;
            if frames == 0 {
                first_at = now;
            }
            last_at = now;
            frames += 1;
        }
    }

    let centihz = if frames < 2 || last_at <= first_at || hz == 0 {
        0
    } else {
        (((frames - 1) as u64 * 100 * hz) / (last_at - first_at)) as u32
    };
    (frames, centihz, seen)
}

/// Program the display controller for 1080p60 and prove it is scanning out.
///
/// Nothing here reaches the television yet - the HDMI transmitter is the stage after this one - and
/// that separation is the point: this stage answers "is the controller producing a correct raster
/// from a real framebuffer", which is a yes-or-no answer over the serial console, while the
/// transmitter stage answers "does that raster leave the board". Merged, a dark screen would mean
/// either.
pub fn mode_set() -> bool {
    let top = DC_BASE.load(Ordering::Relaxed);
    let vout = VOUTCRG_BASE.load(Ordering::Relaxed);
    if top == 0 || DC_REGS.load(Ordering::Relaxed) == 0 || vout == 0 {
        super::print_str("riscv64: display - no dc8200 to program\n");
        return false;
    }

    // IDENTIFY THE SILICON BEFORE WRITING TO IT. The vendor driver refuses any revision it does not
    // recognise, and so does this: every offset below is that revision's map, and writing them into a
    // part with a different one is writing at random.
    let rev = mmio_read(top, DC_HW_REVISION);
    let cid = mmio_read(top, DC_HW_CHIP_CID);
    super::print_str("riscv64: display - dc8200 revision ");
    super::print_hex(rev as u64);
    super::print_str(" cid ");
    super::print_hex(cid as u64);
    super::print_str("\n");
    if rev != 0x5720 && rev != 0x5721 {
        super::print_str("riscv64: display - unrecognised revision; refusing to program it\n");
        return false;
    }

    // The framebuffer. RESERVED rather than merely allocated: it is handed to a device that scans it
    // forever, so it must never return to the pool to be handed out again as a page table - the same
    // rule the other ports' framebuffers follow, reached here through the DMA-arena path because that
    // is what reserving means in this allocator.
    let frames = FB_BYTES.div_ceil(4096);
    let Some(fb_phys) = crate::memory::allocator::alloc_dma_arena(frames) else {
        super::print_str("riscv64: display - could not reserve a framebuffer\n");
        return false;
    };
    // The controller's address register is 32 bits wide. This board's RAM starts at 1 GiB so a
    // framebuffer normally lands well inside that, but "normally" is not a guarantee and a truncated
    // address would have the display scanning out someone else's memory - a corruption that looks
    // like a graphics bug.
    if fb_phys + FB_BYTES as u64 > u32::MAX as u64 {
        super::print_str("riscv64: display - framebuffer is above 4 GiB; the controller cannot reach it\n");
        return false;
    }
    FB_PHYS.store(fb_phys, Ordering::Relaxed);

    // SAFETY: `frames` pages of physically-contiguous RAM, just reserved by the frame allocator and
    // owned by nobody else, inside the kernel's identity map. Turning it into a slice here is the one
    // unsafe step; every pixel written afterwards is a bounds-checked slice write, which is how the
    // boot console stays free of `unsafe` on every port.
    let fb: &'static mut [u8] =
        unsafe { core::slice::from_raw_parts_mut(fb_phys as *mut u8, FB_BYTES) };

    super::print_str("riscv64: display - framebuffer at ");
    super::print_hex(fb_phys);
    super::print_str(", ");
    super::print_dec(frames as u64);
    super::print_str(" pages\n");
    paint_test_pattern(fb);

    pixel_clock_on(vout);

    // The controller's own initialisation, from the vendor driver's per-panel loop.
    dc_write(DC_DISPLAY_PANEL_CONFIG, 0x111);

    // THE MODE, and it comes before the plane because that is the order the driver runs in: the
    // display is enabled when the output comes up, the plane is written on the frame that follows.
    // Starting a display whose plane is not configured yet scans one null frame, which costs nothing
    // and is what the reference does.
    //
    // The two writes at the top are the ones the FIRST attempt missed, and missing them is why it
    // programmed a correct-looking controller that never scanned. The driver has two display paths -
    // `setup_display` and `setup_display_ex` - and only one of them is reachable: the function table
    // names the `_ex` variant, which does this and then calls the other. Reading the plain one and
    // stopping there produced code that matched a function nothing calls.
    //
    // Bit 3 of the display-port config is the output enable; the driver sets it for every encoder
    // that is not a DSI panel, and clears it for one that is. Bit 16 of the panel config selects a
    // YUV pipeline, which this is not.
    dc_write(DC_DISPLAY_DP_CONFIG, DP_RGB888 | (1 << 3));
    dc_modify(DC_DISPLAY_PANEL_CONFIG, 0, 1 << 16);

    // The output is stopped before a timing register is touched (the driver clears the same two bits
    // first) because changing a raster's size underneath a running scan is how a controller ends up
    // wedged mid-frame.
    dc_write(DC_DISPLAY_DPI_CONFIG, DPI_RGB888);
    dc_modify(DC_DISPLAY_PANEL_START, 0, (1 << 0) | (1 << 2));

    // Bit 30 of each sync register enables the pulse and bit 31 inverts its polarity; this mode wants
    // both syncs positive, so bit 31 stays clear. The end of the sync pulse sits at bit 15 rather
    // than 16 - a field boundary that is easy to misread and costs a whole frame's geometry.
    dc_write(DC_DISPLAY_H, H_ACTIVE | (H_TOTAL << 16));
    dc_write(DC_DISPLAY_H_SYNC, H_SYNC_START | (H_SYNC_END << 15) | (1 << 30));
    dc_write(DC_DISPLAY_V, V_ACTIVE | (V_TOTAL << 16));
    dc_write(DC_DISPLAY_V_SYNC, V_SYNC_START | (V_SYNC_END << 15) | (1 << 30));
    dc_write(DC_DISPLAY_DITHER_CONFIG, 0);

    // And start it: bit 12 of the panel config enables the output, bit 0 starts display 0, and bit 3
    // is the two-display sync mode this board does not use.
    dc_modify(DC_DISPLAY_PANEL_CONFIG, 1 << 12, 0);
    dc_modify(DC_DISPLAY_PANEL_START, 1 << 0, 1 << 3);

    // SHADOW REGISTERS OFF WHILE THE PLANE IS WRITTEN, ON AFTERWARDS - the bracket the driver puts
    // around every commit. With bit 12 set, a write to a plane register lands in a shadow bank that
    // the hardware latches at the next vertical blank; with it clear, the write is direct. Writing a
    // whole plane through the shadow bank and never re-arming it would leave the values staged and
    // never applied, which reads exactly like a write that did not land.
    dc_modify(DC_FRAMEBUFFER_CONFIG_EX, 0, 1 << 12);

    // The primary plane: where the pixels are, how they are laid out, and where on the screen they
    // go. The position registers matter more than they look - they default to zero, which is an empty
    // rectangle, so a plane with a perfectly good address and stride would show nothing at all.
    dc_write(DC_FRAMEBUFFER_ADDRESS, fb_phys as u32);
    dc_write(DC_FRAMEBUFFER_STRIDE, FB_STRIDE);
    dc_write(DC_FRAMEBUFFER_U_ADDRESS, 0);
    dc_write(DC_FRAMEBUFFER_V_ADDRESS, 0);
    dc_write(DC_FRAMEBUFFER_U_STRIDE, 0);
    dc_write(DC_FRAMEBUFFER_V_STRIDE, 0);
    dc_write(DC_FRAMEBUFFER_SIZE, H_ACTIVE | (V_ACTIVE << 15));
    dc_write(DC_FRAMEBUFFER_WATER_MARK, 0);
    dc_write(DC_FRAMEBUFFER_TOP_LEFT, 0);
    dc_write(DC_FRAMEBUFFER_BOTTOM_RIGHT, H_ACTIVE | (V_ACTIVE << 15));

    // THE PLANE IS OPAQUE, said three times because there are three ways to say it and the reset
    // state of all three is "invisible". The driver writes the blend registers on every commit and
    // never leaves them at reset; leaving them there means a global alpha of ZERO, which composites
    // the plane away no matter what its pixels contain. `0x3548` is the driver's value for the mode
    // that ignores per-pixel alpha and uses the global one, and the global one is now 0xff - so
    // neither the pixels' fourth byte nor the blend unit can make the picture disappear again.
    dc_write(DC_FRAMEBUFFER_SRC_GLOBAL_COLOR, 0xff << 24);
    dc_write(DC_FRAMEBUFFER_DST_GLOBAL_COLOR, 0xff << 24);
    dc_write(DC_FRAMEBUFFER_BLEND_CONFIG, 0x3548);

    // Format, and everything alongside it switched off explicitly: no swizzle, no tiling, no YUV, no
    // rotation, no hardware clear, no scaling. The clear masks are the driver's, kept whole rather
    // than trimmed to the fields being set, because what they buy is that this register ends in a
    // known state whatever it held before.
    dc_modify(
        DC_FRAMEBUFFER_CONFIG,
        FORMAT_X8R8G8B8 << 26,
        (0x1f << 26) | (1 << 25) | (0x03 << 23) | (1 << 22) | (0x1f << 17) | (0x07 << 14)
            | (0x07 << 11) | (1 << 8),
    );
    // Bit 6 says the source is RGB and bit 8 says it is YUV - the second thing the `_ex` path does
    // that the plain one does not, and a plane whose colour space is unstated is not obviously going
    // to scan. Bit 5 is the de-gamma table, off. Bit 13 enables the plane; bits 18:16 are its
    // stacking order and bit 19 says which display it belongs to - both zero, for the bottom of
    // display 0. Bit 12 stays clear here and is set once at the end.
    dc_modify(
        DC_FRAMEBUFFER_CONFIG_EX,
        (1 << 6) | (1 << 13),
        (1 << 1) | (1 << 5) | (1 << 8) | (1 << 13) | (0x07 << 16) | (1 << 19),
    );

    // Re-arm the shadow bank, so anything written from here on takes effect on a frame boundary
    // rather than mid-scan. This is the state the driver leaves the hardware in.
    dc_modify(DC_FRAMEBUFFER_CONFIG_EX, 1 << 12, 0);

    // WHAT THE HARDWARE ACTUALLY HOLDS, read back rather than assumed. A controller that does not
    // scan has two quite different explanations - the registers do not hold what was written (an
    // addressing or bus problem) or they do and something else is missing (a clock, an enable) - and
    // they have nothing in common. One line separates them, and it costs a board boot to guess.
    super::print_str("riscv64: display - readback:");
    for (name, off) in [
        ("panel_cfg", DC_DISPLAY_PANEL_CONFIG),
        ("panel_start", DC_DISPLAY_PANEL_START),
        ("dp", DC_DISPLAY_DP_CONFIG),
        ("dpi", DC_DISPLAY_DPI_CONFIG),
        ("h", DC_DISPLAY_H),
        ("hsync", DC_DISPLAY_H_SYNC),
        ("v", DC_DISPLAY_V),
        ("vsync", DC_DISPLAY_V_SYNC),
        ("fb", DC_FRAMEBUFFER_ADDRESS),
        ("stride", DC_FRAMEBUFFER_STRIDE),
        ("size", DC_FRAMEBUFFER_SIZE),
        ("fbcfg", DC_FRAMEBUFFER_CONFIG),
        ("fbcfg_ex", DC_FRAMEBUFFER_CONFIG_EX),
    ] {
        super::print_str(" ");
        super::print_str(name);
        super::print_str("=");
        super::print_hex(dc_read(off) as u64);
    }
    super::print_str("\n");

    report_scanout(top, "after the mode set");
    let (frames, _, _) = scanout_rate(top);
    if frames == 0 {
        super::print_str("riscv64: display - the controller is NOT scanning out\n");
        return false;
    }
    true
}

/// One line saying whether frames are happening and how fast, tagged with when it was asked.
fn report_scanout(top: u64, when: &str) {
    let (frames, centihz, seen) = scanout_rate(top);
    super::print_str("riscv64: display - ");
    super::print_str(when);
    super::print_str(": ");
    super::print_dec(frames as u64);
    super::print_str(" frames in 1s, ");
    super::print_dec((centihz / 100) as u64);
    super::print_str(".");
    let frac = centihz % 100;
    if frac < 10 {
        super::print_str("0");
    }
    super::print_dec(frac as u64);
    super::print_str(" Hz, latch bits ");
    super::print_hex(seen as u64);
    super::print_str("\n");
}

// ==================== the clock tree, when the raster does not run ====================
//
// Every register the controller was given reads back exactly what was written, and it still does not
// scan. That leaves two explanations, and neither can be settled by reasoning from here: either the
// pixel clock is not running, or the scan-position register is not the instrument it looks like. So
// this measures BOTH rather than picking one - a board boot costs more than a long line of output.

/// Registers in the system controller that hold PLL2, from Linux's `clk-starfive-jh7110-pll.c`.
/// FBDIV and the two power-down bits share one word; the fractional part and the pre- and
/// post-dividers follow it.
const PLL2_PD: usize = 0x2c;
const PLL2_FRAC: usize = 0x30;
const PLL2_PREDIV: usize = 0x34;
/// The crystal every PLL on this SoC multiplies up.
const OSC_HZ: u64 = 24_000_000;

/// What PLL2 is actually generating, worked out from its own registers.
///
/// The whole pixel clock hangs off this: `vout_src` is a gate on PLL2 and `dc8200_pix` divides it,
/// and the divisor of eight was chosen on the belief that PLL2 runs at 1188 MHz - which was INFERRED
/// from the device tree quoting 297 MHz for a clock that is PLL2 over four. Inference is not
/// measurement, and this is the number that decides whether there is a pixel clock at all.
fn pll2_hz(syscon: u64) -> (u64, u32, u32, u32) {
    let pd = mmio_read(syscon, PLL2_PD);
    let prediv_reg = mmio_read(syscon, PLL2_PREDIV);
    let fbdiv = (pd >> 17) & 0xfff;
    let prediv = prediv_reg & 0x3f;
    let postdiv1 = (prediv_reg >> 28) & 0x03;
    // Integer mode. The fractional path adds a 24-bit fraction to FBDIV; it is reported separately
    // rather than folded in, so a board using it is visible rather than silently mis-computed.
    let hz = if prediv == 0 {
        0
    } else {
        OSC_HZ * fbdiv as u64 / prediv as u64 / (1u64 << postdiv1)
    };
    (hz, fbdiv, prediv, postdiv1)
}

/// Print the whole clock path, the PLL under it, the display sub-system controller, and a second
/// opinion on whether frames are happening.
pub fn diagnose() {
    let sys = SYSCRG_BASE.load(Ordering::Relaxed);
    let vout = VOUTCRG_BASE.load(Ordering::Relaxed);
    let syscon = SYSCON_BASE.load(Ordering::Relaxed);
    let dss = DSSCTRL_BASE.load(Ordering::Relaxed);
    let top = DC_BASE.load(Ordering::Relaxed);

    if syscon != 0 {
        let (hz, fbdiv, prediv, postdiv1) = pll2_hz(syscon);
        super::print_str("riscv64: display - pll2: fbdiv=");
        super::print_dec(fbdiv as u64);
        super::print_str(" prediv=");
        super::print_dec(prediv as u64);
        super::print_str(" postdiv1=");
        super::print_dec(postdiv1 as u64);
        super::print_str(" frac=");
        super::print_hex(mmio_read(syscon, PLL2_FRAC) as u64);
        super::print_str(" pd=");
        super::print_hex(mmio_read(syscon, PLL2_PD) as u64);
        super::print_str(" -> ");
        super::print_dec(hz / 1_000_000);
        super::print_str(" MHz\n");
    }

    // The system generator's video-out corner. Index 59 is the one to look at: it is the `vout_axi`
    // DIVIDER, which the first version of this file tried to enable as if it were a gate and then
    // dropped when that failed. A divider holding zero is a clock that is off, and nothing about the
    // way it failed said so.
    if sys != 0 {
        super::print_str("riscv64: display - syscrg[56..63]:");
        for i in 56..64usize {
            super::print_str(" ");
            super::print_hex(mmio_read(sys, i * 4) as u64);
        }
        super::print_str("\n");
    }

    if vout != 0 {
        super::print_str("riscv64: display - voutcrg[0..17]:");
        for i in 0..18usize {
            super::print_str(" ");
            super::print_hex(mmio_read(vout, i * 4) as u64);
        }
        super::print_str("\n");
    }

    if dss != 0 {
        super::print_str("riscv64: display - dssctrl[0..8]:");
        for i in 0..9usize {
            super::print_str(" ");
            super::print_hex(mmio_read(dss, i * 4) as u64);
        }
        super::print_str("\n");
    }

    if top != 0 {
        report_scanout(top, "while diagnosing");
    }
}

// ============================ stage five: the HDMI transmitter ============================
//
// The controller is producing a raster; this is what puts it on a wire. Facts from StarFive's
// `inno_hdmi.c` and `inno_hdmi.h`, read as an executable datasheet (§26.14) - the PLL coefficients,
// the power-up order and the magic values are the silicon's requirements and several of them mean
// nothing outside it. What is not borrowed is the shape: Linux drives this as a DRM encoder with
// runtime power management, an I2C adapter for EDID, a hot-plug interrupt and an audio path. Here it
// is one mode, brought up once, with no EDID read and no hot-plug - the kernel needs a picture to
// print on, and asking the television what it would prefer is a conversation for a service to have.
//
// **The pixel clock changes hands here.** Stage four ran the controller off an internal divider so
// that it could be tested with the transmitter still dark. That was always temporary: on this SoC the
// transmitter's PLL generates the pixel clock and feeds it BACK to the controller, which is why the
// device tree lists `hdmitx0_pixelclk` as one of the controller's inputs and why the vendor driver
// selects it for the HDMI display. So once the transmitter's PLL reports lock, the controller is
// re-pointed at it and the two run from one clock by construction rather than by two dividers
// agreeing.

/// Every register in this block is one byte wide at a four-byte stride, so the driver's `0x1a0` is
/// 0x680 into the window. Offsets are kept in the driver's numbering for the same reason the display
/// controller's are: so a line here can be compared with the line it came from.
fn hdmi_write(off: usize, val: u32) {
    mmio_write(HDMI_BASE.load(Ordering::Relaxed), off * 4, val);
}

fn hdmi_read(off: usize) -> u32 {
    mmio_read(HDMI_BASE.load(Ordering::Relaxed), off * 4) & 0xff
}

/// The reset the video-out generator holds this block in. Its own, separate from the display
/// controller's three.
const VOUTRST_HDMI_TX: u32 = 9;

/// System control, and the video timing block.
const HDMI_SYS_CTRL: usize = 0x00;
const HDMI_VIDEO_TIMING_CTL: usize = 0x08;
const HDMI_VIDEO_EXT_HTOTAL_L: usize = 0x09;
const HDMI_VIDEO_EXT_HBLANK_L: usize = 0x0b;
const HDMI_VIDEO_EXT_HDELAY_L: usize = 0x0d;
const HDMI_VIDEO_EXT_HDURATION_L: usize = 0x0f;
const HDMI_VIDEO_EXT_VTOTAL_L: usize = 0x11;
const HDMI_VIDEO_EXT_VBLANK: usize = 0x13;
const HDMI_VIDEO_EXT_VDELAY: usize = 0x14;
const HDMI_VIDEO_EXT_VDURATION: usize = 0x15;

/// The PHY's own register bank, above 0x100. These have no names in the vendor header either - the
/// driver writes them by number, and the numbers are the interface.
const PHY_PRE_PLL_LOCK: usize = 0x1a9;
const PHY_POST_PLL_LOCK: usize = 0x1af;

/// The PLL coefficients for a 148.5 MHz pixel clock and the same TMDS rate, taken from the two tables
/// in `inno_hdmi.c` at the row marked `1080p 60`. They are a solved simultaneous equation for this
/// PHY, not something to derive: the pre-PLL row is
/// `{148500000, 148500000, 1, 99, 1, 1, 1, 1, 2, 2, 2, 0, 0}` and the post-PLL row is
/// `{148500000, 1, 20, 1, 3, 3}`.
const PRE_PREDIV: u32 = 1;
const PRE_FBDIV: u32 = 99;
const PRE_TMDS_DIV_A: u32 = 1;
const PRE_TMDS_DIV_B: u32 = 1;
const PRE_TMDS_DIV_C: u32 = 1;
const PRE_PCLK_DIV_A: u32 = 1;
const PRE_PCLK_DIV_B: u32 = 2;
const PRE_PCLK_DIV_C: u32 = 2;
const PRE_PCLK_DIV_D: u32 = 2;
const POST_PREDIV: u32 = 1;
const POST_FBDIV: u32 = 20;
const POST_POSTDIV: u32 = 1;

/// Wait, BOUNDED, for a PHY lock bit.
///
/// The reference driver spins on these two bits with no bound at all - `while (!(readb(0x1a9) & 1));`
/// - which is a design this kernel cannot copy: a PLL that never locks would take the machine with
/// it, silently, before a single service started. Same bit, same meaning, an answer either way.
fn wait_lock(off: usize, name: &str) -> bool {
    let hz = super::timebase_hz() as u64;
    // A tenth of a second. A PLL locks in microseconds; this is long enough that a slow one is not
    // called broken and short enough that a broken one costs the boot nothing.
    let deadline = super::sbi::time().wrapping_add(if hz == 0 { 1_000_000 } else { hz / 10 });
    while super::sbi::time() < deadline {
        if hdmi_read(off) & 1 != 0 {
            return true;
        }
    }
    super::print_str("riscv64: display - HDMI PLL did not lock: ");
    super::print_str(name);
    super::print_str("\n");
    false
}

/// Program the transmitter's two PLLs for the mode's pixel clock.
///
/// The order is the driver's and matters: register 0x1a0 is written 1 first and 0 last, which brackets
/// the whole configuration - the PLL is held while its coefficients change and released once. Writing
/// 0x1aa twice is not a mistake in the reference and is not one here: the first write is the "being
/// configured" value and the second is the working one, chosen by whether the post-divider is in use.
fn config_pll() {
    hdmi_write(0x1a0, 0x01);
    hdmi_write(0x1aa, 0x0f);
    hdmi_write(0x1a1, PRE_PREDIV);
    hdmi_write(0x1a2, 0xf0 | (PRE_FBDIV >> 8));
    hdmi_write(0x1a3, PRE_FBDIV & 0xff);
    hdmi_write(0x1a4, (PRE_TMDS_DIV_A << 4) | (PRE_TMDS_DIV_B << 2) | PRE_TMDS_DIV_C);
    hdmi_write(0x1a5, (PRE_PCLK_DIV_B << 5) | PRE_PCLK_DIV_A);
    hdmi_write(0x1a6, (PRE_PCLK_DIV_C << 5) | PRE_PCLK_DIV_D);
    hdmi_write(0x1ab, POST_PREDIV);
    hdmi_write(0x1ac, POST_FBDIV & 0xff);
    // The post-divider is enabled for this rate, so these two carry its divisor and the matching
    // control value rather than the disabled pair (0x00 and 0x02).
    hdmi_write(0x1ad, POST_POSTDIV);
    hdmi_write(0x1aa, 0x0e);
    hdmi_write(0x1a0, 0x00);
}

/// The mode, told to the transmitter in its own terms.
///
/// It wants the same raster the controller is producing but expressed as totals and back porches
/// rather than absolute positions, which is why each of these is a subtraction rather than a constant:
/// stating them twice, once per block, is how a mismatch becomes a mistake in one place instead of
/// two numbers that must be kept equal by hand.
fn config_video_timing() {
    let pairs: [(usize, u32); 5] = [
        (HDMI_VIDEO_EXT_HTOTAL_L, H_TOTAL),
        (HDMI_VIDEO_EXT_HBLANK_L, H_TOTAL - H_ACTIVE),
        (HDMI_VIDEO_EXT_HDELAY_L, H_TOTAL - H_SYNC_START),
        (HDMI_VIDEO_EXT_HDURATION_L, H_SYNC_END - H_SYNC_START),
        (HDMI_VIDEO_EXT_VTOTAL_L, V_TOTAL),
    ];
    for (off, v) in pairs {
        hdmi_write(off, v & 0xff);
        hdmi_write(off + 1, (v >> 8) & 0xff);
    }
    // The vertical back porch, sync offset and sync width are single bytes: a 1080p frame's are 45,
    // 41 and 5, all of which fit, and the transmitter provides no high half for them.
    hdmi_write(HDMI_VIDEO_EXT_VBLANK, V_TOTAL - V_ACTIVE);
    hdmi_write(HDMI_VIDEO_EXT_VDELAY, V_TOTAL - V_SYNC_START);
    hdmi_write(HDMI_VIDEO_EXT_VDURATION, V_SYNC_END - V_SYNC_START);

    // Bit 0 says the timing comes from the registers above rather than from an internal mode table,
    // and bits 2 and 3 are the two sync polarities - positive for this mode, as the controller was
    // told. Bit 1 would be interlace.
    hdmi_write(HDMI_VIDEO_TIMING_CTL, 1 | (1 << 2) | (1 << 3));
}

/// Status, including whether the transmitter can see a television on the other end of the cable.
const HDMI_STATUS: usize = 0xc8;
/// Hot-plug detect: the sink pulls this up through the cable, so it is the transmitter's own answer
/// to "is something plugged in and powered". It is the one bit here that depends on the world outside
/// the board, which is what makes it worth more than any of the others.
const HDMI_HOTPLUG: u32 = 1 << 7;

/// Read the transmitter back, register by register, and say whether it sees a sink.
///
/// Every value written to this block so far has been written blind. The controller's registers were
/// read back and that is how it was established that the mode set was not the problem; the same is
/// owed here before anything else is guessed at. In particular the PHY's output stage - the LDO, the
/// serializer, the TMDS driver - could accept a write and hold nothing if the analog supplies are
/// not up, which is the difference between "the transmitter is configured" and "the transmitter is
/// working" and is invisible from the digital side.
fn report_transmitter() {
    super::print_str("riscv64: display - transmitter readback:");
    for (name, off) in [
        ("sys", HDMI_SYS_CTRL),
        ("vidctl", 0x01usize),
        ("timing", HDMI_VIDEO_TIMING_CTL),
        ("htot_l", HDMI_VIDEO_EXT_HTOTAL_L),
        ("htot_h", HDMI_VIDEO_EXT_HTOTAL_L + 1),
        ("vtot_l", HDMI_VIDEO_EXT_VTOTAL_L),
        ("vtot_h", HDMI_VIDEO_EXT_VTOTAL_L + 1),
        ("vblank", HDMI_VIDEO_EXT_VBLANK),
        ("ce", 0xce),
        ("prelock", PHY_PRE_PLL_LOCK),
        ("postlock", PHY_POST_PLL_LOCK),
        ("ldo", 0x1b4),
        ("ser", 0x1be),
        ("tmds", 0x1b2),
        ("drive_bf", 0x1bf),
        ("drive_c0", 0x1c0),
    ] {
        super::print_str(" ");
        super::print_str(name);
        super::print_str("=");
        super::print_hex(hdmi_read(off) as u64);
    }
    super::print_str("\n");

    let status = hdmi_read(HDMI_STATUS);
    super::print_str("riscv64: display - HDMI status ");
    super::print_hex(status as u64);
    super::print_str(": a television is ");
    super::print_str(if status & HDMI_HOTPLUG != 0 { "CONNECTED" } else { "NOT detected" });
    super::print_str("\n");
}

/// Bring the transmitter up and hand it the raster.
pub fn hdmi_on() -> bool {
    let hdmi = HDMI_BASE.load(Ordering::Relaxed);
    let vout = VOUTCRG_BASE.load(Ordering::Relaxed);
    if hdmi == 0 || vout == 0 {
        super::print_str("riscv64: display - no HDMI transmitter in the device tree\n");
        return false;
    }

    // Its own reset, released now that its three clocks are running. Stage two enabled those and
    // deliberately left this held, because a block out of reset with nothing driving it is a block
    // that can be found in a state nobody chose.
    if !reset_deassert(vout, VOUTCRG_RESET_ASSERT, VOUTCRG_RESET_STATUS, VOUTRST_HDMI_TX) {
        super::print_str("riscv64: display - HDMI transmitter reset did not release\n");
        return false;
    }

    // TEN MILLISECONDS, which the reference driver takes between powering this block and touching
    // it and which the first version of this stage left out. A bounded wait on the machine's own
    // counter rather than a spin count, so it is ten milliseconds on both machines.
    let hz = super::timebase_hz() as u64;
    let settle = super::sbi::time().wrapping_add(if hz == 0 { 40_000 } else { hz / 100 });
    while super::sbi::time() < settle {
        core::hint::spin_loop();
    }

    // Two writes the driver makes before anything else, whose meaning is not in any header: bit 2 of
    // 0x1b0, and 0xf into 0x1cc. Recorded as borrowed rather than explained, which is the honest state
    // of knowledge about them (§26.14).
    hdmi_write(0x1b0, hdmi_read(0x1b0) | 0x04);
    hdmi_write(0x1cc, 0x0f);

    config_pll();
    if !wait_lock(PHY_PRE_PLL_LOCK, "pre") || !wait_lock(PHY_POST_PLL_LOCK, "post") {
        return false;
    }
    super::print_str("riscv64: display - HDMI PLLs locked\n");

    hdmi_write(0x1b4, 0x07); // the PHY's regulator
    hdmi_write(0x1be, 0x71); // the serializer
    // The driver adjusts the transmitter's drive strength per video mode to keep the eye diagram
    // open; these are its values for 1080p60 (CEA mode 16).
    hdmi_write(0x1bf, 0x02);
    hdmi_write(0x1c0, 0x22);

    // Configure the video path with the output stage OFF, then switch it on - so nothing half-formed
    // ever reaches the cable.
    hdmi_write(0x00, 0x63);
    config_video_timing();
    hdmi_write(0x00, 0x61);
    hdmi_write(0x1b2, 0x8f); // the TMDS driver

    // The driver's last act: strobe register 0xce low then high, which restarts the video path with
    // everything above in place.
    hdmi_write(0xce, 0x00);
    hdmi_write(0xce, 0x01);

    report_transmitter();

    // AND NOW THE PIXEL CLOCK CHANGES HANDS. Parent 1 of the controller's pixel-clock mux is
    // `hdmitx0_pixelclk`, which is what the PLL just locked is generating; the device tree calls it a
    // fixed 297 MHz clock only because a device tree has no way to say "the transmitter decides".
    // Doing this after lock rather than before is the whole reason stage four ran off a divider.
    const MUX_MASK: u32 = 0x0f << 24;
    let v = mmio_read(vout, VOUTCLK_DC8200_PIX0 * 4);
    mmio_write(vout, VOUTCLK_DC8200_PIX0 * 4, (v & !MUX_MASK) | (1 << 24) | CLK_ENABLE);
    super::print_str("riscv64: display - pixel clock re-pointed at the transmitter: pix0=");
    super::print_hex(mmio_read(vout, VOUTCLK_DC8200_PIX0 * 4) as u64);
    super::print_str("\n");

    // Did the raster survive the change of clock? If it did not, the transmitter's pixel clock is not
    // reaching the controller and the answer is to put the divider back - which is a fact worth one
    // line rather than a dark screen with no explanation.
    let top = DC_BASE.load(Ordering::Relaxed);
    if top != 0 {
        report_scanout(top, "after the transmitter");
        // Bit 5 of the plane's config is the controller's underflow flag: set means it asked the
        // memory system for pixels and did not get them in time. It distinguishes a display that is
        // scanning nothing from one that is scanning something it could not fetch, which look
        // identical on a dark screen.
        super::print_str("riscv64: display - plane fetch: fbcfg=");
        super::print_hex(dc_read(DC_FRAMEBUFFER_CONFIG) as u64);
        super::print_str(if dc_read(DC_FRAMEBUFFER_CONFIG) & (1 << 5) != 0 {
            " UNDERFLOW\n"
        } else {
            " no underflow\n"
        });
    }
    true
}

// ============================ stage six: text on the screen ============================

/// Give the framebuffer to the kernel's boot/panic console.
///
/// **Nothing about the font or the colours is decided here, and that is the point.** `bootcon` owns
/// the glyphs, the palette and the layout on every port; what an architecture owes it is one
/// `FbParams` - the memory, its physical base, the geometry, and where each colour channel sits in a
/// pixel - and one `fb_commit`. So this port gets the same text, in the same font, in the same
/// colours as the others without a line of code that knows what a character is. That is the whole
/// claim of the arch seam, and a display is the place it is easiest to break by writing "just a small
/// renderer" instead.
///
/// The channel shifts are the controller's pixel format restated: A8R8G8B8 packs alpha, red, green
/// and blue from the top of a 32-bit word down, so red sits at bit 16, green at 8 and blue at 0.
pub fn adopt_as_boot_console() {
    let phys = FB_PHYS.load(Ordering::Relaxed);
    if phys == 0 {
        return;
    }
    // SAFETY: the same run of frames `mode_set` reserved and handed to the display controller,
    // permanently removed from the allocator so nothing else can ever be given it, inside the
    // kernel's identity map. The controller reads it and the console writes it; there is no third
    // owner, and the console's writes are bounds-checked slice writes from here on.
    let mem: &'static mut [u8] =
        unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, FB_BYTES) };
    crate::bootcon::init(crate::bootcon::FbParams {
        mem,
        phys,
        pitch: FB_STRIDE as usize,
        bpp: 4,
        width: H_ACTIVE as usize,
        height: V_ACTIVE as usize,
        r_shift: 16,
        g_shift: 8,
        b_shift: 0,
    });
    // Only NOW does the serial path start mirroring: a byte painted before `bootcon::init` would be
    // drawn through a console that has no framebuffer yet.
    super::screen_ready();
    // DELIBERATELY NOT CLEARED. The colour bars painted before the mode set stay under the text, so
    // the screen carries three distinguishable answers instead of one: bars with text on them means
    // the whole path works; bars alone means the console is not drawing; a black but LIT screen means
    // the controller is scanning memory it cannot really see, which is what a cache-coherence problem
    // looks like from the sofa. Once the path is proven this goes back to a clean screen.
    super::print_str("riscv64: display - the boot console now draws to the screen\n");
}
