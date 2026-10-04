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
//! **V1 issued commands only; V2 adds the data phase.** Identification, CMD52 register access and the CIS
//! walk all travel on the CMD line. CMD53 (`cmd_data`) moves its words by PIO through the FIFO, polled -
//! the firmware upload is what needs it.

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
const DW_BLKSIZ: usize = 0x01c;
const DW_BYTCNT: usize = 0x020;
const DW_INTMASK: usize = 0x024;
const DW_CMDARG: usize = 0x028;
const DW_CMD: usize = 0x02c;
const DW_RESP0: usize = 0x030;
const DW_RINTSTS: usize = 0x044;
const DW_STATUS: usize = 0x048;
const DW_FIFOTH: usize = 0x04c;
const DW_VERID: usize = 0x06c;
const DW_HCON: usize = 0x070;
/// The data FIFO's window. `0x100` below controller version `0x240A`, `0x200` from it on (`DATA_OFFSET` /
/// `DATA_240A_OFFSET` in `dw_mmc.h`); this host is version `0x290A` (V0's census, `docs/wifi-aic8800.md`
/// 9), so `0x200`. Read per transfer from `VERID` rather than assumed, so a different revision of the
/// controller cannot silently move it.
const DW_DATA_OLD: usize = 0x100;
const DW_DATA_240A: usize = 0x200;

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
/// `CTRL` bits that hand the FIFO to a DMA engine. Cleared before every data phase, as
/// `dw_mci_submit_data` does for PIO: this host never points a DMA engine at memory.
const CTRL_DMA_ENABLE: u32 = 1 << 5;
const CTRL_USE_IDMAC: u32 = 1 << 25;

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
const CMD_DAT_WR: u32 = 1 << 10;
const CMD_DAT_EXP: u32 = 1 << 9;
const CMD_RESP_CRC: u32 = 1 << 8;
const CMD_RESP_EXP: u32 = 1 << 6;

const INT_RE: u32 = 1 << 1;
const INT_CMD_DONE: u32 = 1 << 2;
const INT_DATA_OVER: u32 = 1 << 3;
const INT_TXDR: u32 = 1 << 4;
const INT_RXDR: u32 = 1 << 5;
const INT_RCRC: u32 = 1 << 6;
const INT_DCRC: u32 = 1 << 7;
const INT_RTO: u32 = 1 << 8;
const INT_DRTO: u32 = 1 << 9;
const INT_HTO: u32 = 1 << 10;
const INT_FRUN: u32 = 1 << 11;
const INT_HLE: u32 = 1 << 12;
const INT_SBE: u32 = 1 << 13;
const INT_EBE: u32 = 1 << 15;
const INT_CMD_ERRORS: u32 = INT_RE | INT_RCRC | INT_RTO | INT_HLE;
/// `DW_MCI_DATA_ERROR_FLAGS`, plus the FIFO under/overrun a PIO loop can cause itself.
const INT_DATA_ERRORS: u32 = INT_DRTO | INT_DCRC | INT_HTO | INT_SBE | INT_EBE | INT_FRUN;

const STATUS_BUSY: u32 = 1 << 9;
/// `STATUS[29:17]`, the words the FIFO holds now.
const fn status_fifo_count(s: u32) -> u32 {
    (s >> 17) & 0x1fff
}

/// A data phase's budget, end to end. The data timeout in `TMOUT` is set to its maximum, so the
/// controller will not end a slow transfer for us; this is the bound that does (26.6). At the 400 kHz
/// identification clock a 512-byte block is ~10 ms on one data line, so a second is generous for the
/// largest transfer V2 makes and still short enough that a stuck one is seen.
const DATA_WAIT: Budget = Budget::ms(1_000);

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
    /// `(BLKSIZ, BYTCNT)` packed as `BLKSIZ << 16 | BYTCNT` for the last data command, for `last_setup`.
    last_blk: Cell<u32>,
    /// The instruments the shared CMD53 failure lines print (`SdioHost::seen`, `dat_window`): every
    /// `RINTSTS` and `STATUS` bit seen during the last data phase, and the polls at which the data path
    /// was first and last seen busy. Reset at the start of each transfer, so they describe that one.
    seen_int: Cell<u32>,
    seen_status: Cell<u32>,
    dat_first: Cell<u32>,
    dat_last: Cell<u32>,
    /// Polls spent waiting for the FIFO and for data-over since the last `take_waits`.
    waits_ready: Cell<u64>,
    waits_done: Cell<u64>,
}

impl<'a> Host<'a> {
    pub fn new(ctx: &'a ServiceContext, m: &'a Mmio) -> Self {
        Host {
            ctx,
            m,
            need_init: Cell::new(true),
            last_int: Cell::new(0),
            last_cmd: Cell::new(0),
            last_blk: Cell::new(0),
            seen_int: Cell::new(0),
            seen_status: Cell::new(0),
            dat_first: Cell::new(0),
            dat_last: Cell::new(0),
            waits_ready: Cell::new(0),
            waits_done: Cell::new(0),
        }
    }

    fn data_reg(&self) -> usize {
        if self.rd(DW_VERID) & 0xffff < 0x240a { DW_DATA_OLD } else { DW_DATA_240A }
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
    /// `data` carries the data-phase bits (`DAT_EXP`, `DAT_WR`, `PRV_DAT_WAIT`) for `cmd_data`, and is 0
    /// for a command on the CMD line alone.
    fn cmd_inner(&self, c: Cmd, arg: u32, data: u32) -> Option<u32> {
        let mut flags = (c.index as u32 & 0x3f) | data;
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
        // With a data phase to follow, clear only the COMMAND's bits: a receive-ready or data-over that
        // has already landed belongs to `cmd_data_inner`'s loop, and clearing it here would leave that loop
        // waiting for an event that already happened.
        self.wr(DW_RINTSTS, if data != 0 { ints & (INT_CMD_DONE | INT_CMD_ERRORS) } else { ints });
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

    /// A command with a data phase, by PIO through the FIFO: `dw_mci_submit_data` for the setup (DMA off,
    /// `BLKSIZ`, `BYTCNT`), the command with `DAT_EXP` (and `DAT_WR` for a write), then `dw_mci_read_data_pio`
    /// / `dw_mci_write_data_pio`'s loop polled instead of interrupt-driven: on receive-ready or data-over,
    /// take what `STATUS` says the FIFO holds; on transmit-ready, give it what fits. Data-over ends it.
    ///
    /// Every error ends it too, with the FIFO reset so the next transfer starts from an empty one; telling
    /// the CARD to abandon the transfer is the protocol's job (`sdio::abort`), which every CMD53 caller in
    /// `sdio.rs` already does on `Err`.
    fn cmd_data_inner(&self, c: Cmd, arg: u32, x: Xfer, buf: &mut [u32]) -> Result<(), &'static str> {
        let bytes = x.geom.size.max(1) * x.geom.count.max(1);
        // Byte mode's size 0 means 512 bytes to the card; this host is never asked for that, and refusing
        // it keeps `BYTCNT` and the buffer provably the same length.
        if buf.is_empty() || x.geom.size == 0 || bytes as usize != buf.len() * 4 {
            return Err("dw_mmc: the transfer's geometry does not match its buffer");
        }
        self.seen_int.set(0);
        self.seen_status.set(0);
        self.dat_first.set(0);
        self.dat_last.set(0);
        // The card may still hold DAT0 busy from the last transfer (`dw_mci_wait_while_busy`, for commands
        // with data only).
        if wait::until(self.ctx, CARD_WAIT, || self.rd(DW_STATUS) & STATUS_BUSY == 0).is_err() {
            self.last_int.set(self.rd(DW_RINTSTS));
            return Err("dw_mmc: the data path stayed busy from the previous transfer");
        }
        if !self.ctrl_reset(CTRL_FIFO_RESET) {
            return Err("dw_mmc: the FIFO reset did not clear");
        }
        let ctrl = self.rd(DW_CTRL);
        self.wr(DW_CTRL, ctrl & !(CTRL_DMA_ENABLE | CTRL_USE_IDMAC));
        self.wr(DW_BLKSIZ, x.geom.size);
        self.wr(DW_BYTCNT, bytes);
        self.last_blk.set((x.geom.size << 16) | (bytes & 0xffff));

        let dir = if x.read { 0 } else { CMD_DAT_WR };
        if self.cmd_inner(c, arg, CMD_DAT_EXP | dir | CMD_PRV_DAT_WAIT).is_none() {
            let _ = self.ctrl_reset(CTRL_FIFO_RESET);
            return Err("the command was not answered (no data phase was attempted)");
        }

        let fifo = self.data_reg();
        let mut done = 0usize; // words moved
        let mut polls = 0u32;
        let mut d = wait::Deadline::start(self.ctx, DATA_WAIT);
        let result = loop {
            polls = polls.wrapping_add(1);
            let ints = self.rd(DW_RINTSTS);
            let st = self.rd(DW_STATUS);
            self.seen_int.set(self.seen_int.get() | ints);
            self.seen_status.set(self.seen_status.get() | st);
            if st & STATUS_BUSY != 0 {
                if self.dat_first.get() == 0 {
                    self.dat_first.set(polls);
                }
                self.dat_last.set(polls);
            }
            if ints & INT_DATA_ERRORS != 0 {
                self.wr(DW_RINTSTS, ints);
                self.last_int.set(ints);
                break Err(if ints & INT_DCRC != 0 {
                    "data CRC error"
                } else if ints & INT_DRTO != 0 {
                    "the card sent no data (data read timeout)"
                } else if ints & INT_FRUN != 0 {
                    "the FIFO under- or overran"
                } else {
                    "a data error (start bit, end bit or host timeout)"
                });
            }
            if x.read && ints & (INT_RXDR | INT_DATA_OVER) != 0 {
                let mut n = status_fifo_count(self.rd(DW_STATUS)) as usize;
                while n > 0 && done < buf.len() {
                    buf[done] = self.rd(fifo);
                    done += 1;
                    n -= 1;
                }
                self.wr(DW_RINTSTS, INT_RXDR);
            }
            if !x.read && ints & INT_TXDR != 0 {
                let room = FIFO_DEPTH_WORDS.saturating_sub(status_fifo_count(self.rd(DW_STATUS))) as usize;
                let mut n = room;
                while n > 0 && done < buf.len() {
                    self.wr(fifo, buf[done]);
                    done += 1;
                    n -= 1;
                }
                self.wr(DW_RINTSTS, INT_TXDR);
            }
            if ints & INT_DATA_OVER != 0 {
                self.wr(DW_RINTSTS, INT_DATA_OVER);
                if done == buf.len() {
                    break Ok(());
                }
                // Data-over with words still owed: one more look at the FIFO for a read (the last words can
                // land with it), then the shortfall is the answer.
                if x.read {
                    let mut n = status_fifo_count(self.rd(DW_STATUS)) as usize;
                    while n > 0 && done < buf.len() {
                        buf[done] = self.rd(fifo);
                        done += 1;
                        n -= 1;
                    }
                }
                break if done == buf.len() { Ok(()) } else { Err("data-over came before every word moved") };
            }
            if d.expired() {
                self.last_int.set(ints);
                break Err(if done == 0 {
                    "no word moved before the data phase's budget ran out"
                } else {
                    "the data phase stalled part-way and its budget ran out"
                });
            }
        };
        if done < buf.len() || result.is_err() {
            self.waits_ready.set(self.waits_ready.get() + polls as u64);
        } else {
            self.waits_done.set(self.waits_done.get() + polls as u64);
        }
        if result.is_err() {
            let _ = self.ctrl_reset(CTRL_FIFO_RESET);
            return result;
        }
        // A write leaves the card busy on DAT0 while it takes the block; the next command waits on that
        // anyway, but a transfer that ends with the card still busy is reported as such rather than as done.
        if !x.read && wait::until(self.ctx, CARD_WAIT, || self.rd(DW_STATUS) & STATUS_BUSY == 0).is_err() {
            return Err("the card stayed busy after the write");
        }
        Ok(())
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
        self.cmd_inner(c, arg, 0)
    }
    fn cmd_data(&self, c: Cmd, arg: u32, x: Xfer, buf: &mut [u32]) -> Result<(), &'static str> {
        self.cmd_data_inner(c, arg, x, buf)
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
    /// `(BLKSIZ << 16 | BYTCNT, CMD)` for the last data command.
    fn last_setup(&self) -> (u32, u32) {
        (self.last_blk.get(), self.last_cmd.get())
    }
    fn last_ctrl0(&self) -> u32 {
        self.rd(DW_CTRL)
    }
    fn seen(&self) -> (u32, u32) {
        (self.seen_int.get(), self.seen_status.get())
    }
    fn dat_window(&self) -> (u32, u32) {
        (self.dat_first.get(), self.dat_last.get())
    }
    fn take_waits(&self) -> (u64, u64) {
        (self.waits_ready.replace(0), self.waits_done.replace(0))
    }
}
