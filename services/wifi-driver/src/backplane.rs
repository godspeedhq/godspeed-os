// SPDX-License-Identifier: GPL-2.0-only
//! The chip's internal bus, and the one register that settles what this part actually is.
//!
//! **Why this exists now rather than later.** The CIS said manufacturer `0x02D0` and device `0xA9A6`,
//! and `0xA9A6` is 43430 in decimal where the part this board is documented to carry - the CYW43455 -
//! answers `0xA9BF`, which is 43455. The CIS read is trustworthy (the manufacturer came back exactly
//! right, out of the same four-byte tuple body), so the disagreement is real and it decides which
//! firmware blob phase 2 must upload. A document cannot settle it. The silicon can: the chipcommon
//! core's first register carries the chip's own id and revision, and reading it is the authoritative
//! answer.
//!
//! It is also not extra work. Everything below is the opening of the firmware upload path - the
//! backplane window, the clock request, the 32-bit register access - so the identity question is
//! answered as a side effect of the step that had to come next anyway.
//!
//! ## How a host reaches inside this chip
//!
//! The radio's internal bus is not memory-mapped anywhere the host can see. It is reached through
//! **SDIO function 1**, whose 17-bit address space is split:
//!
//! ```text
//!   0x00000 .. 0x07FFF   a 32 KiB WINDOW onto the backplane, wherever the window is currently set
//!   0x08000             the same window, flagged as a 2-or-4-byte access rather than a single byte
//!   0x1000A .. 0x1000F  the function's own control registers, which is where the window is SET
//! ```
//!
//! So a 32-bit read of backplane address `A` is three steps: point the window at `A & 0xFFFF8000` by
//! writing three bytes, then read at `(A & 0x7FFF) | 0x8000`, which the chip answers as a four-byte
//! transfer. The window is only rewritten when it actually changes, because each write is a command on
//! the bus.
//!
//! ## Reference
//!
//! Register names, offsets and the clock-request handshake follow Linux's
//! `drivers/net/wireless/broadcom/brcm80211/brcmfmac/sdio.h` and `sdio.c`, read as an executable
//! datasheet (§26.14): what the silicon needs written, in what order, and what it does when you get it
//! wrong. No code is taken. In particular the `CHIPCLKCSR` write-then-verify below is that driver's own
//! first sanity check on a chip it has just enabled, and it is kept for the reason it exists there - it
//! is the cheapest question that distinguishes "function 1 reported ready" from "the backplane is
//! actually alive".

use godspeed_sdk::ServiceContext;

use crate::host::Host;
use crate::sdio;

/// Function 1's own control registers. Above the window, so reaching them never disturbs it.
mod f1 {
    /// Backplane window, address bits [15:8].
    pub const SBADDRLOW: u32 = 0x1_000A;
    /// Backplane window, address bits [23:16].
    pub const SBADDRMID: u32 = 0x1_000B;
    /// Backplane window, address bits [31:24].
    pub const SBADDRHIGH: u32 = 0x1_000C;
    /// Chip clock control and status.
    pub const CHIPCLKCSR: u32 = 0x1_000E;
}

/// `CHIPCLKCSR` bits. Only the four this step uses are named.
mod clk {
    /// Force the ALP clock on. Not used here; named because it is one of the writable bits, and the
    /// mask below has to cover every bit a host can write or it would forgive a real failure.
    pub const FORCE_ALP: u8 = 0x01;
    /// Force the HT clock on. As above.
    pub const FORCE_HT: u8 = 0x02;
    /// Force the ILP clock on. As above.
    pub const FORCE_ILP: u8 = 0x04;
    /// Request the ALP (active low power) clock, which is what the backplane needs to answer.
    pub const ALP_AVAIL_REQ: u8 = 0x08;
    /// Request the HT clock. Not requested here, and part of the writable mask for the same reason.
    pub const HT_AVAIL_REQ: u8 = 0x10;
    /// Stop the hardware asserting its own clock request, so ours is the only one in play.
    pub const FORCE_HW_CLKREQ_OFF: u8 = 0x20;
    /// The ALP clock is available.
    pub const ALP_AVAIL: u8 = 0x40;
    /// The HT (high throughput) clock is available. Not requested here; reported because a chip that
    /// already has it says something about what state it was left in.
    pub const HT_AVAIL: u8 = 0x80;

    /// What to write first: request ALP, and take the hardware's own request out of the picture.
    pub const INIT: u8 = FORCE_HW_CLKREQ_OFF | ALP_AVAIL_REQ;

    /// The bits of this register a HOST WRITES. Everything above them - `ALP_AVAIL` and `HT_AVAIL` - is
    /// read-only status the hardware sets.
    ///
    /// **This mask is the whole correction.** The readback used to be compared for exact equality with
    /// what was written, which asks the register a question it cannot answer: a working chip grants the
    /// clock, which SETS a status bit, so the value read back is legitimately different from the value
    /// written. On the Pi 4 the driver wrote `0x28`, read `0x68`, and reported that the write had not
    /// stuck - when `0x68` is `0x28` plus `ALP_AVAIL`, i.e. the request stuck AND the clock was already
    /// granted. That is the success case, rejected.
    pub const REQUEST_BITS: u8 = FORCE_ALP | FORCE_HT | FORCE_ILP
        | ALP_AVAIL_REQ | HT_AVAIL_REQ | FORCE_HW_CLKREQ_OFF;
}

/// The window is 32 KiB, so an address's low 15 bits are the offset within it.
const WINDOW_MASK: u32 = 0xFFFF_8000;
const OFFSET_MASK: u32 = 0x0000_7FFF;
/// Set on the function-1 address to say "this is a 2-or-4-byte access, not a single byte".
const ACCESS_WIDE: u32 = 0x0000_8000;

/// The chipcommon core, which is at a fixed backplane address on every part in this family. Its first
/// register is the one worth all of the above.
const CHIPCOMMON_BASE: u32 = 0x1800_0000;

/// Whatever the chip says it is.
pub struct ChipId {
    /// Raw register, kept so the log can print it and a reader can decode it themselves.
    pub raw: u32,
    /// Low 16 bits. Broadcom writes some of these in decimal and some in hex, which is exactly the
    /// confusion this whole module exists to resolve - see `describe`.
    pub id: u16,
    pub rev: u8,
    pub package: u8,
    /// 0 = SB, 1 = AXI/AI. Decides how the core list is walked, which a later phase needs.
    pub chip_type: u8,
}

impl ChipId {
    /// What this id means, named rather than left as a number.
    ///
    /// **The two candidates are the whole point.** `43430` is the part the CIS device code pointed at
    /// (`0xA9A6` is 43430 in decimal); `0x4345` is the 4345 family, of which the CYW43455 is one
    /// variant distinguished by revision. Note that these two are written in DIFFERENT BASES in
    /// Broadcom's own headers - 43430 decimal, 0x4345 hex - which is the trap that made the CIS code
    /// look like a contradiction in the first place.
    pub fn describe(&self) -> &'static str {
        match self.id {
            43430 => "BCM43430 - 2.4 GHz only, the Pi 3 / Zero W part",
            43439 => "BCM43439",
            0x4345 => "the 4345 family - CYW43455 is this chip at a particular revision",
            0x4335 => "BCM4335/4339",
            0x4359 => "BCM4359",
            0x4373 => "BCM4373",
            _ => "not a part this driver has a name for",
        }
    }
}

/// Whether the window currently points at `base`, so an unchanged window costs no commands.
///
/// Owned by the caller rather than stored in a module static: Commandment VI forbids unowned global
/// mutable state in a service, and the build refuses it outright.
pub struct Window(Option<u32>);

impl Window {
    pub fn new() -> Self {
        // `None` rather than 0, because 0 is a legitimate window and starting there would skip the
        // first write - leaving the chip pointed wherever its reset left it while this code believed
        // otherwise. An unknown window and a zero window are different facts.
        Window(None)
    }

    /// Point the backplane window at the 32 KiB region containing `addr`.
    ///
    /// Writes only the bytes that CHANGE, which is what `brcmf_sdiod_set_backplane_window` does and
    /// for the same reason: each one is a command on a bus, and three commands per register read would
    /// dominate a firmware upload made of thousands of them.
    fn set(&mut self, h: &Host, addr: u32, ctx: &ServiceContext) -> bool {
        let base = addr & WINDOW_MASK;
        if self.0 == Some(base) {
            return true;
        }
        let old = self.0.unwrap_or(!base); // unknown: force all three
        for (shift, reg, what) in [
            (8u32, f1::SBADDRLOW, "SBADDRLOW"),
            (16, f1::SBADDRMID, "SBADDRMID"),
            (24, f1::SBADDRHIGH, "SBADDRHIGH"),
        ] {
            let byte = ((base >> shift) & 0xFF) as u8;
            if ((old >> shift) & 0xFF) as u8 == byte {
                continue;
            }
            if sdio::write_reg(h, 1, reg, byte).is_none() {
                ctx.log_fmt(format_args!(
                    "wifi-driver: could not set the backplane window ({} = {:#04x}, for address \
                     {:#010x}) - INT={:#010x}",
                    what,
                    byte,
                    addr,
                    h.last_int()
                ));
                // The window is now PARTLY written, so what it points at is unknown. Say so, rather
                // than leaving a cached value that would make the next read silently skip a write it
                // needed - which would be a wrong answer instead of an error.
                self.0 = None;
                return false;
            }
        }
        self.0 = Some(base);
        true
    }

    /// Read one 32-bit backplane register.
    pub fn read32(&mut self, h: &Host, addr: u32, ctx: &ServiceContext) -> Option<u32> {
        if !self.set(h, addr, ctx) {
            return None;
        }
        sdio::read32(h, 1, (addr & OFFSET_MASK) | ACCESS_WIDE, ctx)
    }
}

/// Wake the backplane and confirm it is actually answering.
///
/// Two questions, and they are different. **Does function 1 accept a write to its own control
/// register and read it back unchanged** - which is the cheapest test that distinguishes a live chip
/// from a function that merely reported ready - and **is the ALP clock available**, which is what the
/// backplane needs before it will answer at all.
///
/// Bounded, and the bound is reported. A chip that never grants the clock is a real condition and
/// silently continuing into a register read would produce a number that means nothing.
pub fn wake(h: &Host, ctx: &ServiceContext) -> bool {
    if sdio::write_reg(h, 1, f1::CHIPCLKCSR, clk::INIT).is_none() {
        ctx.log_fmt(format_args!(
            "wifi-driver: the write to CHIPCLKCSR was refused - INT={:#010x}. Function 1 reported \
             ready, so the chip is enabled and its control register is not answering",
            h.last_int()
        ));
        return false;
    }
    // READ IT BACK AND REQUIRE THE BITS WE WROTE - not the whole register. A write that is accepted
    // and does not stick means the bus is talking to something that is not this register, and every
    // later read would silently inherit that; so the check stays. But `CHIPCLKCSR` mixes our request
    // bits with the hardware's status bits, and comparing the whole register asks it a question it
    // cannot answer: a chip that GRANTS the clock sets a status bit, so the readback is legitimately
    // different from the write. Exactly that happened on the Pi 4 - wrote 0x28, read 0x68 - and the
    // success case was reported as a failure.
    match sdio::read_reg(h, 1, f1::CHIPCLKCSR) {
        Some(v) if v & clk::REQUEST_BITS == clk::INIT => {}
        Some(v) => {
            ctx.log_fmt(format_args!(
                "wifi-driver: CHIPCLKCSR wrote {:#04x} and the request bits read back {:#04x} (whole \
                 register {:#04x}) - the write was accepted and did not stick, so the backplane is not \
                 reachable and no register read below it would mean anything",
                clk::INIT,
                v & clk::REQUEST_BITS,
                v
            ));
            return false;
        }
        None => {
            ctx.log_fmt(format_args!(
                "wifi-driver: could not read CHIPCLKCSR back - INT={:#010x}",
                h.last_int()
            ));
            return false;
        }
    }

    /// Each attempt is one CMD52 - microseconds - so this is a generous number of asks rather than a
    /// long wall-clock wait. A count is not a duration, which is why the failure reports the count.
    const CLOCK_TRIES: u32 = 500;
    for attempt in 0..CLOCK_TRIES {
        match sdio::read_reg(h, 1, f1::CHIPCLKCSR) {
            Some(v) if v & clk::ALP_AVAIL != 0 => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: backplane awake - CHIPCLKCSR {:#04x}, ALP available after {} \
                     read(s){}",
                    v,
                    attempt + 1,
                    if v & clk::HT_AVAIL != 0 { ", and HT too" } else { "" }
                ));
                return true;
            }
            Some(_) => {}
            None => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: a CHIPCLKCSR read failed while waiting for the ALP clock - \
                     INT={:#010x}",
                    h.last_int()
                ));
                return false;
            }
        }
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: the chip never reported the ALP clock available across {} reads of CHIPCLKCSR. \
         The register answers, so the chip is there and its clock is not coming up",
        CLOCK_TRIES
    ));
    false
}

/// Read the identity register as FOUR single-byte CMD52 reads. **A diagnostic, not the way to do this.**
///
/// The wide-access flag exists precisely so the bridge performs one 32-bit fetch, and a byte read may or
/// may not assemble a 32-bit register correctly - so this is not how a driver should read the backplane
/// and it is not proposed as one. It is here because CMD52 is the command that demonstrably works on this
/// bus, and it splits the remaining question in two:
///
///   * a plausible id means the WINDOW and the ADDRESS are right, the fault is confined to the CMD53 data
///     phase, and the firmware question is answered as a side effect of finding that out;
///   * garbage or a refusal means the window or the address is wrong, and the data phase was never the
///     problem.
///
/// Note the address has NO wide-access flag: these are genuine single-byte reads at consecutive window
/// offsets, which is the only shape CMD52 has.
pub fn chip_id_via_cmd52(h: &Host, w: &mut Window, ctx: &ServiceContext) -> Option<ChipId> {
    if !w.set(h, CHIPCOMMON_BASE, ctx) {
        return None;
    }
    let base = CHIPCOMMON_BASE & OFFSET_MASK;
    let mut raw = 0u32;
    for i in 0..4u32 {
        match sdio::read_reg(h, 1, base + i) {
            Some(b) => raw |= (b as u32) << (8 * i),
            None => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: the CMD52 fallback could not read byte {} of the identity register \
                     (function 1 address {:#07x}) - INT={:#010x}. So CMD52 cannot reach this window \
                     either, and the WINDOW or the ADDRESS is the suspect rather than the data phase",
                    i,
                    base + i,
                    h.last_int()
                ));
                return None;
            }
        }
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: the CMD52 fallback read {:#010x} from the identity register - so the window and \
         the address ARE right, and the fault is confined to the CMD53 data phase",
        raw
    ));
    if raw == 0 || raw == 0xFFFF_FFFF {
        ctx.log_fmt(format_args!(
            "wifi-driver:   but {:#010x} is the bus answering with nothing rather than a chip \
             identifying itself, so this says the bytes arrived and not that they are the register",
            raw
        ));
        return None;
    }
    Some(ChipId {
        raw,
        id: (raw & 0xFFFF) as u16,
        rev: ((raw >> 16) & 0xF) as u8,
        package: ((raw >> 20) & 0xF) as u8,
        chip_type: ((raw >> 28) & 0xF) as u8,
    })
}

/// Read the chipcommon core's identity register - the answer this whole module is for.
pub fn chip_id(h: &Host, w: &mut Window, ctx: &ServiceContext) -> Option<ChipId> {
    let raw = match w.read32(h, CHIPCOMMON_BASE, ctx) {
        Some(v) => v,
        None => {
            ctx.log("wifi-driver: the chipcommon identity register could not be read");
            return None;
        }
    };
    // A register that reads all-zeros or all-ones is the bus answering with nothing, not a chip
    // identifying itself. Said out loud, because a chip id of 0 would otherwise be reported as a part
    // nobody has a name for - a wrong answer rather than an error.
    if raw == 0 || raw == 0xFFFF_FFFF {
        ctx.log_fmt(format_args!(
            "wifi-driver: the chipcommon identity register reads {:#010x}, which is the bus answering \
             with nothing rather than a chip identifying itself",
            raw
        ));
        return None;
    }
    Some(ChipId {
        raw,
        id: (raw & 0xFFFF) as u16,
        rev: ((raw >> 16) & 0xF) as u8,
        package: ((raw >> 20) & 0xF) as u8,
        chip_type: ((raw >> 28) & 0xF) as u8,
    })
}
