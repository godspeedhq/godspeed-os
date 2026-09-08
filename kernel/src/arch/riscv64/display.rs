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

/// Pixel format 6 in the controller's table: 8 bits each of alpha, red, green and blue, in that order
/// within a 32-bit word - so a little-endian `u32` written as `0x00RR_GGBB` puts blue at the low
/// byte, which is the same layout the boot console already speaks on every other port.
const FORMAT_A8R8G8B8: u32 = 6;
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
    const BARS: [u32; 8] = [
        0x00ff_ffff, // white
        0x00ff_ff00, // yellow
        0x0000_ffff, // cyan
        0x0000_ff00, // green
        0x00ff_00ff, // magenta
        0x00ff_0000, // red
        0x0000_00ff, // blue
        0x0000_0000, // black
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

/// Is the controller actually scanning out, and at what rate?
///
/// **The one measurement a successful write cannot fake.** Every register above accepts a value
/// whether or not a pixel clock is running; the scan-position register lives in the pixel clock's own
/// domain, so it moves only if the controller is genuinely producing a raster. Watching the line
/// number wrap counts whole frames, which is the mode's refresh rate - a number that is either 60 or
/// tells us precisely how wrong the timing is.
///
/// Bounded by the machine's own counter, so a display that never scans reports zero rather than
/// hanging the boot.
fn scanout_rate() -> (u32, u32, u32) {
    let hz = super::timebase_hz() as u64;
    // A fifth of a second: twelve frames at 60 Hz, which is enough to divide back to a rate with
    // one-frame resolution and short enough that a dead display costs the boot nothing.
    let window = if hz == 0 { 2_000_000 } else { hz / 5 };
    let first = dc_read(DC_DISPLAY_CURRENT_LOCATION);

    let start = super::sbi::time();
    let mut last = (first >> 16) & 0xffff;
    let mut frames = 0u32;
    while super::sbi::time().wrapping_sub(start) < window {
        let y = (dc_read(DC_DISPLAY_CURRENT_LOCATION) >> 16) & 0xffff;
        // A wrap back to the top of the raster is one frame. Counting wraps rather than reading a
        // frame counter means this works without knowing anything else about the register.
        if y < last {
            frames += 1;
        }
        last = y;
    }

    (first, dc_read(DC_DISPLAY_CURRENT_LOCATION), frames * 5)
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

    // Format, and everything alongside it switched off explicitly: no swizzle, no tiling, no YUV, no
    // rotation, no hardware clear, no scaling. The clear masks are the driver's, kept whole rather
    // than trimmed to the fields being set, because what they buy is that this register ends in a
    // known state whatever it held before.
    dc_modify(
        DC_FRAMEBUFFER_CONFIG,
        FORMAT_A8R8G8B8 << 26,
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

    let (first, second, hz) = scanout_rate();
    super::print_str("riscv64: display - scan position ");
    super::print_hex(first as u64);
    super::print_str(" -> ");
    super::print_hex(second as u64);
    super::print_str(", ");
    super::print_dec(hz as u64);
    super::print_str(" frames per second\n");

    if hz == 0 {
        super::print_str("riscv64: display - the controller is NOT scanning out\n");
        return false;
    }
    super::print_str("riscv64: display - scanning 1920x1080; the transmitter is what is left\n");
    true
}
