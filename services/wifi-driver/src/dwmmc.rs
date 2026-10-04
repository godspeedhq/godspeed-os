// SPDX-License-Identifier: GPL-2.0-only
//! The Synopsys DesignWare SD/MMC host (`dw_mmc`), behind `SdioHost` - the VisionFive 2 Lite's radio bus
//! (`docs/wifi-aic8800.md` 4, phase V1).
//!
//! The second implementation of the trait after the Pi 4's Arasan (`host.rs`). The protocol above it -
//! identification, CMD52, the CIS walk - is shared and does not learn which controller this is.
//!
//! Every register, bit and sequence below was read from Linux `drivers/mmc/host/dw_mmc.c` and `dw_mmc.h`
//! as an executable datasheet (26.14): `dw_mci_ctrl_reset`, `mci_send_cmd`, `dw_mci_setup_bus`,
//! `dw_mci_prepare_command`, `dw_mci_start_command`, and the probe's FIFO-threshold and interrupt setup.
//! What is ours is the model: polled, bounded, no interrupts and NO DMA - the host never points a DMA
//! engine at memory, which is what keeps it safe on a non-coherent machine with no IOMMU.
//!
//! **V1 issues commands only.** Identification, CMD52 register access and the CIS walk all travel on the
//! CMD line. The data phase (`cmd_data`, PIO through the FIFO) arrives with the firmware upload that
//! needs it (V2), and until then it refuses by name rather than half-working.

use core::cell::Cell;

use godspeed::driver::wait::{self, Budget};
use godspeed_sdk::{Mmio, ServiceContext};
use godspeed_wifi::sdio::{Cmd, Resp, SdioHost, Xfer};

// Registers (`dw_mmc.h`).
const DW_CTRL: usize = 0x000;
const DW_PWREN: usize = 0x004;
const DW_CLKDIV: usize = 0x008;
const DW_CLKSRC: usize = 0x00c;
const DW_CLKENA: usize = 0x010;
const DW_TMOUT: usize = 0x014;
const DW_CTYPE: usize = 0x018;
const DW_INTMASK: usize = 0x024;
const DW_CMDARG: usize = 0x028;
const DW_CMD: usize = 0x02c;
const DW_RESP0: usize = 0x030;
const DW_RINTSTS: usize = 0x044;
const DW_STATUS: usize = 0x048;
const DW_FIFOTH: usize = 0x04c;
const DW_VERID: usize = 0x06c;
const DW_HCON: usize = 0x070;

/// The FIFO depth in 32-bit words: the board's device tree, `fifo-depth = <32>` on both `jh7110-mmc`
/// nodes. NOT read from `DW_FIFOTH`, which is what V1 first did: the RX watermark's power-on value is
/// depth - 1, but this driver writes depth/2 - 1 there and no reset restores it, so each re-init read
/// the last one's write and halved - 128, 64, 32 across three resets on the board (2026-10-04). Linux
/// gives the same warning in `dw_mci_probe` and takes the depth from the device tree for that reason.
/// The first read on the board implied 128; if that is the real depth, 32 is conservative, and if it is
/// not, 32 is the only safe answer. V2's data phase is where the difference would show.
const FIFO_DEPTH_WORDS: u32 = 32;

const CTRL_RESET: u32 = 1 << 0;
const CTRL_FIFO_RESET: u32 = 1 << 1;
const CTRL_DMA_RESET: u32 = 1 << 2;
const CTRL_ALL_RESET: u32 = CTRL_RESET | CTRL_FIFO_RESET | CTRL_DMA_RESET;

/// `DW_CLKENA`: the card clock on. NEVER the low-power bit (16): it stops the card clock while the bus is
/// idle, and an SDIO card's interrupt needs a running clock - Linux sets `DW_MMC_CARD_NO_LOW_PWR` for
/// every SDIO card for exactly this.
const CLKEN_ENABLE: u32 = 1 << 0;

const CMD_START: u32 = 1 << 31;
const CMD_USE_HOLD_REG: u32 = 1 << 29;
const CMD_UPD_CLK: u32 = 1 << 21;
const CMD_INIT: u32 = 1 << 15;
const CMD_STOP: u32 = 1 << 14;
const CMD_PRV_DAT_WAIT: u32 = 1 << 13;
const CMD_RESP_CRC: u32 = 1 << 8;
const CMD_RESP_EXP: u32 = 1 << 6;

const INT_RE: u32 = 1 << 1;
const INT_CMD_DONE: u32 = 1 << 2;
const INT_RCRC: u32 = 1 << 6;
const INT_RTO: u32 = 1 << 8;
const INT_HLE: u32 = 1 << 12;
const INT_CMD_ERRORS: u32 = INT_RE | INT_RCRC | INT_RTO | INT_HLE;

const STATUS_BUSY: u32 = 1 << 9;

/// The identification clock: at most 400 kHz, as the SD specification requires before a card is known.
const IDENT_HZ: u32 = 400_000;
/// The controller's input clock (`ciu`), in Hz. The device tree assigns the card clock 50 MHz, the
/// generator divides its parent by 2 (the census reads `0x80000002`), and the vendor kernel's log on this
/// board reports `div 62 -> 399193 Hz` for the identification clock - which is 49.5 MHz exactly, by
/// `dw_mci_setup_bus`'s own arithmetic. So 49.5 MHz is MEASURED on this board, not assumed from the
/// device tree's round number.
pub const CIU_HZ: u32 = 49_500_000;

const CONTROL_WAIT: Budget = Budget::ms(500);
const CARD_WAIT: Budget = Budget::ms(1_000);

/// `CMD53`'s card-side abort lives at CCCR 6; a CMD52 to it is sent with `STOP`, as Linux does.
const CCCR_ABORT: u32 = 0x06;

pub struct Host<'a> {
    ctx: &'a ServiceContext,
    m: &'a Mmio,
    /// The FIRST command after the card's power-up carries `INIT` (80 clocks before it), which a card
    /// needs to finish its own power-up. Linux keys it on `DW_MMC_CARD_NEED_INIT`, set at power-on.
    need_init: Cell<bool>,
    last_int: Cell<u32>,
    last_cmd: Cell<u32>,
}

impl<'a> Host<'a> {
    pub fn new(ctx: &'a ServiceContext, m: &'a Mmio) -> Self {
        Host { ctx, m, need_init: Cell::new(true), last_int: Cell::new(0), last_cmd: Cell::new(0) }
    }

    fn rd(&self, off: usize) -> u32 {
        self.m.read32(off)
    }
    fn wr(&self, off: usize, v: u32) {
        self.m.write32(off, v)
    }

    pub fn verid(&self) -> u32 {
        self.rd(DW_VERID)
    }
    pub fn hcon(&self) -> u32 {
        self.rd(DW_HCON)
    }

    /// `dw_mci_ctrl_reset`: set the bits, wait - bounded - for the hardware to clear them.
    fn ctrl_reset(&self, bits: u32) -> bool {
        let v = self.rd(DW_CTRL);
        self.wr(DW_CTRL, v | bits);
        wait::until(self.ctx, CONTROL_WAIT, || self.rd(DW_CTRL) & bits == 0).is_ok()
    }

    /// `mci_send_cmd`: a controller-only command (a clock update), which the CIU acknowledges by clearing
    /// `START`. Never written while `START` is still set - that is the hardware-locked error (`HLE`).
    fn send_ciu(&self, cmd: u32) -> bool {
        if wait::until(self.ctx, CONTROL_WAIT, || self.rd(DW_CMD) & CMD_START == 0).is_err() {
            return false;
        }
        self.wr(DW_CMDARG, 0);
        self.wr(DW_CMD, CMD_START | cmd);
        wait::until(self.ctx, CONTROL_WAIT, || self.rd(DW_CMD) & CMD_START == 0).is_ok()
    }

    /// `dw_mci_setup_bus`: clock off, update; divider, update; clock on (never low-power), update; 1-bit.
    /// `hz == 0` stops the clock. Returns false if the CIU never took an update.
    fn setup_bus(&self, hz: u32) -> bool {
        let upd = CMD_UPD_CLK | CMD_PRV_DAT_WAIT;
        if hz == 0 {
            self.wr(DW_CLKENA, 0);
            return self.send_ciu(upd);
        }
        let mut div = CIU_HZ / hz;
        if CIU_HZ % hz != 0 && CIU_HZ > hz {
            div += 1;
        }
        let div = if CIU_HZ != hz { div.div_ceil(2) } else { 0 };
        self.wr(DW_CLKENA, 0);
        self.wr(DW_CLKSRC, 0);
        if !self.send_ciu(upd) {
            return false;
        }
        self.wr(DW_CLKDIV, div);
        if !self.send_ciu(upd) {
            return false;
        }
        self.wr(DW_CLKENA, CLKEN_ENABLE);
        if !self.send_ciu(upd) {
            return false;
        }
        self.wr(DW_CTYPE, 0);
        let actual = if div == 0 { CIU_HZ } else { CIU_HZ / div / 2 };
        self.ctx.log_fmt(format_args!(
            "wifi-driver: dw_mmc card clock {} Hz (asked {} Hz, ciu {} Hz, div {})", actual, hz, CIU_HZ, div));
        true
    }

    fn reset_inner(&self, ctx: &ServiceContext) -> bool {
        // Power the slot (DW_PWREN bit 0, slot 0), then the probe's reset of controller, FIFO and DMA.
        self.wr(DW_PWREN, 1);
        if !self.ctrl_reset(CTRL_ALL_RESET) {
            ctx.log_fmt(format_args!(
                "wifi-driver: dw_mmc reset did not clear (DW_CTRL={:#010x})", self.rd(DW_CTRL)));
            return false;
        }
        // Everything masked and interrupts off at the controller: this host polls `DW_RINTSTS`, which records
        // whatever the mask says. Clear what is pending; the longest data and response timeouts.
        self.wr(DW_RINTSTS, 0xffff_ffff);
        self.wr(DW_INTMASK, 0);
        self.wr(DW_TMOUT, 0xffff_ffff);
        // FIFO thresholds as the probe sets them: RX mark at half minus one, TX mark at half, burst size
        // code 2. The depth is the device tree's (`FIFO_DEPTH_WORDS`); what `DW_FIFOTH` held before this
        // write is logged only as a record, since after the first reset it is a previous write.
        let held = 1 + ((self.rd(DW_FIFOTH) >> 16) & 0xfff);
        let depth = FIFO_DEPTH_WORDS;
        self.wr(DW_FIFOTH, (2 << 28) | (((depth / 2 - 1) & 0xfff) << 16) | ((depth / 2) & 0xfff));
        ctx.log_fmt(format_args!(
            "wifi-driver: dw_mmc reset, FIFO depth {} words (the device tree's; FIFOTH held a watermark implying {})",
            depth, held));
        self.need_init.set(true);
        self.setup_bus(IDENT_HZ)
    }

    /// One command on the CMD line: `dw_mci_prepare_command` for the flags, `dw_mci_start_command` to
    /// issue it, then a bounded poll of `DW_RINTSTS` for done or an error.
    fn cmd_inner(&self, c: Cmd, arg: u32) -> Option<u32> {
        let mut flags = c.index as u32 & 0x3f;
        let abort = c.index == 52 && (arg >> 9) & 0x1_ffff == CCCR_ABORT;
        if c.index == 0 || abort {
            flags |= CMD_STOP;
        }
        if c.resp != Resp::None {
            flags |= CMD_RESP_EXP;
        }
        if c.check_crc {
            flags |= CMD_RESP_CRC;
        }
        if self.need_init.replace(false) {
            flags |= CMD_INIT;
        }
        flags |= CMD_USE_HOLD_REG;
        // NEVER over a command still in flight (HLE).
        if wait::until(self.ctx, CONTROL_WAIT, || self.rd(DW_CMD) & CMD_START == 0).is_err() {
            self.last_int.set(self.rd(DW_RINTSTS));
            return None;
        }
        self.wr(DW_RINTSTS, 0xffff_ffff);
        self.wr(DW_CMDARG, arg);
        self.wr(DW_CMD, flags | CMD_START);
        self.last_cmd.set(flags);
        let mut ints = 0;
        let done = wait::until(self.ctx, CARD_WAIT, || {
            ints = self.rd(DW_RINTSTS);
            ints & (INT_CMD_DONE | INT_CMD_ERRORS) != 0
        });
        // Wait for CMD_DONE even after an error bit, briefly: the controller raises it after the error.
        let _ = wait::until(self.ctx, Budget::ms(10), || self.rd(DW_RINTSTS) & INT_CMD_DONE != 0);
        ints |= self.rd(DW_RINTSTS);
        self.wr(DW_RINTSTS, ints);
        if done.is_err() || ints & INT_CMD_ERRORS != 0 {
            self.last_int.set(ints);
            return None;
        }
        // R1b (CMD7): the card holds DAT0 busy after answering; wait it out, bounded, before the next.
        if c.resp == Resp::ShortBusy {
            let _ = wait::until(self.ctx, CARD_WAIT, || self.rd(DW_STATUS) & STATUS_BUSY == 0);
        }
        Some(if c.resp == Resp::None { 0 } else { self.rd(DW_RESP0) })
    }
}

impl SdioHost for Host<'_> {
    fn reset(&self, ctx: &ServiceContext) -> bool {
        self.reset_inner(ctx)
    }
    /// The bus quiet across a power edge: the card clock stopped.
    fn park(&self, _ctx: &ServiceContext) -> bool {
        self.setup_bus(0)
    }
    fn set_operating_clock(&self, hz: u32, _ctx: &ServiceContext) -> bool {
        self.setup_bus(hz)
    }
    fn cmd(&self, c: Cmd, arg: u32) -> Option<u32> {
        self.cmd_inner(c, arg)
    }
    fn cmd_data(&self, _c: Cmd, _arg: u32, _x: Xfer, _buf: &mut [u32]) -> Result<(), &'static str> {
        Err("dw_mmc: the data phase arrives with the firmware upload (V2); this host issues commands only")
    }
    fn status(&self) -> u32 {
        self.rd(DW_STATUS)
    }
    fn last_int(&self) -> u32 {
        self.last_int.get()
    }
    fn last_resp(&self) -> u32 {
        self.rd(DW_RESP0)
    }
    fn last_setup(&self) -> (u32, u32) {
        (0, self.last_cmd.get())
    }
    fn last_ctrl0(&self) -> u32 {
        self.rd(DW_CTRL)
    }
    fn seen(&self) -> (u32, u32) {
        (0, 0)
    }
    fn dat_window(&self) -> (u32, u32) {
        (0, 0)
    }
    fn take_waits(&self) -> (u64, u64) {
        (0, 0)
    }
}
