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
/// The FIFO can take a word.
const INT_WRITE_RDY: u32 = 1 << 4;
/// The FIFO has a word.
const INT_READ_RDY: u32 = 1 << 5;
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
        // 1-bit bus and no high-speed for now: every command this phase issues rides the CMD line
        // alone (CMD52 carries its one byte in the response), so 4-bit DAT and the 50 MHz mode are
        // work with nothing yet to carry. Written explicitly rather than inherited from whatever the
        // firmware left, so the starting state is a fact rather than a hope.
        self.wr(CONTROL0, self.rd(CONTROL0) & !((1 << 1) | (1 << 2)));
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
    /// `code` is the SDHCI `CMDTM` word: `index << 24 | RSPNS_TYPE << 16 | flags`. CRC and index
    /// checking are deliberately left OFF (bits 19/20 clear) because two of the responses this driver
    /// reads - R4 from CMD5 and R3 from an OCR query - carry neither a CRC7 nor a command index, so
    /// asking the controller to verify them fails a correct response.
    pub fn cmd(&self, code: u32, arg: u32) -> Option<u32> {
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
        self.wr(CMDTM, code);
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
    pub fn cmd_data(&self, code: u32, arg: u32, buf: &mut [u32], read: bool)
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
        // ONE block of `bytes`: block count in the high half, block size in the low. A four-byte
        // register read is a single block of four, which is what byte-mode CMD53 asks for.
        self.wr(BLKSIZECNT, (1 << 16) | (bytes as u32 & 0xFFFF));

        // THE COMMAND PHASE IS THE PROVEN ONE. It clears stale status, writes ARG1/CMDTM, polls
        // CMD_DONE, captures `last_int` on failure and resets the lines - all of it already exercised
        // by every other command this driver issues.
        // KEEP THE RESPONSE. For a CMD53 this is the R5, whose flag byte says whether the card
        // accepted the transfer - and a refusal is indistinguishable, from the controller's side, from
        // the data phase simply not happening.
        match self.cmd(code, arg) {
            Some(r) => self.last_resp.set(r),
            None => return Err("the command itself did not complete"),
        }

        // Then the FIFO, one word at a time. The ready bit is latched, so it is cleared before each
        // word rather than once - otherwise the first word's flag would satisfy every later wait and
        // this would read the FIFO faster than the controller fills it.
        let ready = if read { INT_READ_RDY } else { INT_WRITE_RDY };
        for w in buf.iter_mut() {
            let mut t = 0u32;
            loop {
                let i = self.rd(INTERRUPT);
                if i & ready != 0 {
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
