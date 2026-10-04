// SPDX-License-Identifier: GPL-2.0-only
//! ROLE: discovery
//!
//! Establishing that this board's WiFi radio has an SD host controller to be reached through, and
//! preparing it to be granted (`docs/wifi-aic8800.md` 3, phase V0).
//!
//! **This file does not drive the radio, for the same reason `net.rs` does not drive ethernet.** The
//! VisionFive 2 Lite's AIC8800D80 module sits on the JH7110's second DesignWare SD/MMC host (`mmc1`).
//! Talking SDIO to it is a driver, and drivers are services (CLAUDE.md 4.4). What the kernel owes is what
//! lives in SHARED blocks and so cannot be handed to a service: the host's two clocks and its reset in the
//! system clock generator, and its six pins plus the radio's power pin in the system GPIO block. Then it
//! checks that the controller ANSWERS, says so, and grants the window by the device's KIND, `WIFI_SDIO`,
//! never by a service name - the same seam, and the same kind, as the Pi 4's radio.
//!
//! Every value below was read from a working implementation used as an executable datasheet (26.14):
//! Linux `clk-starfive-jh7110-sys.c` and `reset-starfive-jh7110.c` for the clock and reset, the pinctrl
//! driver `pinctrl-starfive-jh7110.c` / `-sys.c` and `jh7110-pinfunc.h` for the pin functions and pad
//! bits, `jh7110-common.dtsi`'s `mmc1_pins` for which pad bits this host's pins take, and StarFive's
//! vendor device tree for GPIO 33 as the radio's enable (`gpio_wl_reg_on`).

use core::sync::atomic::{AtomicBool, Ordering};
use portable_atomic::AtomicU64;

use super::display::{clk_enable, mmio_read, mmio_write, reset_deassert, SYSCRG_RESET_ASSERT, SYSCRG_RESET_STATUS};

/// The system clock generator's gates for `mmc1`: the register (AHB) clock and the card clock. The AHB
/// clock is the one register access needs; the card clock is what the bus runs on.
const SYSCLK_SDIO1_AHB: usize = 92;
const SYSCLK_SDIO1_SDCARD: usize = 94;
/// `mmc1`'s AHB reset in the system generator (`reset-starfive-jh7110.c`), whose assert and status
/// registers `display.rs` already names.
const SYSRST_SDIO1_AHB: u32 = 65;

/// The system pinctrl block (`pinctrl-starfive-jh7110-sys.c`). DOEN and DOUT select, per pin, which
/// peripheral drives its output-enable and its output; GPI selects, per INPUT SIGNAL, which pin feeds it;
/// GPIOIN reads the pads; each pad has a configuration word.
const PIN_DOEN: usize = 0x000;
const PIN_DOUT: usize = 0x040;
const PIN_GPI: usize = 0x080;
const PIN_GPIOEN: usize = 0x0dc;
const PIN_GPIOIN: usize = 0x118;
const PIN_PADCFG: usize = 0x120;
const DOEN_MASK: u32 = 0x3f;
const DOUT_MASK: u32 = 0x7f;
const GPI_MASK: u32 = 0x7f;
/// `jh7110-pinfunc.h` / the bindings header: a pin's output enable from function 0 is always on, from 1
/// always off; an input signal fed from GPI value 0 or 1 is a constant, so a pin is `pin + 2`.
const GPOEN_ENABLE: u32 = 0;
const GPOUT_LOW: u32 = 0;
const GPOUT_HIGH: u32 = 1;
/// Pad configuration bits (`pinctrl-starfive-jh7110.c`).
const PAD_IE: u32 = 1 << 0;
const PAD_DS_MASK: u32 = 0b11 << 1;
const PAD_DS_12MA: u32 = 3 << 1;
const PAD_PU: u32 = 1 << 3;
const PAD_PD: u32 = 1 << 4;
const PAD_SLEW: u32 = 1 << 5;
const PAD_SMT: u32 = 1 << 6;
/// The bits this file owns in a pad word; anything else in it is left as found.
const PAD_OWNED: u32 = PAD_IE | PAD_DS_MASK | PAD_PU | PAD_PD | PAD_SLEW | PAD_SMT;

/// `mmc1_pins` in `jh7110-common.dtsi`: pin, output function, output-enable function, input signal.
/// `None` for the clock, which is output only. The clock pad takes pull-up and 12 mA with input and
/// Schmitt DISABLED; the other five take pull-up, 12 mA, input and Schmitt enabled, slew 0 for all.
const SDIO1_PINS: [(u32, u32, u32, Option<u32>, &str); 6] = [
    (10, 55, GPOEN_ENABLE, None, "CLK"),
    (9, 57, 19, Some(44), "CMD"),
    (11, 58, 20, Some(45), "D0"),
    (12, 59, 21, Some(46), "D1"),
    (7, 60, 22, Some(47), "D2"),
    (8, 61, 23, Some(48), "D3"),
];

/// The radio's enable, `gpio_wl_reg_on` in StarFive's vendor device tree for this board. Mainline Linux
/// has no node for it at all, which is how a board can look radio-less to a kernel that trusts mainline.
/// It was the old left-channel audio pin; the vendor tree disables the PWM-DAC that used it, and nothing
/// in this system drives audio on this board.
const RADIO_POWER_PIN: u32 = 33;

/// `dw_mmc` registers read at the census: the version and the hardware configuration.
const DW_VERID: usize = 0x6c;
const DW_HCON: usize = 0x70;

static PINCTRL_BASE: AtomicU64 = AtomicU64::new(0);
static SYSCRG_BASE: AtomicU64 = AtomicU64::new(0);
static HOST_BASE: AtomicU64 = AtomicU64::new(0);
/// Set only once the controller has answered - the grant, and the power control, are gated on it.
static PRESENT: AtomicBool = AtomicBool::new(false);

pub(super) fn set_bases(syscrg: u64, pinctrl: u64, host: u64) {
    SYSCRG_BASE.store(syscrg, Ordering::Relaxed);
    PINCTRL_BASE.store(pinctrl, Ordering::Relaxed);
    HOST_BASE.store(host, Ordering::Relaxed);
}

/// Whether the census saw the radio's host controller answer. The grant and `DEVICE_POWER` both ask this.
pub fn present() -> bool {
    PRESENT.load(Ordering::Acquire)
}

/// The controller's register window, or zero when there is none to offer.
pub fn window() -> u64 {
    if present() { HOST_BASE.load(Ordering::Relaxed) } else { 0 }
}

/// Read-modify-write one byte-wide field in a four-to-a-word mux register file.
fn set_field(base: u64, file: usize, index: u32, mask: u32, val: u32) {
    let off = file + 4 * (index as usize / 4);
    let shift = 8 * (index % 4);
    let v = mmio_read(base, off);
    mmio_write(base, off, (v & !(mask << shift)) | ((val & mask) << shift));
}

fn set_pad(base: u64, pin: u32, bits: u32) {
    let off = PIN_PADCFG + 4 * pin as usize;
    let v = mmio_read(base, off);
    mmio_write(base, off, (v & !PAD_OWNED) | bits);
}

/// The level the pad reads, which is what the radio's enable actually is - not what was asked for.
fn pad_level(base: u64, pin: u32) -> u32 {
    (mmio_read(base, PIN_GPIOIN + 4 * (pin as usize / 32)) >> (pin % 32)) & 1
}

/// Drive the radio's enable and read the pad back. The result follows the READ-BACK, as the Pi 4's does
/// (CLAUDE.md 12.3): a pin that reads the wrong level is a request that did not happen.
pub fn set_radio_power(on: bool) -> bool {
    let base = PINCTRL_BASE.load(Ordering::Relaxed);
    if base == 0 || !present() {
        return false;
    }
    set_field(base, PIN_DOUT, RADIO_POWER_PIN, DOUT_MASK, if on { GPOUT_HIGH } else { GPOUT_LOW });
    let level = pad_level(base, RADIO_POWER_PIN);
    super::print_str("riscv64: wifi - radio enable (GPIO 33) asked ");
    super::print_dec(on as u64);
    super::print_str(", the pad reads ");
    super::print_dec(level as u64);
    super::print_str("\n");
    level == on as u32
}

/// Bring the radio's host controller up far enough to answer, and say whether it did.
pub fn init() -> bool {
    let sys = SYSCRG_BASE.load(Ordering::Relaxed);
    let pin = PINCTRL_BASE.load(Ordering::Relaxed);
    let host = HOST_BASE.load(Ordering::Relaxed);
    if sys == 0 || pin == 0 || host == 0 {
        // QEMU's `virt`, or a board whose tree describes no second SD host. Not an error: no radio here.
        super::print_str("riscv64: wifi - no second SD host in the device tree; no radio to grant\n");
        return false;
    }
    super::print_str("riscv64: wifi - the radio's SD host is at ");
    super::print_hex(host);
    super::print_str(" (the second jh7110-mmc node; this board's design has it at 0x16020000)\n");

    // CLOCKS BEFORE THE RESET. The reset driver notes that releasing a reset whose clock is gated can hang,
    // and a read into an unclocked block on this interconnect stalls rather than faulting. So the AHB clock
    // must stick, or nothing below is attempted.
    let ahb = clk_enable(sys, SYSCLK_SDIO1_AHB);
    let card = clk_enable(sys, SYSCLK_SDIO1_SDCARD);
    if !ahb {
        super::print_str("riscv64: wifi - the SD host's AHB clock did not enable; not touching the controller\n");
        return false;
    }
    // The card clock's divider is the integrator's (the tree asks 50 MHz); reported, never changed here.
    super::print_str("riscv64: wifi - SD host clocks on (ahb on, card ");
    super::print_str(if card { "on" } else { "FAIL" });
    super::print_str("; card clock register ");
    super::print_hex(mmio_read(sys, SYSCLK_SDIO1_SDCARD * 4) as u64);
    super::print_str(")\n");

    if !reset_deassert(sys, SYSCRG_RESET_ASSERT, SYSCRG_RESET_STATUS, SYSRST_SDIO1_AHB) {
        super::print_str("riscv64: wifi - the SD host's reset did not report released; not reading it\n");
        return false;
    }

    // THE PINS. The GPIO block's own enable, then each pin's output, output-enable, input routing and pad.
    mmio_write(pin, PIN_GPIOEN, 1);
    for (p, dout, doen, din, _name) in SDIO1_PINS {
        set_field(pin, PIN_DOEN, p, DOEN_MASK, doen);
        set_field(pin, PIN_DOUT, p, DOUT_MASK, dout);
        if let Some(signal) = din {
            set_field(pin, PIN_GPI, signal, GPI_MASK, p + 2);
            set_pad(pin, p, PAD_PU | PAD_DS_12MA | PAD_IE | PAD_SMT);
        } else {
            set_pad(pin, p, PAD_PU | PAD_DS_12MA);
        }
    }
    // The radio's enable: an output driven from the DOUT value, with the pad's input on so the level can
    // be read back. Driven HIGH - powered, as the vendor image leaves it; the driver owns any power cycle
    // and its hold-offs, through `DevicePower` (CLAUDE.md 12.3, 26.10).
    set_field(pin, PIN_DOUT, RADIO_POWER_PIN, DOUT_MASK, GPOUT_HIGH);
    set_field(pin, PIN_DOEN, RADIO_POWER_PIN, DOEN_MASK, GPOEN_ENABLE);
    let pad = mmio_read(pin, PIN_PADCFG + 4 * RADIO_POWER_PIN as usize);
    mmio_write(pin, PIN_PADCFG + 4 * RADIO_POWER_PIN as usize, pad | PAD_IE);
    super::print_str("riscv64: wifi - pins 10,9,11,12,7,8 routed to the SD host; radio enable (GPIO 33) reads ");
    super::print_dec(pad_level(pin, RADIO_POWER_PIN) as u64);
    super::print_str("\n");

    // DOES IT ANSWER. Two reads, both printed, so the grant rests on a controller seen rather than assumed.
    // `VERID` picks where the FIFO is (below 0x240A it is at 0x100, else 0x200) and `HCON` bits 9:7 give
    // its width; the driver reads both itself, and these lines are what it compares against.
    let verid = mmio_read(host, DW_VERID);
    let hcon = mmio_read(host, DW_HCON);
    super::print_str("riscv64: wifi - dw_mmc VERID=");
    super::print_hex(verid as u64);
    super::print_str(" HCON=");
    super::print_hex(hcon as u64);
    if verid == 0 || verid == 0xffff_ffff {
        super::print_str(" - that is no controller; nothing is granted\n");
        return false;
    }
    super::print_str(" (version ");
    super::print_hex((verid & 0xffff) as u64);
    super::print_str(", FIFO at ");
    super::print_str(if verid & 0xffff < 0x240a { "+0x100" } else { "+0x200" });
    super::print_str(") - granted by kind WIFI_SDIO\n");
    PRESENT.store(true, Ordering::Release);
    true
}
