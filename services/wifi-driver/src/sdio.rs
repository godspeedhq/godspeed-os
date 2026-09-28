// SPDX-License-Identifier: GPL-2.0-only
//! The SDIO card protocol: enough of it to identify what is on the bus and name it out loud.
//!
//! **SDIO is not SD-with-extras, and the difference is what this file is.** A memory card is
//! identified by CMD8's voltage handshake, ACMD41's OCR negotiation and a CSD that describes a
//! capacity. An I/O card answers none of those. It is identified by **CMD5** (`IO_SEND_OP_COND`),
//! which reports how many I/O functions it has, and it is then read through **CMD52**
//! (`IO_RW_DIRECT`) - one register byte per command, carried in the response on the CMD line, with no
//! data transfer and no DAT lines involved at all. That is why this phase needs no DMA arena, no
//! 4-bit bus and no block transfers: everything below rides the command line.
//!
//! ## The order, and why CMD8 is absent
//!
//! `mmc_attach_sdio` in Linux is the reference for the sequence (behaviour cited per §26.14; no code
//! taken): go idle, CMD5 with a zero argument to ASK, CMD5 with the reported voltage window to
//! COMMIT, CMD3 for a relative address, CMD7 to select it. CMD8 is skipped deliberately rather than
//! forgotten - it is the memory card's interface-condition query, and an I/O-only card is entitled to
//! ignore it. Issuing it would produce a timeout that looks like a fault.
//!
//! ## What "the radio is there" actually means
//!
//! A controller answering its version register says a HOST CONTROLLER exists - the kernel's boot
//! census establishes that much and says so. It does not say a radio is behind it. The proof is the
//! **CIS**: a linked list of tuples in the card's own address space, whose `CISTPL_MANFID` carries a
//! manufacturer code and a device code. Broadcom is `0x02D0`; the CYW43455 answers `0xA9BF`. Reading
//! those two numbers off the bus is the difference between believing a device tree and having asked
//! the part, which is the same distinction the kernel census was written for.

use godspeed_sdk::ServiceContext;

use crate::host::Host;

// SDHCI `CMDTM` words: `index << 24 | RSPNS_TYPE << 16`. RSPNS 0 = none, 2 = 48-bit, 3 = 48-bit+busy.
// CRC and index checking stay off - see `Host::cmd`.
const CMD_GO_IDLE: u32 = 0x0000_0000; // CMD0, no response
const CMD_IO_SEND_OP_COND: u32 = 0x0502_0000; // CMD5  -> R4
const CMD_SEND_REL_ADDR: u32 = 0x0302_0000; // CMD3  -> R6
const CMD_SELECT_CARD: u32 = 0x0703_0000; // CMD7  -> R1b
const CMD_IO_RW_DIRECT: u32 = 0x3402_0000; // CMD52 -> R5
/// CMD53 (`IO_RW_EXTENDED`), read: `0x353A_0012`.
///
/// **Every field here was read off Linux rather than reasoned about, after two flashes lost to
/// reasoning.** `sdhci_send_command` builds the command flags from the mmc response flags and
/// `sdhci_set_transfer_mode` builds the transfer mode, and for a single-block PIO read of an SDIO
/// register that comes to command flags `0x3A` and transfer mode `0x0012`:
///
/// ```text
///   0x3A = RESP_SHORT (0x02) | CMD_CRC (0x08) | CMD_INDEX (0x10) | CMD_DATA (0x20)
///   0x12 = TM_BLKCNT_EN (0x02) | TM_DAT_DIR read (0x10)
/// ```
///
/// **`CMD_CRC` and `CMD_INDEX` were missing**, because `cmd`'s old comment left them off for every
/// command on the grounds that CMD5's R4 carries neither a CRC7 nor an index. True of R4; R5 carries
/// both, and `MMC_RSP_R5 = PRESENT | CRC | OPCODE` is where Linux gets them. One template, applied to a
/// command it did not fit.
///
/// The transfer mode already matched exactly, which is why the previous flash's `TM_BLKCNT_EN` was right
/// and did not help.
///
/// **`TM_BLKCNT_EN` was missing and is the fix this constant exists to record.** The spec's wording
/// invites you to leave it off for a single block - the block count is "not used" there - and on hardware
/// the consequence was exact: the card ACCEPTED the transfer (R5 flags clean) and the controller ran no
/// data phase at all, leaving `STATUS` with no DAT activity and no buffer to read. A controller whose
/// block count is not enabled is entitled to read that count as zero and move nothing, which is what
/// this looked like.
///
/// Linux is unambiguous about it: `sdhci_set_transfer_mode` opens with `mode = SDHCI_TRNS_BLK_CNT_EN` for
/// **any** command that carries data, and only then adds the multi-block bits. Cited per §26.14; the bit
/// is the silicon's requirement, not their design.
///
/// `block-driver`'s CMD17 gets away without it on a 512-byte block, which is why the shape was copied
/// from there and came up short. The two differ in exactly two ways - the index and the block size - and
/// that made this the only bit worth suspecting.
const CMD_IO_RW_EXTENDED_READ: u32 = 0x353A_0012;
/// CMD53, write: the same command flags (the data-present bit is set either way) without the direction
/// bit in the transfer mode. Used by `write32`.
const CMD_IO_RW_EXTENDED_WRITE: u32 = 0x353A_0002;

// THE OTHER COMMANDS ARE LEFT ALONE, DELIBERATELY. Their R5/R6/R1b responses carry a CRC7 and an index
// too, so by the same reading their check bits should be on as well - `0x341A_0000` for CMD52,
// `0x031A_0000` for CMD3, `0x071B_0000` for CMD7, with CMD5's `0x0502_0000` correct as it stands because
// R4 genuinely has neither. But all of them WORK, and changing a working command in the same image as
// the fix for a broken one makes the result unattributable. Recorded here so the next reader knows the
// inconsistency is a choice and what the values would be (§26.7).

/// The voltage window to ask for: the 3.2-3.4 V bits of the OCR.
///
/// Deliberately NOT bit 24 (S18R, "switch to 1.8 V signalling"). That request must be followed by a
/// CMD11 voltage switch, and asking for a switch this driver does not perform is a way to leave a real
/// card unresponsive - invisible under emulation, where the negotiation is not modelled. The same
/// omission is spelled out in `block-driver`'s ACMD41 for the same reason.
const OCR_3V3: u32 = 0x00FF_8000;

/// R5's response-flag byte, which `RESP0[15:8]` carries. Any of these set means the card refused.
///
/// bit7 COM_CRC_ERROR, bit6 ILLEGAL_COMMAND, bit3 ERROR, bit1 FUNCTION_NUMBER, bit0 OUT_OF_RANGE.
/// Bits 5:4 are IO_CURRENT_STATE and are not errors.
const R5_ERRORS: u32 = 0x80 | 0x40 | 0x08 | 0x02 | 0x01;

/// A function's own Basic Registers, which live in FUNCTION 0's address space.
///
/// `SDIO_FBR_BASE(f) = f * 0x100` and `SDIO_FBR_BLKSIZE = 0x10`, both quoted from
/// `include/linux/mmc/sdio.h` rather than recalled - so function 1's block size is at `0x110`/`0x111`
/// and is written by a CMD52 to function **0**, not to function 1.
mod fbr {
    /// Where function `f`'s basic registers start.
    pub const fn base(f: u8) -> u32 {
        (f as u32) * 0x100
    }
    /// Block size, two bytes little-endian at `base + 0x10`.
    pub const BLKSIZE: u32 = 0x10;
}

/// Function 0's Card Common Control Registers - the fixed part every SDIO card has.
mod cccr {
    /// CCCR/SDIO specification revision (low nibble CCCR, high nibble SDIO).
    pub const REVISION: u32 = 0x00;
    /// I/O Enable: one bit per function.
    pub const IO_ENABLE: u32 = 0x02;
    /// I/O Ready: one bit per function, set when that function is ready to be used.
    pub const IO_READY: u32 = 0x03;
    /// Bus Interface Control (bus width in the low two bits).
    pub const BUS_IFACE: u32 = 0x07;
    /// Card Capability.
    pub const CARD_CAPS: u32 = 0x08;
    /// Common CIS pointer, three bytes little-endian at 0x09..0x0C.
    pub const CIS_PTR: u32 = 0x09;
}

/// CIS tuple codes. Only the two worth reading here are named; the walk skips the rest by length.
mod cistpl {
    /// End of the tuple chain.
    pub const END: u8 = 0xFF;
    /// A no-op tuple with no length byte at all - the one special case in the walk.
    pub const NULL: u8 = 0x00;
    /// Manufacturer identification: manufacturer code then device code, both u16 little-endian.
    pub const MANFID: u8 = 0x20;
    /// Function identification.
    pub const FUNCID: u8 = 0x21;
}

/// What the card said about itself.
pub struct Card {
    /// Relative card address, from CMD3. Every later command addresses the card by this.
    pub rca: u16,
    /// How many I/O functions it has, from CMD5's R4. A radio has at least one; zero would mean this
    /// is not an I/O card at all.
    pub funcs: u8,
    /// Whether it ALSO presents memory (a combo card). Reported because it changes nothing here and
    /// would be surprising to discover later.
    pub memory: bool,
    /// The I/O OCR the card reported - the voltage window it can work in.
    pub ocr: u32,
}

/// The identifying pair from the CIS: `(manufacturer, device)`.
pub struct Manfid {
    pub manf: u16,
    pub device: u16,
}

impl Manfid {
    /// Broadcom's SDIO manufacturer code, which is the part of this tuple worth checking.
    pub const BROADCOM: u16 = 0x02D0;

    /// Is the manufacturer Broadcom? The only verdict this tuple can actually support.
    pub fn is_broadcom(&self) -> bool {
        self.manf == Self::BROADCOM
    }

    // WHAT THIS TUPLE DOES *NOT* DECIDE, and why a check here was misleading for five boots.
    //
    // This used to carry a `CYW43455 = 0xA9BF` constant and an `is_expected_radio` that compared the CIS
    // device code against it - so on a board reporting `0xA9A6` it announced on every boot that the part
    // "is NOT the expected radio", and that was taken seriously enough to drive a prediction.
    //
    // **The CIS device code is not the field that identifies the part for any purpose this driver has.**
    // brcmfmac matches a DRIVER to a device on the SDIO id and selects FIRMWARE from the CHIP ID read
    // over the backplane, and on this board those disagree: the CIS says `0xA9A6` while the silicon says
    // `0x4345` rev 6. Nothing requires the two to name the same part number, so comparing the CIS code
    // to an expected value was asking a question it cannot answer.
    //
    // So the manufacturer is checked (it is meaningful, and it confirmed the tuple was being read
    // correctly all along) and the device code is REPORTED. The verdict lives with `ChipId`, which is
    // the field that decides.
}

/// Why a CMD52 did not produce a byte. **Two different facts that `None` used to conflate**, which is
/// the same gap CMD53 had one layer up: a command that never completed and a card that refused need
/// different fixes, and a caller that cannot tell them apart cannot say which it hit.
#[derive(Clone, Copy)]
pub enum ReadFail {
    /// The command did not complete - the controller's problem, or nothing listening.
    NoAnswer,
    /// The card answered and refused, carrying its R5 flag byte.
    Refused(u32),
}

impl ReadFail {
    /// One phrase naming the failure, for a caller's log line.
    pub fn describe(&self) -> &'static str {
        match self {
            ReadFail::NoAnswer => "the command never completed",
            ReadFail::Refused(_) => "the card REFUSED it",
        }
    }
    /// The R5 flag byte, or 0 when the card never answered to set one.
    pub fn flags(&self) -> u32 {
        match self {
            ReadFail::NoAnswer => 0,
            ReadFail::Refused(f) => *f,
        }
    }
}

/// Read one register byte through CMD52, saying WHICH way it failed.
pub fn read_reg_detail(h: &Host, func: u8, addr: u32) -> Result<u8, ReadFail> {
    // CMD52 argument: bit31 R/W (0 = read), bits30:28 function, bit27 RAW, bits25:9 address,
    // bits7:0 write data.
    let arg = ((func as u32 & 0x7) << 28) | ((addr & 0x1_FFFF) << 9);
    let resp = match h.cmd(CMD_IO_RW_DIRECT, arg) {
        Some(r) => r,
        None => return Err(ReadFail::NoAnswer),
    };
    // R5 occupies response bits [39:8], which is exactly RESP0 - so the read DATA is the low byte and
    // the flags are the next one up.
    let flags = (resp >> 8) & 0xFF;
    if flags & R5_ERRORS != 0 {
        return Err(ReadFail::Refused(flags));
    }
    Ok((resp & 0xFF) as u8)
}

/// Read one register byte through CMD52. `None` means the card did not answer or refused.
///
/// The short form, for the callers that only need the byte. Reach for `read_reg_detail` where WHICH
/// failure it was decides what to say next.
pub fn read_reg(h: &Host, func: u8, addr: u32) -> Option<u8> {
    read_reg_detail(h, func, addr).ok()
}

/// Write one register byte through CMD52. `None` means the card did not answer or refused.
pub fn write_reg(h: &Host, func: u8, addr: u32, val: u8) -> Option<()> {
    let arg = (1 << 31) | ((func as u32 & 0x7) << 28) | ((addr & 0x1_FFFF) << 9) | val as u32;
    let resp = h.cmd(CMD_IO_RW_DIRECT, arg)?;
    if (resp >> 8) & R5_ERRORS != 0 {
        return None;
    }
    Some(())
}

/// Identify the card on the bus: CMD0, CMD5 twice, CMD3, CMD7.
///
/// Every step names itself on failure. That is not verbosity - on this board each one fails for a
/// different reason and they need different fixes. CMD5 timing out means the radio is not reachable at
/// all (its power domain, or the GPIO34-39 mux, both of which the kernel reports at boot). CMD5
/// answering but never becoming ready means the voltage window was refused. CMD3 failing after a ready
/// CMD5 means the card is listening and the bus is marginal.
pub fn identify(h: &Host, ctx: &ServiceContext) -> Option<Card> {
    // CMD0 has no response, so its "success" says only that the controller accepted it. Its value is
    // putting a card that some earlier owner left mid-transaction back into the idle state.
    if h.cmd(CMD_GO_IDLE, 0).is_none() {
        ctx.log_fmt(format_args!(
            "wifi-driver: CMD0 (GO_IDLE) did not complete - STATUS={:#010x} INT={:#010x}. That is the \
             CONTROLLER failing, not the card: CMD0 expects no response",
            h.status(),
            h.last_int()
        ));
        return None;
    }

    // CMD5 with a zero argument ASKS rather than commits: the card reports its I/O OCR and function
    // count without being told a voltage.
    let probe = match h.cmd(CMD_IO_SEND_OP_COND, 0) {
        Some(v) => v,
        None => {
            ctx.log_fmt(format_args!(
                "wifi-driver: CMD5 (IO_SEND_OP_COND) got NO ANSWER - STATUS={:#010x} INT={:#010x}",
                h.status(),
                h.last_int()
            ));
            ctx.log(
                "wifi-driver:   nothing is on this bus. The controller is ours (its version register \
                 answered), so suspect the two things the KERNEL reports at boot: the SD power domain \
                 (`sdio: SET_POWER_STATE`) and the GPIO34-39 mux (`sdio: GPIO34-39 fsel=`)",
            );
            return None;
        }
    };
    // R4: bit31 C (ready), bits30:28 function count, bit27 memory present, bits23:0 I/O OCR.
    let funcs = ((probe >> 28) & 0x7) as u8;
    let memory = probe & (1 << 27) != 0;
    let ocr = probe & 0x00FF_FFFF;
    ctx.log_fmt(format_args!(
        "wifi-driver: CMD5 answered R4={:#010x} - {} I/O function(s), memory {}, I/O OCR {:#08x}",
        probe,
        funcs,
        if memory { "present" } else { "absent" },
        ocr
    ));
    if funcs == 0 {
        ctx.log(
            "wifi-driver: ZERO I/O functions - something answered CMD5 but it is not an I/O card, so \
             there is no radio here to drive",
        );
        return None;
    }

    // CMD5 again, now WITH a voltage window, repeatedly until the card reports ready. A card still
    // powering up is entitled to answer not-ready; the spec's sequence is to repeat. Bounded, and the
    // bound is reported (a silent give-up here is indistinguishable from success on the next line).
    const READY_TRIES: u32 = 100;
    let mut tries = 0u32;
    let ready = loop {
        match h.cmd(CMD_IO_SEND_OP_COND, OCR_3V3 & ocr) {
            Some(v) if v & (1 << 31) != 0 => break Some(v),
            Some(_) => {}
            None => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: CMD5 (with the voltage window) stopped answering after {} \
                     attempt(s) - INT={:#010x}",
                    tries,
                    h.last_int()
                ));
                break None;
            }
        }
        tries += 1;
        if tries >= READY_TRIES {
            ctx.log_fmt(format_args!(
                "wifi-driver: the card never reported READY across {} CMD5 attempts. It is on the bus \
                 and answering, so the voltage window ({:#08x}) is the suspect",
                READY_TRIES,
                OCR_3V3 & ocr
            ));
            break None;
        }
    };
    ready?;
    if tries > 0 {
        ctx.log_fmt(format_args!("wifi-driver: the card reported ready after {} extra CMD5(s)", tries));
    }

    // CMD3: the card publishes a relative address. R6 carries it in bits [31:16] of the payload.
    let r6 = match h.cmd(CMD_SEND_REL_ADDR, 0) {
        Some(v) => v,
        None => {
            ctx.log_fmt(format_args!(
                "wifi-driver: CMD3 (SEND_RELATIVE_ADDR) failed - INT={:#010x}. The card was READY one \
                 command ago, so this is the bus rather than the device",
                h.last_int()
            ));
            return None;
        }
    };
    let rca = ((r6 >> 16) & 0xFFFF) as u16;
    if rca == 0 {
        ctx.log("wifi-driver: CMD3 returned RCA 0, which no selected card may have");
        return None;
    }

    // CMD7 with the RCA moves the card from stand-by into the transfer state, which is the only state
    // in which CMD52 is answered.
    if h.cmd(CMD_SELECT_CARD, (rca as u32) << 16).is_none() {
        ctx.log_fmt(format_args!(
            "wifi-driver: CMD7 (SELECT_CARD rca={:#06x}) failed - INT={:#010x}",
            rca,
            h.last_int()
        ));
        return None;
    }
    ctx.log_fmt(format_args!("wifi-driver: card selected, RCA {:#06x}", rca));
    Some(Card { rca, funcs, memory, ocr })
}

/// Read function 0's common CIS pointer - three bytes little-endian at CCCR 0x09.
pub fn cis_pointer(h: &Host, ctx: &ServiceContext) -> Option<u32> {
    let mut p = 0u32;
    for i in 0..3u32 {
        match read_reg(h, 0, cccr::CIS_PTR + i) {
            Some(b) => p |= (b as u32) << (8 * i),
            None => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: CMD52 read of the CIS pointer byte {} failed - INT={:#010x}",
                    i,
                    h.last_int()
                ));
                return None;
            }
        }
    }
    if p == 0 {
        ctx.log("wifi-driver: the card reports a CIS pointer of 0, which cannot be walked");
        return None;
    }
    Some(p)
}

/// Write one 32-bit register through CMD53 in byte mode - the twin of `read32`.
///
/// Same argument shape with the write bit set, same four-byte byte-mode transfer, same wide-access
/// flag expectation on the caller's address. The constant it uses already existed with
/// `#[allow(dead_code)]` waiting for a caller; selecting a CR4 memory bank is the first one, and the
/// firmware upload is the reason the path had to exist at all.
pub fn write32(h: &Host, func: u8, addr: u32, val: u32, ctx: &ServiceContext) -> Option<()> {
    // Bit 31 set = write. Otherwise identical to the read: byte mode, incrementing address, four
    // bytes.
    let arg = (1 << 31) | ((func as u32 & 0x7) << 28) | (1 << 26) | ((addr & 0x1_FFFF) << 9) | 4;
    let mut word = [val];
    if let Err(phase) = h.cmd_data(CMD_IO_RW_EXTENDED_WRITE, arg, &mut word, false) {
        let resp = h.last_resp();
        ctx.log_fmt(format_args!(
            "wifi-driver: CMD53 write of {:#010x} to function {} address {:#07x} failed - {} \
             (STATUS={:#010x} INT={:#010x} R5 flags {:#04x})",
            val,
            func,
            addr,
            phase,
            h.status(),
            h.last_int(),
            (resp >> 8) & 0xFF
        ));
        // Same reason as the read path: a transfer the card ACCEPTED leaves it holding the
        // transaction open, and every later command then fails for that reason rather than its own.
        abort(h, func, ctx);
        return None;
    }
    Some(())
}

/// Tell the card to abandon a transfer on `func` - CCCR `IO_ABORT`, written to function 0.
///
/// **Without this, one failed data transfer poisons every command after it.** A CMD53 the card ACCEPTS
/// moves it into the transfer state, and `Host` resetting its own lines says nothing to the card - so the
/// card sits holding the transfer open and refuses what comes next. Measured, not supposed: the CMD52
/// after a failed CMD53 came back with R5 flags `0x28`, ERROR set and `IO_CURRENT_STATE` reading TRN. It
/// is also why that CMD52's refusal had nothing to do with the address it was reading.
///
/// That is a recovery that does not recover (§26.7) - the driver treated the failure as handled while
/// leaving the device in a state that broke everything downstream. `brcmf_sdiod_abort` makes the same
/// write for the same reason.
///
/// Its own failure is reported rather than propagated: the caller is already on a failure path with
/// something more useful to say, but an abort that is itself refused means the card has stopped listening
/// altogether, and that is worth seeing.
pub fn abort(h: &Host, func: u8, ctx: &ServiceContext) {
    /// CCCR `IO_ABORT`. Bits 2:0 name the function to abort; bit 3 would reset the card outright, which
    /// is a bigger hammer than a failed register read deserves.
    const CCCR_IO_ABORT: u32 = 0x06;
    if write_reg(h, 0, CCCR_IO_ABORT, func & 0x7).is_none() {
        ctx.log_fmt(format_args!(
            "wifi-driver:   and the IO_ABORT for function {} was itself refused - INT={:#010x}. The card \
             may be left mid-transfer, so later commands can fail for that reason rather than their own",
            func,
            h.last_int()
        ));
    }
}

/// Read one 32-bit register through CMD53 in byte mode.
///
/// **CMD52 cannot do this.** It carries a single byte in its RESPONSE, on the command line, with no
/// data phase at all - which is why the CCCR and CIS reads above use it and why a 32-bit backplane
/// register cannot. CMD53 moves bytes through the controller's FIFO, and four of them is one block of
/// four.
///
/// `addr` is a function-1 address, and for a backplane access the caller has already ORed in the
/// wide-access flag that tells the chip this is not a single-byte read (see `backplane`).
///
/// Byte mode rather than block mode (bit 27 clear) because the transfer is four bytes and block mode
/// would mean declaring a block size the card has not been given. Incrementing address (bit 26 set),
/// so the four bytes come from consecutive register bytes rather than four reads of the same one.
pub fn read32(h: &Host, func: u8, addr: u32, ctx: &ServiceContext) -> Option<u32> {
    // CMD53 argument: bit31 R/W (0 = read), bits30:28 function, bit27 block mode, bit26 OP code
    // (1 = incrementing), bits25:9 address, bits8:0 count (0 means 512 in byte mode, so 4 is 4).
    let arg = ((func as u32 & 0x7) << 28) | (1 << 26) | ((addr & 0x1_FFFF) << 9) | 4;
    let mut word = [0u32; 1];
    // THE PHASE IS THE DIAGNOSIS, and STATUS is printed beside INTERRUPT. `INT=0` alone narrowed
    // nothing: `CMD_DONE` is cleared once the command lands, so zero is what a healthy command looks
    // like while the FIFO is awaited. The sentence now says which of the four waits expired.
    if let Err(phase) = h.cmd_data(CMD_IO_RW_EXTENDED_READ, arg, &mut word, true) {
        let resp = h.last_resp();
        let (blk, cmdtm) = h.last_setup();
        ctx.log_fmt(format_args!(
            "wifi-driver: CMD53 read of function {} address {:#07x} failed - {} (STATUS={:#010x} \
             INT={:#010x} arg={:#010x})",
            func,
            addr,
            phase,
            h.status(),
            h.last_int(),
            arg
        ));
        // WHAT THE CONTROLLER IS ACTUALLY HOLDING, which is the question "no DAT line activity" raises:
        // SDHCI starts a transfer when a command with Data Present Select completes, so a controller
        // that started nothing either has a zero block size or does not have that bit set. Both are read
        // back, with the wanted values beside them so a reader need not know the encoding to see a
        // mismatch.
        ctx.log_fmt(format_args!(
            "wifi-driver:   the controller holds BLKSIZECNT={:#010x} (want {:#010x}) CMDTM={:#010x} \
             (want {:#010x}, data-present {})",
            blk,
            0x0001_7004u32,
            cmdtm,
            CMD_IO_RW_EXTENDED_READ,
            if cmdtm & (1 << 21) != 0 { "set" } else { "CLEAR - the controller has no data phase" }
        ));
        // AND WHAT CONTROL0 HELD GOING IN. Its DMA-select field must be zero for PIO - Linux clears it
        // before every transfer and says some controllers cannot do PIO at all while it names ADMA. On
        // this board nothing has configured this controller (the firmware boots from the other one), so
        // whether those bits were set is a real question rather than a formality.
        let c0 = h.last_ctrl0();
        ctx.log_fmt(format_args!(
            "wifi-driver:   CONTROL0 was {:#010x} going in, DMA select {:#x} ({})",
            c0,
            (c0 & 0x18) >> 3,
            if c0 & 0x18 != 0 {
                "NON-ZERO - this is why PIO did nothing, and it is now cleared per transfer"
            } else {
                "already zero, so the DMA selection was never the problem"
            }
        ));
        // DID THE CONTROLLER MOVE AT ALL? The accumulated OR of everything seen while waiting. Reporting
        // the registers after a timeout cannot tell "never moved" from "moved and settled back"; this
        // can, and it is the question the last four changes were each guessing at.
        let (si, ss) = h.seen();
        ctx.log_fmt(format_args!(
            "wifi-driver:   while waiting, INTERRUPT was ever {:#010x} and STATUS ever {:#010x} - \
             transfer-active {}, buffer-enable {}",
            si,
            ss,
            if ss & 0x0000_0300 != 0 { "SEEN" } else { "never" },
            if ss & 0x0000_0C00 != 0 { "SEEN" } else { "never" }
        ));
        let (first, last) = h.dat_window();
        ctx.log_fmt(format_args!(
            "wifi-driver:   the data phase was active from poll {} to poll {} of 2000000 ({})",
            first,
            last,
            if first == 0 {
                "never active at all"
            } else if last < 10_000 {
                "it gave up almost at once - a data timeout the controller declined to latch"
            } else {
                "it stayed active, so it was waiting on a card that never sent"
            }
        ));
        // THE R5 IS THE PART THAT CAN SAY WHY, and it was being discarded. Its flag byte is
        // `RESP0[15:8]`; a set bit there is the CARD refusing, which from the controller's side is
        // indistinguishable from the data phase never happening.
        let flags = (resp >> 8) & 0xFF;
        if flags & R5_ERRORS != 0 {
            ctx.log_fmt(format_args!(
                "wifi-driver:   the CARD REFUSED it - R5 flags {:#04x} (R5 {:#010x}): {}{}{}{}{}",
                flags,
                resp,
                if flags & 0x01 != 0 { "out-of-range " } else { "" },
                if flags & 0x02 != 0 { "bad-function " } else { "" },
                if flags & 0x08 != 0 { "error " } else { "" },
                if flags & 0x40 != 0 { "illegal-command " } else { "" },
                if flags & 0x80 != 0 { "crc " } else { "" },
            ));
        } else {
            ctx.log_fmt(format_args!(
                "wifi-driver:   the card ACCEPTED it - R5 flags {:#04x} are clean (R5 {:#010x}, IO \
                 state {}), so the card agreed to the transfer and the CONTROLLER did not run it",
                flags,
                resp,
                (flags >> 4) & 0x3
            ));
        }
        // THE CARD IS STILL HOLDING THE TRANSFER OPEN. Resetting the host's lines does not tell it to
        // stop, so without this every later command meets a card in the transfer state and is refused
        // for that reason rather than its own - which is how the CMD52 fallback came to report a refusal
        // that had nothing to do with the address it was reading.
        abort(h, func, ctx);
        return None;
    }
    // The FIFO delivers the four bytes in transfer order, which for a little-endian register is its
    // value as read. No byte swap: the controller hands over a 32-bit word already assembled that way,
    // which is the same assumption `block-driver` makes about its own 512-byte blocks.
    Some(word[0])
}

/// Set one function's block size, and read it back.
///
/// **A step this driver never performed, and the reference performs first.** `brcmf_sdiod_probe` sets
/// function 1's block size to 64 and function 2's to 512 **before** it enables function 1 - the very
/// stretch the fault has been narrowed to, since the window is verified and the card accepts the command
/// and then sends nothing.
///
/// Whether it is required for a BYTE-mode transfer is not obvious and is not claimed here: the block size
/// register configures block mode, and byte mode carries its length in the command. But `sdio_max_byte_size`
/// clamps a byte-mode transfer by the function's current block size for cards that need it, so the two are
/// less independent than the spec's wording suggests - and brcmfmac does this unconditionally on every
/// card it supports before it will touch the chip. Doing what the reference does, in the order it does it,
/// is the method (§26.14).
///
/// Read back because the last two boots taught it twice: a write the card ACCEPTED is not a register that
/// HOLDS a value, and the difference cost several boots when it went unasked about the backplane window.
pub fn set_block_size(h: &Host, func: u8, size: u16, ctx: &ServiceContext) -> bool {
    let addr = fbr::base(func) + fbr::BLKSIZE;
    let lo = (size & 0xFF) as u8;
    let hi = (size >> 8) as u8;
    // TO FUNCTION 0, not to `func`. The FBR block lives in function 0's address space; addressing it to
    // the function itself would write somewhere inside that function's own registers.
    if write_reg(h, 0, addr, lo).is_none() || write_reg(h, 0, addr + 1, hi).is_none() {
        ctx.log_fmt(format_args!(
            "wifi-driver: could not set function {}'s block size to {} (FBR {:#05x}) - INT={:#010x}",
            func, size, addr, h.last_int()
        ));
        return false;
    }
    match (read_reg(h, 0, addr), read_reg(h, 0, addr + 1)) {
        (Some(rlo), Some(rhi)) => {
            let got = u16::from(rlo) | (u16::from(rhi) << 8);
            ctx.log_fmt(format_args!(
                "wifi-driver: function {} block size set to {} (FBR {:#05x}), reads back {} - {}",
                func, size, addr, got,
                if got == size { "held" } else { "MISMATCH, the register did not take it" }
            ));
            got == size
        }
        _ => {
            ctx.log_fmt(format_args!(
                "wifi-driver: function {}'s block size was written but could not be READ BACK - \
                 INT={:#010x}",
                func, h.last_int()
            ));
            false
        }
    }
}

/// Enable one I/O function and wait for the card to say it is ready.
///
/// **A function that is not enabled does nothing at all**, which makes this the difference between a
/// card that is identified and a card that can be talked to. Function 1 on this part is the backplane -
/// the path a firmware image is later written through, and the first thing Linux's `brcmfmac` enables,
/// before any firmware exists inside the chip to answer.
///
/// It is also the first WRITE this driver performs. Everything before it is a read, so a bus that
/// answers reads and drops writes would look perfectly healthy up to this point; the readback of IORx
/// is what says otherwise.
///
/// Bounded, and the bound is REPORTED. A function that never reports ready is a real condition - the
/// card is entitled to take time - and silently giving up here is indistinguishable from success one
/// line later.
pub fn enable_function(h: &Host, func: u8, ctx: &ServiceContext) -> bool {
    if func == 0 || func > 7 {
        // Function 0 is the common area and is always present; it has no enable bit, and asking for one
        // would set a bit belonging to nothing.
        ctx.log_fmt(format_args!("wifi-driver: function {} cannot be enabled - not an I/O function", func));
        return false;
    }
    let bit = 1u8 << func;
    let current = match read_reg(h, 0, cccr::IO_ENABLE) {
        Some(v) => v,
        None => {
            ctx.log_fmt(format_args!(
                "wifi-driver: could not read IO_ENABLE before enabling function {} - INT={:#010x}",
                func,
                h.last_int()
            ));
            return false;
        }
    };
    // READ-MODIFY-WRITE, not a bare store: IO_ENABLE carries one bit per function, and writing `bit`
    // alone would DISABLE every other function on the card. Harmless today with one function in use and
    // exactly the kind of thing that becomes a mystery when a second one is added.
    if write_reg(h, 0, cccr::IO_ENABLE, current | bit).is_none() {
        ctx.log_fmt(format_args!(
            "wifi-driver: the WRITE to IO_ENABLE for function {} was refused - INT={:#010x}. Every \
             command before this one was a read, so a bus that answers reads and drops writes looks \
             healthy until exactly here",
            func,
            h.last_int()
        ));
        return false;
    }

    /// How many times to ask before giving up. Each attempt is one CMD52, which is microseconds, so
    /// this is a generous number of asks rather than a long wall-clock wait - and a count is not a
    /// duration, which is why the failure below reports the count rather than implying a time.
    const READY_TRIES: u32 = 500;
    for attempt in 0..READY_TRIES {
        match read_reg(h, 0, cccr::IO_READY) {
            Some(v) if v & bit != 0 => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: function {} enabled and READY (IOE {:#04x} -> {:#04x}, after {} \
                     read(s) of IOR)",
                    func,
                    current,
                    current | bit,
                    attempt + 1
                ));
                return true;
            }
            Some(_) => {}
            None => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: the IO_READY read failed while waiting on function {} - INT={:#010x}",
                    func,
                    h.last_int()
                ));
                return false;
            }
        }
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: function {} was enabled but never reported ready across {} reads of IOR. The \
         write was accepted, so the function exists and is not coming up",
        func, READY_TRIES
    ));
    false
}

/// Report the fixed CCCR facts, which are cheap and say a great deal about what we are talking to.
pub fn report_cccr(h: &Host, ctx: &ServiceContext) {
    let rev = read_reg(h, 0, cccr::REVISION);
    let caps = read_reg(h, 0, cccr::CARD_CAPS);
    let bus = read_reg(h, 0, cccr::BUS_IFACE);
    let en = read_reg(h, 0, cccr::IO_ENABLE);
    let rdy = read_reg(h, 0, cccr::IO_READY);
    match (rev, caps, bus, en, rdy) {
        (Some(rev), Some(caps), Some(bus), Some(en), Some(rdy)) => {
            // The CCCR revision's two nibbles are the CCCR format and the SDIO spec the card claims,
            // which is what decides later what registers are legal to read at all.
            ctx.log_fmt(format_args!(
                "wifi-driver: CCCR rev {:#04x} (CCCR fmt {}, SDIO spec {}), caps {:#04x}, bus iface \
                 {:#04x}, IOE {:#04x}, IOR {:#04x}",
                rev,
                rev & 0xF,
                (rev >> 4) & 0xF,
                caps,
                bus,
                en,
                rdy
            ));
        }
        _ => ctx.log_fmt(format_args!(
            "wifi-driver: a CCCR read failed - INT={:#010x}. The card was selected, so this is a \
             CMD52 problem rather than an identification one",
            h.last_int()
        )),
    }
}

/// Walk the CIS tuple chain and return the manufacturer identification if it is there.
///
/// Bounded twice over, because this walks a linked list whose links come from the DEVICE: a tuple
/// count, and a hard ceiling on how far from the start address it will read. A corrupt or hostile link
/// byte otherwise walks forever, and the thing being walked is a radio whose firmware is not loaded
/// yet - which is precisely when its registers are least trustworthy.
pub fn walk_cis(h: &Host, start: u32, ctx: &ServiceContext) -> Option<Manfid> {
    /// Tuples in a function-0 CIS.
    ///
    /// 64 was "generous and finite" and it was only the second of those: the Pi 4's radio ran the bound
    /// out without reaching an END tuple, because a Broadcom CIS carries a long run of `0x80`
    /// vendor-specific tuples after the three standard ones. That cost nothing - `CISTPL_MANFID` is the
    /// FIRST tuple, so identification was already done - and the bound firing and NAMING ITSELF is the
    /// bound working rather than a wasting asset. Raised so the chain can actually be seen to end.
    const MAX_TUPLES: u32 = 256;
    /// How far past the CIS pointer the walk will follow. The CIS lives in the card's common register
    /// space, so this is a bound on a 17-bit address rather than on memory.
    const MAX_SPAN: u32 = 0x800;

    let mut addr = start;
    let mut found: Option<Manfid> = None;
    let mut tuples = 0u32;
    let mut reported = 0u32;
    // WHY THE WALK STOPPED, tracked rather than inferred afterwards. There are four ways out of the
    // loop below and only one of them used to say so: the tuple count. The SPAN condition simply
    // dropped out of the `while` with nothing printed, which is the failure `arch/CLAUDE.md` rule 2
    // names outright - a bound that does not return a result the caller reads. It matters here for a
    // concrete reason: two boots of the same chip disagreed about where this chain ends, and a silent
    // exit is why that was hard to see.
    let mut reached_end = false;
    let mut read_failed = false;

    while tuples < MAX_TUPLES && addr < start + MAX_SPAN {
        tuples += 1;
        let code = match read_reg(h, 0, addr) {
            Some(c) => c,
            None => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: the CIS walk could not read the tuple code at {:#07x} - \
                     INT={:#010x}",
                    addr,
                    h.last_int()
                ));
                read_failed = true;
                break;
            }
        };
        if code == cistpl::END {
            reached_end = true;
            break;
        }
        if code == cistpl::NULL {
            // The one tuple with no length byte. Advancing by two here (as every other tuple does)
            // would step over whatever follows.
            addr += 1;
            continue;
        }
        let len = match read_reg(h, 0, addr + 1) {
            Some(l) => l as u32,
            None => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: the CIS walk could not read the length of tuple {:#04x} at {:#07x}",
                    code, addr
                ));
                read_failed = true;
                break;
            }
        };
        let body = addr + 2;

        // ANNOUNCED BEFORE IT IS DECODED. This line used to sit at the BOTTOM of the loop, below the
        // branches that decode a tuple's contents - so on hardware the decoded `CIS FUNCID` line
        // printed ABOVE its own tuple header and read as though it belonged to the tuple before it.
        // Cheap to misread, cheaper to reorder.
        if reported < 24 {
            reported += 1;
            ctx.log_fmt(format_args!(
                "wifi-driver: CIS tuple {:#04x} len {} at {:#07x}",
                code, len, addr
            ));
        }

        if code == cistpl::MANFID && len >= 4 {
            let b: [Option<u8>; 4] = [
                read_reg(h, 0, body),
                read_reg(h, 0, body + 1),
                read_reg(h, 0, body + 2),
                read_reg(h, 0, body + 3),
            ];
            if let [Some(m0), Some(m1), Some(d0), Some(d1)] = b {
                found = Some(Manfid {
                    manf: (m0 as u16) | ((m1 as u16) << 8),
                    device: (d0 as u16) | ((d1 as u16) << 8),
                });
            }
        } else if code == cistpl::FUNCID && len >= 1 {
            if let Some(f) = read_reg(h, 0, body) {
                // 0x0C is the SDIO function class for a "network adapter" in the CIS encoding. Logged
                // as a raw byte with the note rather than decoded into a table, because one value is
                // all this phase needs and a lookup table nobody reads is the kind of speculative
                // machinery §26.2 asks not to build.
                ctx.log_fmt(format_args!(
                    "wifi-driver: CIS FUNCID {:#04x} (0x0c = network adapter)",
                    f
                ));
            }
        }

        addr = body + len;
    }

    // ONE LINE PER WAY OUT, so the walk never ends without saying how.
    if reached_end {
        ctx.log_fmt(format_args!(
            "wifi-driver: the CIS ended properly at {:#07x} after {} tuple(s)", addr, tuples - 1
        ));
    } else if read_failed {
        // Already reported in detail at the point of failure; named here so the summary is complete.
        ctx.log_fmt(format_args!(
            "wifi-driver: the CIS walk stopped after {} tuple(s) because a read failed", tuples
        ));
    } else if tuples >= MAX_TUPLES {
        ctx.log_fmt(format_args!(
            "wifi-driver: the CIS walk stopped at its {}-tuple bound without reaching an END tuple. \
             The chain is longer than expected or a link byte is wrong",
            MAX_TUPLES
        ));
    } else {
        // THE BOUND THAT USED TO EXIT IN SILENCE.
        ctx.log_fmt(format_args!(
            "wifi-driver: the CIS walk stopped at its {:#x}-byte span bound ({:#07x}, from {:#07x}) \
             without reaching an END tuple - so it was reading past the real chain",
            MAX_SPAN, addr, start
        ));
    }
    found
}
