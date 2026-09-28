// SPDX-License-Identifier: GPL-2.0-only
//! The SD host controller, as much of it as talking to an SDIO card on the CMD line needs.
//!
//! **This is the Arasan SDHCI block at `0xFE30_0000`** - the BCM2711's older SD controller, which on a
//! Raspberry Pi 4 is wired not to the card slot but to the CYW43455 radio (the vendor device tree's
//! `mmcnr@7e300000`, bus-width 4, `sdio_pins`). The kernel grants this service that one page of
//! registers at spawn, by name and only when its boot census saw the controller answer, so every
//! access below goes through the SDK's safe `Mmio` wrapper and this crate contains no `unsafe`
//! (§18.1/§18.2).
//!
//! ## Why this is a second SDHCI implementation and not a shared one
//!
//! `services/block-driver/src/sdhci.rs` drives the same silicon on the Pi 2 and is the reference this
//! was written from - the reset dance, the ten-bit clock divider split across `CONTROL1`, the
//! command-timeout bit that the error mask deliberately excludes, the Arasan's clock-domain write
//! erratum and the `Ncc` gap between commands are all its hard-won lessons, kept here because they are
//! facts about the part rather than about SD cards. What is NOT kept is everything about a MEMORY card:
//! CMD8's voltage handshake, ACMD41's OCR negotiation, the CSD capacity decode, the 512-byte PIO block
//! loops. An SDIO card answers none of those, so sharing the file would mean sharing a state machine
//! whose larger half is inapplicable.
//!
//! Sharing it later is a real option and deliberately left open: what would move is exactly the code
//! below (reset, clock, `cmd`), which is the part with no card protocol in it. That is a refactor
//! across two ports and two boards, and §26.2 says a feature is pulled into existence by a need - so
//! it waits until the SDIO path works on hardware and there is something to share rather than
//! something to predict.
//!
//! ## What every wait here owes
//!
//! Every loop is bounded and every bound RETURNS A RESULT THE CALLER READS (`arch/CLAUDE.md`, rule 2).
//! A hardware wait with no bound is a core that never comes back; a bounded wait nobody checks is
//! worse, because it looks like it worked. `cmd` returns `Option`, and the interrupt register at the
//! moment of failure is captured BEFORE the line reset clears it - without that, a caller logging
//! `INTERRUPT` after a failure always reads 0 and cannot tell a timeout from an error.

use godspeed_sdk::{Mmio, ServiceContext};

// Register offsets from the controller base. SDHCI-standard; the BCM2711's Arasan is a conforming
// implementation of the parts used here.
const BLKSIZECNT: usize = 0x04;
const ARG1: usize = 0x08;
const CMDTM: usize = 0x0C;
const RESP0: usize = 0x10;
/// The PIO data FIFO - 32 bits wide, which is why every transfer length here is a multiple of 4.
const DATA: usize = 0x20;
const STATUS: usize = 0x24;
const CONTROL0: usize = 0x28;
/// The DMA-select field of Host Control (`0x28` bits 3-4), which **must be zero for PIO**.
///
/// Linux clears this before EVERY transfer - `sdhci_config_dma`, called from `sdhci_prepare_data` - and
/// says why in as many words: *"Always adjust the DMA selection as some controllers (e.g. JMicron) can't
/// do PIO properly when the selection is ADMA."* Not an init-time setting; a per-transfer one.
///
/// **Why this port and not the Pi 2.** There the firmware BOOTS from this controller, so `CONTROL0` is
/// left in a working PIO state and `block-driver`'s CMD17 inherits it. Here the firmware boots from
/// `emmc2` and never touches the Arasan, so the register holds whatever reset left - and `SRST_HC` is not
/// specified to clear this field. Copying a driver that is correct on a pre-configured controller is not
/// enough on one nothing has configured.
const CONTROL0_DMA_SELECT: u32 = 0x18;
const CONTROL1: usize = 0x2C;
const INTERRUPT: usize = 0x30;
const INT_MASK: usize = 0x34;
const INT_EN: usize = 0x38;
/// A 32-bit read here spans `SLOT_INT_STATUS` (0xFC) and `HOST_CONTROLLER_VERSION` (0xFE). Both
/// read-only. The kernel's boot census identifies the controller from exactly this register, for the
/// reason recorded there: `CAPABILITIES` reads 0 on this part (Linux's `sdhci-iproc` carries
/// `.missing_caps = true` for `bcm2835_data` and supplies them from the driver), so a presence test
/// built on CAPS is built on the one register the silicon does not populate.
const SLOTISR_VER: usize = 0xFC;

// STATUS bits.
const SR_CMD_INHIBIT: u32 = 1 << 0;
const SR_DAT_INHIBIT: u32 = 1 << 1;

// INTERRUPT bits.
const INT_CMD_DONE: u32 = 1 << 0;
/// Transfer complete. NOT the same as the last FIFO access: the controller still has to finish on the
/// bus, and starting the next command before it does is a line conflict.
const INT_DATA_DONE: u32 = 1 << 1;
/// The FIFO can take a word (interrupt status).
const INT_WRITE_RDY: u32 = 1 << 4;
/// The FIFO has a word (interrupt status).
const INT_READ_RDY: u32 = 1 << 5;
/// **Buffer Read Enable, in STATUS - a different register from the interrupt status above.**
///
/// u-boot's polled `sdhci_transfer_data` checks BOTH: the interrupt status for `DATA_AVAIL` and then
/// `PRESENT_STATE` for `SDHCI_DATA_AVAILABLE` (`0x800`). Linux cannot - it is interrupt-driven and must
/// wait on the status bit - but a polled driver can read either, and waiting only on the interrupt flag
/// means a controller that raises this bit without latching that flag is waited on forever. Which is
/// precisely what a wrong interrupt-status-enable would produce.
const ST_BUF_READ_ENABLE: u32 = 1 << 11;
/// Buffer Write Enable, the write-side twin.
const ST_BUF_WRITE_ENABLE: u32 = 1 << 10;
/// DAT Line Active, in STATUS. The bit that says a data phase is in progress at all.
const ST_DAT_ACTIVE: u32 = 1 << 2;
/// The error mask `sdhci.rs` uses, which follows its own reference driver.
const INT_ERR: u32 = 0x017E_8000;
/// Command Timeout - the card did not respond to the command AT ALL. Kept out of `INT_ERR` above (as
/// it is there) so that it is tested explicitly: it is the commonest real-hardware failure, and
/// without naming it the wait simply ran to its bound with `INTERRUPT` reading 0, which looks like
/// nothing happening rather than like a non-response.
const INT_CMD_TIMEOUT: u32 = 0x0001_0000;

// CONTROL1 bits.
const C1_CLK_INTLEN: u32 = 1 << 0;
const C1_CLK_STABLE: u32 = 1 << 1;
const C1_CLK_EN: u32 = 1 << 2;
const C1_TOUNIT_MAX: u32 = 0x000E_0000;
const C1_SRST_HC: u32 = 1 << 24;
const C1_SRST_CMD: u32 = 1 << 25;
const C1_SRST_DATA: u32 = 1 << 26;

/// The SDMA buffer-boundary field both references put in the block-size register.
///
/// Linux writes `SDHCI_MAKE_BLKSZ(host->sdma_boundary, blksz)` and u-boot
/// `SDHCI_MAKE_BLKSZ(SDHCI_DEFAULT_BOUNDARY_ARG, blocksize)`, both giving 7 in bits 12-14. Irrelevant to a
/// PIO transfer that cannot reach a boundary, and written because being the only one of three
/// implementations that puts something different there is not a position worth defending.
const BLK_BOUNDARY: u32 = 7 << 12;

/// `BLKSIZECNT` for a BYTE-mode transfer: one block of `bytes`.
pub const fn blk_byte_mode(bytes: u32) -> u32 {
    (1 << 16) | BLK_BOUNDARY | (bytes & 0xFFF)
}

/// `BLKSIZECNT` for a multi-BLOCK transfer: `blocks` blocks of `size` bytes.
///
/// Named rather than assembled at each call site. Building a register word out of parts is precisely the
/// habit that cost this driver six boots on the chip clock CSR, and a block transfer has two fields where
/// a byte transfer has one.
pub const fn blk_block_mode(blocks: u32, size: u32) -> u32 {
    ((blocks & 0xFFFF) << 16) | BLK_BOUNDARY | (size & 0xFFF)
}

/// A short delay. Spins rather than sleeps because these are microsecond-scale hardware settling gaps
/// on a path that holds no lock and serves nobody yet; a count is not a duration (`arch/CLAUDE.md`),
/// which is why nothing here uses one as a TIMEOUT - the timeouts below are separate bounded loops on
/// a register condition, and this is only ever a minimum gap.
fn spin() {
    for _ in 0..2000 {
        core::hint::spin_loop();
    }
}

pub struct Host<'a> {
    m: &'a Mmio,
    /// The controller's base clock in Hz, from the platform. **0 means refuse**, never guess: every
    /// card clock derives from this, the Arasan reports it wrongly in CAPS on this family, and a
    /// divider from a wrong base runs the identification clock at the wrong speed - so nothing
    /// answers, silently, and only on hardware.
    base_clock: u32,
    /// `INTERRUPT` captured at the moment a command failed, before the line reset that clears it.
    last_int: core::cell::Cell<u32>,
    /// `CONTROL0` as it stood before the last data command cleared its DMA-select field.
    last_ctrl0: core::cell::Cell<u32>,
    /// The OR of every `INTERRUPT` value seen while waiting for the FIFO, and the same for `STATUS`.
    ///
    /// **This is the measurement four hypotheses were substituting for.** Reporting the registers AFTER
    /// a timeout cannot distinguish "the controller never moved" from "it moved and settled back"; an
    /// accumulated OR can. If these read the same as they did going in, the data phase did not happen at
    /// all, and no amount of adjusting the setup is the answer.
    seen_int: core::cell::Cell<u32>,
    seen_status: core::cell::Cell<u32>,
    /// The poll iteration at which DAT Line Active was first and last seen. 0 = never.
    dat_first: core::cell::Cell<u32>,
    dat_last: core::cell::Cell<u32>,
    /// `BLKSIZECNT` as it read back after being written for the last data command.
    last_blk: core::cell::Cell<u32>,
    /// `CMDTM` as it read back after the last command was issued.
    last_cmdtm: core::cell::Cell<u32>,
    /// `RESP0` from the last command a DATA transfer issued.
    ///
    /// **This was being thrown away, and it is the answer to the failure it was hiding.** `cmd_data`
    /// calls `cmd()`, which returns the response, and discarded it - so when a CMD53 completed and no
    /// data followed there was no way to see whether the CARD had refused. A refusal looks exactly like
    /// that: the command completes, the card answers with its flags set, and no data comes. Same shape
    /// as `last_int` and kept for the same reason.
    last_resp: core::cell::Cell<u32>,
}

impl<'a> Host<'a> {
    pub fn new(m: &'a Mmio, base_clock: u32) -> Self {
        Host {
            m,
            base_clock,
            last_int: core::cell::Cell::new(0),
            last_resp: core::cell::Cell::new(0),
            last_blk: core::cell::Cell::new(0),
            last_cmdtm: core::cell::Cell::new(0),
            last_ctrl0: core::cell::Cell::new(0),
            seen_int: core::cell::Cell::new(0),
            seen_status: core::cell::Cell::new(0),
            dat_first: core::cell::Cell::new(0),
            dat_last: core::cell::Cell::new(0),
        }
    }

    fn rd(&self, off: usize) -> u32 {
        self.m.read32(off)
    }
    fn wr(&self, off: usize, v: u32) {
        self.m.write32(off, v)
    }

    /// The version register the kernel's census identified this controller by, for the service to
    /// print and compare. Reading the SAME register the kernel did is deliberate: if the two disagree,
    /// the MMIO grant is pointed somewhere other than where the census looked, and that is worth
    /// catching in the one line where both numbers are visible.
    pub fn version_reg(&self) -> u32 {
        self.rd(SLOTISR_VER)
    }

    pub fn status(&self) -> u32 {
        self.rd(STATUS)
    }

    /// `INTERRUPT` as it was when the last command failed. 0 if none has.
    pub fn last_int(&self) -> u32 {
        self.last_int.get()
    }

    /// `RESP0` from the last command a data transfer issued - the R5 for a CMD53.
    pub fn last_resp(&self) -> u32 {
        self.last_resp.get()
    }

    /// `BLKSIZECNT` and `CMDTM` as they READ BACK - what the controller is actually holding, rather than
    /// what this driver believes it wrote.
    pub fn last_setup(&self) -> (u32, u32) {
        (self.last_blk.get(), self.last_cmdtm.get())
    }

    /// `CONTROL0` as it stood going into the last data command, before its DMA-select field was cleared.
    pub fn last_ctrl0(&self) -> u32 {
        self.last_ctrl0.get()
    }

    /// Every bit ever seen in `INTERRUPT` and in `STATUS` while waiting for the FIFO.
    pub fn seen(&self) -> (u32, u32) {
        (self.seen_int.get(), self.seen_status.get())
    }

    /// The poll iterations at which the data phase was first and last seen active. `(0, 0)` = never.
    pub fn dat_window(&self) -> (u32, u32) {
        (self.dat_first.get(), self.dat_last.get())
    }

    /// The ten-bit SDHCI clock divider for a target clock, from the controller's REAL base clock.
    ///
    /// Card clock is `base / (2 * divisor)`. Ceiling division, so the result is never FASTER than
    /// asked - on the identification clock, faster means no card answers.
    fn divider_for(&self, target_hz: u32) -> u32 {
        if self.base_clock == 0 || target_hz == 0 {
            return 0;
        }
        let d = (self.base_clock + (2 * target_hz) - 1) / (2 * target_hz);
        if d > 0x3FF {
            0x3FF
        } else {
            d
        }
    }

    /// Program the card clock. Returns false if it never reports stable.
    fn set_clock(&self, divisor: u32, ctx: &ServiceContext) -> bool {
        self.wr(CONTROL1, self.rd(CONTROL1) & !C1_CLK_EN);
        for _ in 0..5 {
            spin();
        }
        let c1 = (self.rd(CONTROL1) & !0x0000_FFE0)
            | C1_CLK_INTLEN
            | ((divisor & 0xFF) << 8) // divider low 8 bits  [15:8]
            | (((divisor >> 8) & 0x3) << 6) // divider high 2 bits [7:6], SDHCI 3.0 ten-bit mode
            | C1_TOUNIT_MAX;
        self.wr(CONTROL1, c1);
        // The Arasan loses successive writes to the same register that land within two card-clock
        // cycles of each other - a clock-domain-crossing erratum Linux's `sdhci-iproc` spaces out
        // explicitly. At 400 kHz two cycles is ~5 us, so the CONTROL1 writes are spaced generously.
        for _ in 0..40 {
            spin();
        }
        let mut t = 0u32;
        while self.rd(CONTROL1) & C1_CLK_STABLE == 0 {
            t += 1;
            if t > 1_000_000 {
                ctx.log_fmt(format_args!(
                    "wifi-driver: card clock never reported stable (divisor={}, CONTROL1={:#010x})",
                    divisor,
                    self.rd(CONTROL1)
                ));
                return false;
            }
        }
        self.wr(CONTROL1, self.rd(CONTROL1) | C1_CLK_EN);
        for _ in 0..40 {
            spin();
        }
        true
    }

    /// Reset the controller and bring it up at the 400 kHz identification clock.
    ///
    /// Returns false with a reason logged. Each step reports WHICH one failed, because on hardware the
    /// difference between "the controller never left reset" and "the clock never stabilised" and "the
    /// platform would not tell us the base clock" is three different bugs, and a bare false is a
    /// debugging session (invariant 12).
    pub fn reset(&self, ctx: &ServiceContext) -> bool {
        self.wr(CONTROL1, self.rd(CONTROL1) | C1_SRST_HC);
        let mut t = 0u32;
        while self.rd(CONTROL1) & C1_SRST_HC != 0 {
            t += 1;
            if t > 1_000_000 {
                ctx.log("wifi-driver: SRST_HC never cleared - the controller did not leave reset");
                return false;
            }
        }
        if self.base_clock == 0 {
            ctx.log(
                "wifi-driver: the platform reported NO base clock, so no card clock can be derived. \
                 Refusing rather than guessing: a divider from a wrong base runs the identification \
                 clock at the wrong speed and nothing answers, silently",
            );
            return false;
        }
        let id_div = self.divider_for(400_000);
        ctx.log_fmt(format_args!(
            "wifi-driver: base clock {} Hz, identification divisor {} (target 400 kHz)",
            self.base_clock, id_div
        ));
        if !self.set_clock(id_div, ctx) {
            return false;
        }
        // 1-bit bus, no high-speed, and NO DMA SELECTION. Every command this phase issues rides the
        // CMD line alone, so 4-bit DAT and the 50 MHz mode are work with nothing yet to carry - and the
        // DMA-select field must be zero for PIO to work at all on some controllers (see
        // `CONTROL0_DMA_SELECT`). Written explicitly rather than inherited from whatever the firmware
        // left, which on this board is nothing at all: it boots from the other controller.
        //
        // LOGGED BEFORE AND AFTER, because whether those bits were set is the question. A reader should
        // not have to take "cleared it" on trust when "it was already clear" means something different.
        let c0_before = self.rd(CONTROL0);
        self.wr(CONTROL0, c0_before & !((1 << 1) | (1 << 2) | CONTROL0_DMA_SELECT));
        let c0_after = self.rd(CONTROL0);
        ctx.log_fmt(format_args!(
            "wifi-driver: CONTROL0 {:#010x} -> {:#010x} (DMA select was {:#x}, must be 0 for PIO)",
            c0_before,
            c0_after,
            (c0_before & CONTROL0_DMA_SELECT) >> 3
        ));
        // Latch every status bit so `cmd` can read them; the controller is polled, not interrupt
        // driven, so nothing is unmasked to the CPU.
        self.wr(INT_EN, 0xFFFF_FFFF);
        self.wr(INT_MASK, 0xFFFF_FFFF);
        // No data transfers in this phase, but leave the block registers defined rather than at
        // whatever reset left: a stale block count is the kind of thing that makes the FIRST data
        // command behave oddly, long after this code is out of mind.
        self.wr(BLKSIZECNT, 0);
        true
    }

    /// Set the operating clock once a card has been identified. Separate from `reset` so the caller
    /// decides when identification is over; bounded and reported like everything else.
    pub fn set_operating_clock(&self, hz: u32, ctx: &ServiceContext) -> bool {
        let d = self.divider_for(hz);
        ctx.log_fmt(format_args!("wifi-driver: raising the card clock to ~{} Hz (divisor {})", hz, d));
        self.set_clock(d, ctx)
    }

    /// Issue one command and wait for it to complete. Returns `RESP0`, or `None` with `last_int` set.
    ///
    /// `code` is the SDHCI `CMDTM` word: `index << 24 | flags << 16 | transfer mode`.
    ///
    /// **Whether CRC and index checking are on is PER COMMAND, and this used to get it wrong.** The
    /// comment here said they were "deliberately left OFF because two of the responses this driver reads
    /// - R4 from CMD5 and R3 - carry neither a CRC7 nor a command index". That is true of CMD5, and it
    /// was applied to every command through one shared template. **R5 carries both**, and Linux sets
    /// both for it from `MMC_RSP_R5 = PRESENT | CRC | OPCODE` - see the per-command constants in
    /// `sdio.rs`, which now carry the values `sdhci_send_command` computes.
    pub fn cmd(&self, code: u32, arg: u32) -> Option<u32> {
        self.cmd_inner(code, arg, None)
    }

    /// The one command path. `blk` is the `BLKSIZECNT` word for a command that carries data.
    ///
    /// **`BLKSIZECNT` is written HERE, between the argument and the command**, because that is the order
    /// this controller actually sees from Linux: `sdhci_iproc_writew` defers the block registers into a
    /// shadow and flushes them when the COMMAND register is written, and `sdhci_send_command` writes
    /// ARGUMENT then COMMAND. It was previously written by the caller, before the status clear and the
    /// argument. Both are before the write that starts the transfer, so this is unlikely to matter - it
    /// is here because matching the reference where there is no reason to differ is the method.
    fn cmd_inner(&self, code: u32, arg: u32, blk: Option<u32>) -> Option<u32> {
        let mut t = 0u32;
        while self.rd(STATUS) & (SR_CMD_INHIBIT | SR_DAT_INHIBIT) != 0 {
            t += 1;
            if t > 1_000_000 {
                self.last_int.set(self.rd(INTERRUPT));
                return None;
            }
        }
        self.wr(INTERRUPT, self.rd(INTERRUPT)); // clear stale status
        self.wr(ARG1, arg);
        if let Some(b) = blk {
            // PER TRANSFER, not once at init, because that is where Linux does it: `sdhci_config_dma`
            // runs from `sdhci_prepare_data`. The pre-clear value is kept so a failure can report
            // whether the field had drifted back.
            let c0 = self.rd(CONTROL0);
            self.last_ctrl0.set(c0);
            if c0 & CONTROL0_DMA_SELECT != 0 {
                self.wr(CONTROL0, c0 & !CONTROL0_DMA_SELECT);
            }
            self.wr(BLKSIZECNT, b);
            // READ IT BACK. If the block registers did not take, everything after this is a transfer
            // the controller was never set up for - and a zero block size gives it nothing to move and
            // no reason to report an error, which is exactly what a silent data phase looks like.
            // Recorded rather than acted on: the value is evidence for the caller's log line.
            self.last_blk.set(self.rd(BLKSIZECNT));
        }
        self.wr(CMDTM, code);
        // And the command word as the controller holds it, for the same reason: SDHCI starts a transfer
        // when a command with Data Present Select completes, so a controller that started nothing either
        // has a zero block size or does not have that bit set.
        self.last_cmdtm.set(self.rd(CMDTM));
        let mut t = 0u32;
        loop {
            let i = self.rd(INTERRUPT);
            if i & INT_CMD_DONE != 0 {
                break;
            }
            if i & (INT_ERR | INT_CMD_TIMEOUT) != 0 {
                // Record WHY before recovering: `reset_cmd_dat` clears INTERRUPT, so a caller that
                // logs the register afterwards reads 0 and cannot tell a timeout from an error.
                self.last_int.set(i);
                self.reset_cmd_dat();
                return None;
            }
            t += 1;
            if t > 2_000_000 {
                self.last_int.set(self.rd(INTERRUPT));
                self.reset_cmd_dat();
                return None;
            }
        }
        self.wr(INTERRUPT, INT_CMD_DONE);
        // Ncc: the SD spec wants at least 8 card-clock cycles between the end of one command and the
        // start of the next. At 400 kHz that is 20 us, and issuing back-to-back can catch the card
        // still driving CMD - which the controller reports as a line conflict (timeout AND CRC
        // together), the exact signature that cost the Pi 2's card a debugging session.
        for _ in 0..10 {
            spin();
        }
        Some(self.rd(RESP0))
    }

    /// Issue a command that carries a DATA phase, and move `buf` through the FIFO by PIO.
    ///
    /// `Err` names WHICH wait expired, because they mean different things and the interrupt register
    /// cannot tell them apart: `CMD_DONE` is cleared once the command lands, so a register reading zero
    /// while waiting for the FIFO is exactly what a healthy command looks like. "The command never
    /// issued" and "the command was fine and no data came" need different fixes.
    ///
    /// **The command phase is `cmd()`**, the same function CMD0, CMD3, CMD5, CMD7 and CMD52 all go
    /// through. This used to inline its own copy of that logic - the inhibit wait, the stale-status
    /// clear, the ARG1/CMDTM writes, the CMD_DONE poll - which is four chances to differ subtly from
    /// code already proven on this silicon. `block-driver`'s backend has the shape that works on this
    /// controller and this now matches it: wait DAT, set the block registers, `cmd()`, then the data.
    ///
    /// PIO, not DMA, for the two reasons `block-driver`'s backend gives: DMA on this SoC is not cache
    /// coherent without explicit maintenance, and these transfers are four bytes. Whether a firmware
    /// upload wants DMA is a MEASUREMENT for the phase that does one, not a guess for this one.
    pub fn cmd_data(&self, code: u32, arg: u32, blk: u32, buf: &mut [u32], read: bool)
        -> Result<(), &'static str>
    {
        let bytes = buf.len() * 4;
        if buf.is_empty() || bytes > 0xFFFF {
            return Err("the caller asked for a transfer this driver will not do");
        }
        // The DAT line before the block registers, which is the order the working backend uses.
        let mut t = 0u32;
        while self.rd(STATUS) & SR_DAT_INHIBIT != 0 {
            t += 1;
            if t > 1_000_000 {
                self.last_int.set(self.rd(INTERRUPT));
                return Err("the DAT line never came out of inhibit");
            }
        }
        // THE BLOCK REGISTERS COME FROM THE CALLER, because byte mode and block mode need different
        // words and only the caller knows which it is issuing (`blk_byte_mode` / `blk_block_mode`). It is
        // handed to `cmd_inner` rather than written here so it lands between the argument and the command,
        // exactly as the references' shadow-flush order puts it.

        // THE COMMAND PHASE IS THE ONE EVERY OTHER COMMAND USES. It clears stale status, writes ARG1,
        // the block registers, CMDTM, polls CMD_DONE, captures `last_int` on failure and resets the
        // lines.
        // KEEP THE RESPONSE. For a CMD53 this is the R5, whose flag byte says whether the card accepted
        // the transfer - and a refusal is indistinguishable, from the controller's side, from the data
        // phase simply not happening.
        match self.cmd_inner(code, arg, Some(blk)) {
            Some(r) => self.last_resp.set(r),
            None => return Err("the command itself did not complete"),
        }

        // Then the FIFO, one word at a time. The ready bit is latched, so it is cleared before each
        // word rather than once - otherwise the first word's flag would satisfy every later wait and
        // this would read the FIFO faster than the controller fills it.
        // EITHER SIGNAL SATISFIES THE WAIT, which is what u-boot's polled loop effectively does: the
        // interrupt status flag, or the buffer-enable bit in STATUS. Two registers, and a controller
        // need only say so in one of them.
        let ready = if read { INT_READ_RDY } else { INT_WRITE_RDY };
        let ready_st = if read { ST_BUF_READ_ENABLE } else { ST_BUF_WRITE_ENABLE };
        for w in buf.iter_mut() {
            let mut t = 0u32;
            loop {
                let i = self.rd(INTERRUPT);
                let s = self.rd(STATUS);
                // ACCUMULATE EVERYTHING SEEN, so a timeout can say whether the controller moved AT ALL.
                // Four hypotheses have been guessing at that; this asks it. If these come back as they
                // went in, nothing about the data phase happened.
                self.seen_int.set(self.seen_int.get() | i);
                self.seen_status.set(self.seen_status.get() | s);
                // WHEN, not just whether. A controller that goes active and inactive within a few
                // hundred polls gave up almost at once - a data timeout it declined to latch. One that
                // stays active for most of two million was waiting on a card that never spoke. Those
                // are different faults and only the timing separates them.
                if s & ST_DAT_ACTIVE != 0 {
                    if self.dat_first.get() == 0 {
                        self.dat_first.set(t + 1);
                    }
                    self.dat_last.set(t + 1);
                }
                if i & ready != 0 || s & ready_st != 0 {
                    break;
                }
                if i & (INT_ERR | INT_CMD_TIMEOUT) != 0 {
                    self.last_int.set(i);
                    self.reset_cmd_dat();
                    return Err("the controller reported an error during the data phase");
                }
                t += 1;
                if t > 2_000_000 {
                    self.last_int.set(self.rd(INTERRUPT));
                    self.reset_cmd_dat();
                    return Err("the FIFO never became ready - the command completed and no data came");
                }
            }
            self.wr(INTERRUPT, ready);
            if read {
                *w = self.rd(DATA);
            } else {
                self.wr(DATA, *w);
            }
        }

        // TRANSFER COMPLETE, waited for rather than assumed. The last FIFO access is not the end of the
        // transaction - the controller still has to finish on the bus - and issuing the next command
        // before it does is a line conflict.
        let mut t = 0u32;
        loop {
            let i = self.rd(INTERRUPT);
            if i & INT_DATA_DONE != 0 {
                break;
            }
            if i & (INT_ERR | INT_CMD_TIMEOUT) != 0 {
                self.last_int.set(i);
                self.reset_cmd_dat();
                return Err("the controller reported an error after the data moved");
            }
            t += 1;
            if t > 2_000_000 {
                self.last_int.set(self.rd(INTERRUPT));
                self.reset_cmd_dat();
                return Err("the data moved and the transfer never reported complete");
            }
        }
        self.wr(INTERRUPT, INT_DATA_DONE);
        for _ in 0..10 {
            spin(); // Ncc, as in `cmd`
        }
        Ok(())
    }

    /// After a command error both lines stay inhibited (SDHCI 3.10), so every later command would spin
    /// to its bound. Reset them (bounded) and clear the latched bits, so one transient does not wedge
    /// the driver.
    fn reset_cmd_dat(&self) {
        self.wr(CONTROL1, self.rd(CONTROL1) | C1_SRST_CMD | C1_SRST_DATA);
        let mut t = 0u32;
        while self.rd(CONTROL1) & (C1_SRST_CMD | C1_SRST_DATA) != 0 {
            t += 1;
            if t > 1_000_000 {
                break;
            }
        }
        self.wr(INTERRUPT, self.rd(INTERRUPT));
    }
}
