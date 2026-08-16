//! USB ethernet (SMSC LAN9514) - find its bulk endpoints and configure it.
//!
//! Slice 4, first part (`docs/arm32-usb-userspace.md`). Same shape as the keyboard and the disk: the
//! descriptor walk is control transfers, which are proven, so "did we find it" fails separately from
//! "can we talk to it".
//!
//! The NIC is `0424:ec00` on port 1 - HIGH speed and directly attached, so no splits. It reports
//! device class `0xff` (vendor-specific), which is why the endpoint walk does NOT filter by class:
//! there is no standard class to match, only bulk endpoints to find.

use godspeed_sdk::{Dma, Mmio, ServiceContext};

use crate::chan::{self, Target};

const DESC_CONFIG: u8 = 0x02;
const DESC_ENDPOINT: u8 = 0x05;
const EP_TYPE_BULK: u8 = 0x02;

/// A bound USB ethernet device.
/// What the frame path has actually moved, as opposed to what it was asked to move.
///
/// These separate the three failures that look identical from outside ("no network"): a TX the device
/// refuses, a bulk-IN that never returns bytes, and bytes that arrive and are then thrown away by the
/// status-word parse. Each needs a different fix and the log cannot tell them apart without counting.
#[derive(Default)]
pub struct Stats {
    pub tx_ok:      u32,
    pub tx_fail:    u32,
    pub rx_bursts:  u32,   // bulk-IN returned >0 bytes
    pub rx_bytes:   u32,   // total bytes those bursts carried
    pub rx_frames:  u32,   // complete frames the parse handed up
    pub rx_bad:     u32,   // bursts whose first status word did not parse
    /// HCINT from the LAST bulk-IN that did not complete, plus how many times the channel never
    /// halted at all. The controller writes down why every transfer ended; discarding that and
    /// guessing is what turns a five-minute diagnosis into an afternoon. 0 with a non-zero
    /// `rx_nohalt` means the channel never halted - a different fault from any error bit.
    pub rx_hcint:   u32,
    pub rx_nohalt:  u32,
    /// The PHY's BMSR as of the last link question, and whether it said UP. Reported to net-stack but
    /// never to the log until now - so "link UP" was inferred from net-stack's behaviour rather than
    /// ever being SEEN at the moment frames should have been arriving. A device that NAKs every IN
    /// while promiscuous and enabled has received nothing, and the link is the only thing left.
    pub bmsr: u32,
    pub rx_fifo: u32,
    pub int_sts: u32,
}

pub struct Nic {
    pub ep_in: u8,
    pub ep_out: u8,
    pub mps: u16,
    /// The station MAC, read from the device's serial-number STRING descriptor.
    ///
    /// Not a register read: the LAN9514 publishes its MAC as twelve ASCII hex characters in a UTF-16
    /// string descriptor, which is why the parse below steps FOUR bytes per output byte - two UTF-16
    /// code units, each two bytes, per pair of hex digits.
    pub mac: [u8; 6],
    /// Endpoint data toggles, per DIRECTION, for the device's lifetime - the level USB defines them
    /// at, learned three times over on the disk path.
    pub stats: Stats,
    /// Is a bulk-IN currently armed on CH_NET, waiting for the device to have a frame?
    pub in_armed: bool,
    pub pid_in: u32,
    pub pid_out: u32,
    /// Cached PHY link state, and when it was read.
    ///
    /// A transmit into a cable that is not plugged in cannot succeed, and it is not cheap to fail: it
    /// burns the full 2 s bulk budget before giving up, on the ONE thread that also polls the
    /// keyboard. Hardware showed exactly what that costs - `net-stack` queued roughly a hundred
    /// doomed frames a second during a DHCP attempt, and this service stopped answering anything else
    /// for a minute and a half. The keyboard went dead, `q` never reached the shell, and the driver
    /// printed no heartbeat because it never had an idle pass to print one from, so it read as
    /// crashed when it was merely drowning.
    ///
    /// Reading BMSR per transmit would just move the cost (it is two control transfers), so the
    /// answer is cached and refreshed at a bounded rate. Stale by at most `LINK_TTL_MS`, which is far
    /// shorter than any human notices a cable going in and far longer than a frame burst.
    pub link_up: bool,
    pub link_at: u64,
    /// Consecutive failed transmits, and when the backoff they triggered began.
    ///
    /// A device that refuses one frame refuses the next, and hardware showed how expensive believing
    /// otherwise is: 1481 failures against 637 successes, ~18 doomed transmits a second, each holding
    /// the thread for its full budget. That is ~90% of this service spent proving the same thing over
    /// and over, and it is felt as the shell going slow, because storage and the keyboard are served
    /// by the very same thread.
    ///
    /// This is a BACKOFF, not a recovery - it does not reset the device, it declines to ask. That
    /// distinction matters, because eager RECOVERY thresholds have twice been worse than the fault
    /// they chased (a re-enumeration resets the port and stops the device settling). Declining to
    /// attempt is free and reversible: one probe per window finds the moment the device is willing
    /// again, and a single success clears it.
    pub tx_fail_run: u32,
    pub tx_backoff_at: u64,
    /// HCINT from the last failed transmit, and whether the channel simply never halted.
    ///
    /// TX passed `None` for this while RX recorded it, so the log could say a transmit failed but
    /// never WHY - and "the device refuses frames" and "the transfer never completed" are different
    /// faults with different fixes. Recording it costs a field and removes a guess.
    pub tx_hcint: u32,
    pub tx_nohalt: u32,
    /// GNPTXSTS at the last failed transmit: bits [31:24] are request-queue entries FREE, [15:0] are
    /// FIFO words free.
    ///
    /// This is the register that separates the two remaining explanations for a transaction that
    /// never happens. FIFO space with NO queue space means the core has nowhere to put the request -
    /// the signature of aborted channels whose entries were never retired. Both non-zero means the
    /// core could have scheduled it and chose not to, which is a different bug entirely. Guessing
    /// between those cost a boot on the FIFO-flush theory; reading it costs a word.
    pub tx_nptxsts: u32,
    /// USB 2.0 PING state for the bulk OUT endpoint (§8.5.1, Linux `qh->ping_state`).
    ///
    /// Set when the endpoint answers NAK or NYET, cleared when it ACKs. While set, the next transfer
    /// carries HCTSIZ.DOPNG so the core pings until the device has room instead of re-sending data at
    /// a device that is refusing it. Per-ENDPOINT, like the data toggle, because that is the level
    /// USB defines it at.
    pub ping_out: bool,
}

/// Consecutive failures before this endpoint is presumed unwilling.
const TX_FAIL_RUN: u32 = 8;
/// How long to decline transmits before letting one probe through.
const TX_BACKOFF_MS: u64 = 500;

/// How long a cached link answer is trusted before it is read again.
const LINK_TTL_MS: u64 = 500;

/// How long one frame is given to leave the host.
///
/// This was 2000 ms, chosen by nobody for no recorded reason, and it is what takes this service off
/// the air. Do the arithmetic the number never had: a 1522-byte frame is three 512-byte packets on a
/// 480 Mbit bus, about 25 us of wire time. A high-speed microframe is 125 us. Fifty milliseconds is
/// four hundred microframes - if the device has not accepted the frame by then it is not going to,
/// and the previous budget spent a further 1.95 SECONDS establishing that, per frame, on the one
/// thread that also polls the keyboard.
///
/// The failure this produces is identical to the failure at 2 s; it simply arrives 40x sooner. That
/// distinction is the entire fix: nothing downstream learns less, and the driver stays answerable.
/// Guarding only the link-down case was too narrow - hardware then showed the same storm with the
/// link genuinely UP (BMSR 0x782d), because a transmit can fail for reasons that have nothing to do
/// with the cable. What must be bounded is the COST of failing, not one of its causes.
const TX_BUDGET_MS: u64 = 50;

fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

/// Read the station MAC from string descriptor `idx`, as twelve ASCII hex characters.
fn read_mac(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, idx: u8) -> [u8; 6] {
    let mut mac = [0u8; 6];
    if idx == 0 {
        return mac;
    }
    // Length first: a string descriptor's length is its first byte, and asking for a fixed guess
    // either truncates it or overruns the device's answer - the same rule the config descriptor needs.
    let mut sd = [0u8; 40];
    let head = [0x80, 0x06, idx, 0x03, 0x09, 0x04, 2, 0]; // wIndex = langid 0x0409 (en-US)
    if !chan::control(ctx, mmio, dma, t, &head, &mut sd, true, 2) {
        return mac;
    }
    let len = (sd[0] as usize).min(sd.len());
    if len < 26 {
        return mac; // too short to hold twelve hex characters as UTF-16
    }
    let full = [0x80, 0x06, idx, 0x03, 0x09, 0x04, len as u8, 0];
    if !chan::control(ctx, mmio, dma, t, &full, &mut sd, true, len) {
        return mac;
    }
    for b in 0..6 {
        mac[b] = (hex_val(sd[2 + b * 4]) << 4) | hex_val(sd[2 + b * 4 + 2]);
    }
    mac
}

/// Find the bulk IN/OUT pair. No class filter: this device is vendor-specific.
fn find_bulk(buf: &[u8], total: usize) -> Option<(u8, u8, u16)> {
    let mut i = 0usize;
    let (mut ep_in, mut ep_out, mut mps) = (0u8, 0u8, 64u16);
    while i + 2 <= total {
        let len = buf[i] as usize;
        if len < 2 || i + len > total {
            break; // malformed descriptor ends the walk rather than freezing the cursor
        }
        if buf[i + 1] == DESC_ENDPOINT && len >= 7 && buf[i + 3] & 0x03 == EP_TYPE_BULK {
            let raw = u16::from_le_bytes([buf[i + 4], buf[i + 5]]);
            mps = match raw & 0x07FF {
                0 => 64,
                v => v,
            };
            if buf[i + 2] & 0x80 != 0 {
                ep_in = buf[i + 2] & 0x0F;
            } else {
                ep_out = buf[i + 2] & 0x0F;
            }
        }
        i += len;
    }
    if ep_in != 0 && ep_out != 0 {
        Some((ep_in, ep_out, mps))
    } else {
        None
    }
}

/// Configure the NIC and return its bulk endpoints.
pub fn bind(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target) -> Option<Nic> {
    let mut head = [0u8; 9];
    let get9 = [0x80, 0x06, 0, DESC_CONFIG, 0, 0, 9, 0];
    if !chan::control(ctx, mmio, dma, t, &get9, &mut head, true, 9) {
        return None;
    }
    let total = u16::from_le_bytes([head[2], head[3]]) as usize;
    let cfg_val = head[5];

    let want = total.min(chan::DATA_LEN);
    let mut full = [0u8; chan::DATA_LEN];
    let getall = [
        0x80, 0x06, 0, DESC_CONFIG, 0, 0,
        (want & 0xFF) as u8, ((want >> 8) & 0xFF) as u8,
    ];
    if !chan::control(ctx, mmio, dma, t, &getall, &mut full, true, want) {
        return None;
    }

    let (ep_in, ep_out, mps) = find_bulk(&full, want)?;

    // RETRY SET_CONFIGURATION, with a settle between attempts.
    //
    // This is not defensive coding - it is a documented property of this device, and the kernel
    // driver's comment records it from hardware: the LAN9514 ACCEPTS the request (the SETUP is ACKed)
    // and then XactErrs the zero-length status stage for tens of milliseconds while it brings up its
    // internal ethernet state. SET_ADDRESS, the same shape of no-data control-OUT, succeeded moments
    // earlier - so the device CAN do the transfer, it just needs to settle after accepting the config.
    //
    // It also NAKs that status stage for longer than an ordinary control budget allows, which is the
    // other half of the same story. This driver's control path treats a NAK as a failed stage, so the
    // retry here is what covers both: eight attempts, each a fresh whole request, exactly as usbcore
    // retries control transfers. HW-blind and tuned by observation in the kernel driver; carried
    // across rather than re-derived, because it passes in QEMU either way.
    let setcfg = [0x00, 0x09, cfg_val, 0, 0, 0, 0, 0];
    let mut none: [u8; 0] = [];
    let mut set_ok = false;
    for attempt in 0..8u32 {
        if chan::control(ctx, mmio, dma, t, &setcfg, &mut none, false, 0) {
            set_ok = true;
            if attempt > 0 {
                ctx.log_fmt(format_args!(
                    "dwc2-svc: NIC SET_CONFIGURATION took {} attempts (the LAN9514 settles slowly)",
                    attempt + 1));
            }
            break;
        }
        ctx.sleep(ctx.duration_cycles(50));
    }
    if !set_ok {
        ctx.log("dwc2-svc: NIC SET_CONFIGURATION FAILED after 8 attempts");
        return None;
    }

    let mac = smsc_bring_up(ctx, mmio, dma, t)?;

    ctx.log_fmt(format_args!(
        "dwc2-svc: smsc95xx (LAN9514) UP - bulk IN {} OUT {} mps {} MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        ep_in, ep_out, mps, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]));
    // Link starts DOWN and unstamped: the first transmit reads the PHY instead of assuming a
    // cable is there. Assuming UP is how a driver spends a full budget per frame proving otherwise.
    Some(Nic { ep_in, ep_out, mps, mac, stats: Stats::default(), in_armed: false,
               pid_in: chan::PID_DATA0, pid_out: chan::PID_DATA0,
               link_up: false, link_at: 0,
               tx_fail_run: 0, tx_backoff_at: 0, tx_hcint: 0, tx_nohalt: 0, tx_nptxsts: 0,
               ping_out: false })
}

// --- smsc95xx (LAN9514) bring-up -------------------------------------------------------------------
//
// The Pi 2's onboard NIC is NOT CDC-ECM. It is a vendor-specific part (class 0xFF, VID 0x0424 SMSC)
// whose entire configuration happens through VENDOR control requests: bRequest 0xA0 writes a 4-byte
// register, 0xA1 reads one, with the register offset in wIndex. Until that configuration runs the chip
// has no MAC programmed, no receive filter, and TX/RX disabled - it will not move a single frame.
//
// Slice 4b ported this device's FRAMING (the 8-byte TX command, the 4-byte RX status word) without its
// bring-up, which is why the framing was correct and unreachable. QEMU hid it: QEMU's `usb-net` is
// CDC-ECM, a device that needs none of this.
//
// Reimplemented from the working kernel driver in `arch/arm/dwc2.rs`, which was itself written from the
// u-boot/Linux `smsc95xx` reference (driver doctrine: behaviour cited, code reimplemented).

const SMSC_TX_CFG: u16 = 0x10;
const SMSC_TX_CFG_ON: u32 = 0x0000_0004;
const SMSC_INT_STS:     u16 = 0x08;
/// RX FIFO information: bits [15:0] are the bytes the MAC has PUT IN the receive FIFO. This separates
/// the two halves of "the device NAKs": a MAC that never saw a frame (link/PHY/receive engine) from a
/// MAC holding frames it will not hand to the bulk endpoint (a USB-side problem). Nothing else can.
const SMSC_RX_FIFO_INF: u16 = 0x18;
const SMSC_HW_CFG: u16 = 0x14;
const SMSC_HW_CFG_LRST: u32 = 0x0000_0008;
const SMSC_HW_CFG_BCE:  u32 = 0x0000_0002;
const SMSC_HW_CFG_MEF:  u32 = 0x0000_0020;
/// HW_CFG.BIR = answer an empty bulk-IN with a NAK rather than a zero-length packet. Keep it set: a NAK
/// is retried by the DWC2 core in hardware, so an idle device stays quiet. Clear it and every idle poll
/// completes instantly with 0 bytes, which turns polling into a max-rate spin. Do not "fix" it away.
const SMSC_HW_CFG_BIR:  u32 = 0x0000_1000;
const SMSC_HW_CFG_RXDOFF: u32 = 0x0000_0600;
const SMSC_PM_CTRL: u16 = 0x20;
const SMSC_PM_CTRL_PHY_RST: u32 = 0x0000_0010;
const SMSC_AFC_CFG: u16 = 0x2C;
const SMSC_BURST_CAP: u16 = 0x38;
const SMSC_BULK_IN_DLY: u16 = 0x6C;
const SMSC_MAC_CR: u16 = 0x100;
const SMSC_MAC_CR_RXEN: u32 = 0x0000_0004;
const SMSC_MAC_CR_TXEN: u32 = 0x0000_0008;
const SMSC_MAC_CR_HPFILT: u32 = 0x0000_2000;
const SMSC_MAC_CR_PRMS:  u32 = 0x0004_0000;
const SMSC_MAC_CR_MCPAS: u32 = 0x0008_0000;
const SMSC_MAC_CR_FDPX:  u32 = 0x0010_0000;
const SMSC_ADDRH: u16 = 0x104;
const SMSC_ADDRL: u16 = 0x108;
const SMSC_MII_ADDR: u16 = 0x114;
const SMSC_MII_DATA: u16 = 0x118;
const SMSC_PHY_ID: u32 = 1;
const SMSC_MII_BMCR: u32 = 0;
const SMSC_MII_ADVERTISE: u32 = 4;
const SMSC_MII_BMSR: u32 = 1;   // basic mode STATUS; bit 2 = link up, bit 5 = auto-negotiation done

/// Is the ethernet link up, per the PHY itself?
///
/// BMSR's link bit is **latch-low**: once the link has been down, the bit reads 0 on the NEXT read even
/// if the link has since come back, and only the read AFTER that shows the truth. A single read
/// therefore reports a freshly-connected cable as down, forever, until something happens to read twice.
/// This is standard 802.3 clause-22 behaviour, not a quirk of this part, and it is why every driver
/// reads this register twice.
fn link_up(ctx: &ServiceContext, m: &Mmio, d: &Dma, t: &Target) -> bool {
    let _latched = mii_read(ctx, m, d, t, SMSC_MII_BMSR);
    matches!(mii_read(ctx, m, d, t, SMSC_MII_BMSR), Some(v) if v & 0x0004 != 0)
}

/// The link state, re-read at most every `LINK_TTL_MS`.
///
/// `link_up` costs two control transfers, so asking it per frame would replace one expensive answer
/// with another. Caching makes the common case - a burst of frames while the cable state has not
/// changed - free, and bounds how stale the answer can be to half a second.
///
/// The cache starts DOWN and un-stamped, so the very first transmit reads the PHY rather than
/// trusting a default. An unreadable PHY counts as down, for the same reason `OP_NET_INFO` says so:
/// an unanswerable question is not a yes.
fn link_fresh(ctx: &ServiceContext, m: &Mmio, d: &Dma, t: &Target, nic: &mut Nic) -> bool {
    let now = ctx.read_tsc();
    if nic.link_at == 0 || now.wrapping_sub(nic.link_at) >= ctx.duration_cycles(LINK_TTL_MS) {
        nic.link_up = link_up(ctx, m, d, t);
        nic.link_at = now;
    }
    nic.link_up
}
/// RX burst size in 512-byte high-speed packets, and the IN transfer length that must match it.
///
/// 8 packets / 4096 bytes, copied VERBATIM from the in-kernel driver that works on this exact board.
/// I had chosen 4 / 2048 - self-consistent, and wrong. With the chip configured to accumulate a burst
/// it evidently never considered complete, it NAKed every IN while its receive FIFO filled to 20,464
/// bytes (`RX_FIFO=0x4ff0`, near the LAN9514's entire FIFO). Data in, nothing out, forever.
///
/// This is the "read the working code" lesson again: the values were sitting in `arch/arm/dwc2.rs`,
/// proven on this hardware, and I picked my own instead.
const SMSC_BURST_PKTS: u32 = 8;

/// Every register poll below is bounded by this many attempts. A count is not a duration - but each
/// attempt here is one CONTROL TRANSFER, which carries its own hardware timeout, so the product is
/// bounded in time as well as in iterations. A healthy part clears in one or two.
const SMSC_POLLS: u32 = 64;

fn smsc_write(ctx: &ServiceContext, m: &Mmio, d: &Dma, t: &Target, index: u16, value: u32) -> bool {
    let setup = [0x40, 0xA0, 0x00, 0x00, index as u8, (index >> 8) as u8, 4, 0x00];
    let mut data = value.to_le_bytes();
    chan::control(ctx, m, d, t, &setup, &mut data, false, 4)
}

/// `None` if the control transfer itself failed - deliberately distinct from a register that genuinely
/// reads zero. Fabricating a 0 there is what lets a dead USB link masquerade as a real reading.
fn smsc_read(ctx: &ServiceContext, m: &Mmio, d: &Dma, t: &Target, index: u16) -> Option<u32> {
    let setup = [0xC0, 0xA1, 0x00, 0x00, index as u8, (index >> 8) as u8, 4, 0x00];
    let mut data = [0u8; 4];
    if !chan::control(ctx, m, d, t, &setup, &mut data, true, 4) { return None; }
    Some(u32::from_le_bytes(data))
}

fn smsc_read_or0(ctx: &ServiceContext, m: &Mmio, d: &Dma, t: &Target, index: u16) -> u32 {
    smsc_read(ctx, m, d, t, index).unwrap_or(0)
}

/// Wait for the MDIO engine to drop BUSY. Bounded: a stuck engine leaves the PHY unconfigured (net
/// degrades) rather than holding this service forever.
fn mii_idle(ctx: &ServiceContext, m: &Mmio, d: &Dma, t: &Target) -> bool {
    for _ in 0..SMSC_POLLS {
        match smsc_read(ctx, m, d, t, SMSC_MII_ADDR) {
            Some(v) if v & 1 == 0 => return true,
            Some(_) => {}
            None => return false,
        }
    }
    false
}

fn mii_write(ctx: &ServiceContext, m: &Mmio, d: &Dma, t: &Target, reg: u32, val: u16) -> bool {
    if !mii_idle(ctx, m, d, t) { return false; }
    if !smsc_write(ctx, m, d, t, SMSC_MII_DATA, val as u32) { return false; }
    smsc_write(ctx, m, d, t, SMSC_MII_ADDR, (SMSC_PHY_ID << 11) | (reg << 6) | (1 << 1) | 1)
}

fn mii_read(ctx: &ServiceContext, m: &Mmio, d: &Dma, t: &Target, reg: u32) -> Option<u16> {
    if !mii_idle(ctx, m, d, t) { return None; }
    if !smsc_write(ctx, m, d, t, SMSC_MII_ADDR, (SMSC_PHY_ID << 11) | (reg << 6) | 1) { return None; }
    if !mii_idle(ctx, m, d, t) { return None; }
    smsc_read(ctx, m, d, t, SMSC_MII_DATA).map(|v| (v & 0xFFFF) as u16)
}

/// Reset the chip and its PHY, program the station MAC, enable turbo RX, start auto-negotiation, and
/// turn TX + RX on. Returns the MAC the chip is now filtering on.
///
/// The MAC is the one place this service is WEAKER than the in-kernel driver, and the log says so
/// rather than hiding it. The kernel reads the real board MAC (b8:27:eb:..) from the VideoCore mailbox,
/// which lives outside the DWC2 register window this service was granted. Reaching it would mean
/// granting a second, much wider MMIO window for one identity fact - authority out of proportion to the
/// need. So this asks the CHIP (which the Pi's firmware may have programmed) and, failing that, uses a
/// locally-administered address. Networking works either way: an LAA is a valid unicast MAC and DHCP
/// serves it normally. What is lost is the address matching the sticker, and the log names which case
/// happened so nobody has to guess later.
fn smsc_bring_up(ctx: &ServiceContext, m: &Mmio, d: &Dma, t: &Target) -> Option<[u8; 6]> {
    // Lite reset the chip.
    let hw = smsc_read(ctx, m, d, t, SMSC_HW_CFG)?;
    if !smsc_write(ctx, m, d, t, SMSC_HW_CFG, hw | SMSC_HW_CFG_LRST) {
        ctx.log("dwc2-svc: smsc lite-reset write FAILED - NIC stays down");
        return None;
    }
    let mut cleared = false;
    for _ in 0..SMSC_POLLS {
        if smsc_read_or0(ctx, m, d, t, SMSC_HW_CFG) & SMSC_HW_CFG_LRST == 0 { cleared = true; break; }
    }
    if !cleared {
        ctx.log("dwc2-svc: smsc lite-reset never cleared - NIC stays down (loud, not silently degraded)");
        return None;
    }

    // Reset the PHY.
    let pm = smsc_read_or0(ctx, m, d, t, SMSC_PM_CTRL);
    smsc_write(ctx, m, d, t, SMSC_PM_CTRL, pm | SMSC_PM_CTRL_PHY_RST);
    for _ in 0..SMSC_POLLS {
        if smsc_read_or0(ctx, m, d, t, SMSC_PM_CTRL) & SMSC_PM_CTRL_PHY_RST == 0 { break; }
    }

    // MAC: ask the chip, else a locally-administered address (bit 1 of byte 0 set).
    let lo = smsc_read_or0(ctx, m, d, t, SMSC_ADDRL);
    let hi = smsc_read_or0(ctx, m, d, t, SMSC_ADDRH);
    let from_chip = [lo as u8, (lo >> 8) as u8, (lo >> 16) as u8, (lo >> 24) as u8, hi as u8, (hi >> 8) as u8];
    let mac = if from_chip == [0u8; 6] || from_chip == [0xFFu8; 6] {
        ctx.log("dwc2-svc: NIC has no MAC programmed - using a locally-administered address (the board MAC lives in the VideoCore mailbox, outside this service's window)");
        [0x02, 0x00, 0x00, 0x12, 0x34, 0x56]
    } else {
        from_chip
    };
    smsc_write(ctx, m, d, t, SMSC_ADDRL,
               (mac[0] as u32) | ((mac[1] as u32) << 8) | ((mac[2] as u32) << 16) | ((mac[3] as u32) << 24));
    smsc_write(ctx, m, d, t, SMSC_ADDRH, (mac[4] as u32) | ((mac[5] as u32) << 8));

    // Turbo RX: the chip aggregates MANY frames into one bulk-IN burst, each with its 4-byte status word
    // and DWORD-aligned - which is exactly what `rx` already parses. Read-modify-write, because a bare
    // write clears the power-on defaults; clear RXDOFF so the frame sits immediately after its status.
    let hw = (smsc_read_or0(ctx, m, d, t, SMSC_HW_CFG) | SMSC_HW_CFG_BIR | SMSC_HW_CFG_MEF | SMSC_HW_CFG_BCE)
             & !SMSC_HW_CFG_RXDOFF;
    smsc_write(ctx, m, d, t, SMSC_HW_CFG, hw);
    smsc_write(ctx, m, d, t, SMSC_BURST_CAP, SMSC_BURST_PKTS);
    smsc_write(ctx, m, d, t, SMSC_BULK_IN_DLY, 0x2000);
    smsc_write(ctx, m, d, t, SMSC_AFC_CFG, 0x00F8_30A1);

    // PHY: reset, advertise 10/100, restart auto-negotiation. Do NOT block on link - net-stack retries
    // and self-configures when the link comes up, so waiting here would only delay the boot.
    mii_write(ctx, m, d, t, SMSC_MII_BMCR, 0x8000);
    for _ in 0..SMSC_POLLS {
        match mii_read(ctx, m, d, t, SMSC_MII_BMCR) {
            Some(v) if v & 0x8000 == 0 => break,
            Some(_) => {}
            None => break,
        }
    }
    mii_write(ctx, m, d, t, SMSC_MII_ADVERTISE, 0x01E1);
    mii_write(ctx, m, d, t, SMSC_MII_BMCR, 0x1200);

    // Enable TX + RX. FDPX because the internal PHY negotiates full duplex and a half-duplex MAC on a
    // full-duplex link drops frames to late collisions. The receive filter is our-unicast + broadcast
    // ONLY: promiscuous, all-multicast and the hash filter are CLEARED so the mDNS/SSDP/IPv6-ND flood is
    // dropped at the chip instead of drowning our replies. Some of those bits come out of reset SET.
    // The promiscuous DIAGNOSTIC is gone (it did its job: bursts stayed at zero with the filter open,
    // which is what proved the receive path was broken rather than the network quiet). Back to
    // our-unicast + broadcast, which is why the filter was narrowed in the first place: the
    // mDNS/SSDP/IPv6-ND flood gets dropped at the CHIP instead of filling a 20 KB FIFO we then have to
    // drain. Some of these bits come out of reset SET, so they are cleared explicitly.
    let cr = (smsc_read_or0(ctx, m, d, t, SMSC_MAC_CR)
              & !(SMSC_MAC_CR_PRMS | SMSC_MAC_CR_MCPAS | SMSC_MAC_CR_HPFILT))
             | SMSC_MAC_CR_TXEN | SMSC_MAC_CR_RXEN | SMSC_MAC_CR_FDPX;
    if !smsc_write(ctx, m, d, t, SMSC_MAC_CR, cr) {
        ctx.log("dwc2-svc: smsc MAC_CR enable FAILED - NIC stays down");
        return None;
    }
    smsc_write(ctx, m, d, t, SMSC_TX_CFG, SMSC_TX_CFG_ON);

    // Read the state back OFF THE CHIP rather than assuming the writes took. Every register here was
    // just written by us, so a value that disagrees says the vendor control path is not landing - and
    // that is a completely different bug from "frames do not flow". BMSR bit 2 is the PHY's own link
    // bit: the only honest answer to "is the cable up", as against the UP this driver currently
    // reports to net-stack unconditionally.
    let cr  = smsc_read_or0(ctx, m, d, t, SMSC_MAC_CR);
    let txc = smsc_read_or0(ctx, m, d, t, SMSC_TX_CFG);
    let hwc = smsc_read_or0(ctx, m, d, t, SMSC_HW_CFG);
    let bmsr = mii_read(ctx, m, d, t, SMSC_MII_BMSR);
    ctx.log_fmt(format_args!(
        "dwc2-svc: smsc readback MAC_CR=0x{:08x} (TXEN {} RXEN {}) TX_CFG=0x{:08x} HW_CFG=0x{:08x} BMSR={} link {}",
        cr, cr & SMSC_MAC_CR_TXEN != 0, cr & SMSC_MAC_CR_RXEN != 0, txc, hwc,
        match bmsr { Some(v) => v, None => 0xFFFF },
        match bmsr {
            Some(v) if v & 0x0020 == 0 => "NEGOTIATING (this instant, not a verdict - autonegotiation                                            was restarted microseconds ago and takes seconds; ask again                                            with `net`)",
            Some(v) if v & 0x0004 != 0 => "UP",
            Some(_) => "DOWN",
            None => "UNREADABLE",
        }));
    Some(mac)
}

/// Where frames live in the DMA arena, clear of everything else.
///
/// DERIVED, not asserted in prose. The first values here were `0x1000`/`0x2000` and the const
/// assertion below rejected them at BUILD time: `msc`'s data buffer spans `0x400..0x1400`, so the TX
/// frame would have been laid inside it. That is the identical overlap that cost a boot in slice 2 -
/// where the comment claimed the buffer was clear of the scratch and nobody checked the arithmetic.
/// The assertion is why this one cost a compile instead.
pub const TX_OFF: usize = 0x2000;
pub const RX_OFF: usize = 0x3000;
// C3-1. This said 1514 while the kernel, nic-driver and genet all said 1600 - six definitions of one
// fact, and mine was the one that disagreed. 1514 is a correct number (an ethernet frame without its
// FCS) which is exactly why it looked right when written; but nic-driver ACCEPTS up to 1600, so a frame
// between 1515 and 1600 bytes was accepted upstream and silently TRUNCATED here. Nothing reports a
// truncated frame - it just becomes a corrupt packet somebody else has to explain.
//
// Raised to 1600 to agree with every other definition. That removes the truncation window; it does NOT
// remove the duplication, which is the actual Commandment III violation and needs one shared source
// these crates can both name. Recorded rather than pretended away.
pub const FRAME_MAX: usize = 1600;
/// One IN transfer can carry SEVERAL frames, so the receive burst is larger than one frame.
pub const RX_BURST: usize = 4096;   // 8 x 512, matching SMSC_BURST_PKTS and the kernel driver
const _: () = assert!(TX_OFF >= crate::msc::DATA_OFF + crate::msc::DATA_MAX);
const _: () = assert!(RX_OFF >= TX_OFF + FRAME_MAX + 8);
/// The RX burst must FIT the granted arena (64 KiB). Added when the burst doubled to 4096 to match the
/// kernel driver: a buffer that runs off the end of the arena is a DMA write into whatever follows, and
/// the device would do it silently. The sibling assertions above already caught one real overlap at
/// build time, which is the argument for spending three lines on the next one.
const _: () = assert!(RX_OFF + RX_BURST <= 64 * 1024);

/// Send one ethernet frame.
///
/// **The LAN9514 is NOT plain CDC-ECM.** It wants an 8-byte TX command in front of the frame, and a
/// driver that sends the raw frame instead is not rejected - the device silently drops it, which on a
/// NIC is the worst failure shape there is.
pub fn tx(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, nic: &mut Nic, frame: &[u8],
) -> bool {
    // REFUSE, CHEAPLY, WITH NO LINK.
    //
    // The frame cannot go anywhere, and the expensive part is not the failure - it is HOW LONG the
    // failure takes. `bulk` below is given 2 s, so a client retrying a doomed transmit does not merely
    // waste its own time, it takes this service off the air: the keyboard poll shares this thread, and
    // a pass that spends seconds in a transmit polls the keyboard seconds apart. That is what a user
    // experiences as "the keyboard stopped working", and the driver looked crashed rather than busy
    // because a heartbeat is only printed from an idle pass.
    //
    // The answer returned is the same `false` the timeout would have produced after 2 s. Nothing
    // downstream learns anything new; it just learns it immediately (§26.7 - fail loudly and fast,
    // rather than slowly and identically).
    if !link_fresh(ctx, mmio, dma, t, nic) {
        nic.stats.tx_fail += 1;
        return false;
    }
    // A DEVICE THAT HAS REFUSED EIGHT FRAMES IN A ROW IS NOT ASKED AGAIN FOR HALF A SECOND.
    //
    // The link guard above covers a missing cable; this covers everything else, and hardware showed
    // that everything else is the common case: 1481 failures to 637 successes with the link UP, at
    // ~18 attempts a second. Even at the reduced 50 ms budget that is ~90% of this service's thread
    // spent re-proving one fact, and the thread is shared with storage and the keyboard, so the whole
    // machine feels slow.
    //
    // A probe is still let through every window, so nothing is latched: the moment the device accepts
    // a frame the run clears and full rate resumes. Costs at most one budget per window rather than
    // one per request.
    let now = ctx.read_tsc();
    if nic.tx_fail_run >= TX_FAIL_RUN {
        if now.wrapping_sub(nic.tx_backoff_at) < ctx.duration_cycles(TX_BACKOFF_MS) {
            nic.stats.tx_fail += 1;
            return false;
        }
        // Window elapsed: let ONE frame through to ask whether the device is willing yet.
        nic.tx_backoff_at = now;
        // FLUSH THE FIFO BEFORE THE PROBE, when the signature says the core stalled rather than the
        // device refusing.
        //
        // HCINT reading literally zero with the channel never halting is not a device declining a
        // frame - a refusal is a NAK, and a NAK sets a bit. Zero means no transaction happened at
        // all, which is the fault this driver's own bring-up comment describes: a stale non-periodic
        // TX FIFO pointer, where "the channel arms but never transacts (ChEna set, HCINT=0, zero
        // bytes moved)". That was diagnosed here during bring-up and the remedy was only ever applied
        // AT bring-up, so the same wedge hours later had no path back - transmit froze at 590 frames
        // and stayed frozen for the life of the service.
        //
        // Retrying the probe without this just re-asks a stalled FIFO. Gated on the signature so a
        // genuinely refusing device (which needs patience, not a flush) is left alone, and it runs at
        // most once per backoff window.
        if nic.tx_hcint == 0 && nic.tx_nohalt != 0 {
            let flushed = crate::core::flush_np_tx_fifo(ctx, mmio);
            ctx.log_fmt(format_args!(
                "dwc2-svc: TX stalled with no transaction (HCINT=0) - non-periodic FIFO flush {}",
                if flushed { "done, retrying" } else { "FAILED - transmit stays wedged" }));
        }
    }
    // RELEASE THE ARMED RECEIVE BEFORE TRANSMITTING.
    //
    // This is what actually starves transmit, and it took a corrected register read to see. The
    // receive path arms a bulk-IN and deliberately LEAVES it armed, because with BIR set the core
    // NAK-retries in hardware and an idle device is silent by design. What that comment does not say
    // is where those retries go: every one takes an entry in the core's NON-PERIODIC REQUEST QUEUE,
    // the same queue a transmit needs. A quiet network therefore fills it with retried INs, and the
    // OUT is never scheduled at all - which is exactly the observed fault, a channel that arms and
    // transacts nothing with HCINT reading 0x00000000.
    //
    // GNPTXSTS said so plainly once read correctly: 0x18000100 is 256 FIFO words free and ZERO queue
    // entries free. Space for the data, nowhere to put the request.
    //
    // So the IN stands aside for the duration of one frame. `rx` re-arms on its next pass because
    // `in_armed` is what it tests, so this costs one round of receive latency; frames are not lost,
    // the device holds them in its own FIFO, which is the whole reason RX_FIFO_INF is non-zero while
    // this happens. Receive latency is a fair price for transmit existing at all.
    if nic.in_armed {
        chan::halt(mmio, CH_NET_RX);
        nic.in_armed = false;
    }
    let n = frame.len().min(FRAME_MAX);
    // TX_CMD_A = len | FIRST_SEG | LAST_SEG, TX_CMD_B = len. Both little-endian.
    let a = (n as u32) | 0x0000_2000 | 0x0000_1000;
    for (i, b) in a.to_le_bytes().iter().enumerate() {
        dma.write8(TX_OFF + i, *b);
    }
    for (i, b) in (n as u32).to_le_bytes().iter().enumerate() {
        dma.write8(TX_OFF + 4 + i, *b);
    }
    for i in 0..n {
        dma.write8(TX_OFF + 8 + i, frame[i]);
    }
    // A ZLP IS NEEDED. The comment here used to say it was not - "the smsc95xx carries an explicit
    // length in its TX command, so it does not rely on a short packet to find the frame boundary the
    // way CDC-ECM does" - and Linux says otherwise in one line:
    //
    //     .flags = FLAG_ETHER | FLAG_SEND_ZLP | FLAG_LINK_INTR,     (smsc95xx.c)
    //
    // which usbnet acts on:
    //
    //     if (length % dev->maxpacket == 0) {
    //         if (!(info->flags & FLAG_SEND_ZLP)) { length++; }     // pad instead
    //         else urb->transfer_flags |= URB_ZERO_PACKET;          // <- this device
    //     }
    //
    // A bulk transfer whose length is an exact multiple of the endpoint's max packet size ends on a
    // FULL packet, and a full packet means "more to come". Without a terminating zero-length packet
    // the device keeps its receive buffer open for the rest of a frame that never arrives, and every
    // subsequent OUT is NAKed because that buffer is full. Permanently.
    //
    // That is the whole fault: transmit worked for hundreds of frames and then died forever, at a
    // different count each boot, because it takes one frame whose total length happens to land on a
    // 512-byte boundary. With 8 bytes of TX command that is a payload of 504, 1016, ... - ordinary
    // sizes that ordinary traffic reaches sooner or later. It also explains why every host-side
    // remedy failed: the full buffer is the DEVICE's, so flushing our FIFO, resetting our channel and
    // pinging all correctly reported a device with no room.
    // ASK WHY. This was `None`, so a failing transmit was counted and never explained - and "the
    // device NAKs every frame" and "the transfer never halted" are different faults needing different
    // fixes. The receive path has recorded this from the start; the transmit path guessed.
    let mut why = (0u32, 0u32);
    let mut ping = nic.ping_out;
    let total = (n + 8) as u32;
    let ok = bulk(ctx, mmio, t, nic.mps, false, nic.ep_out, dma.phys_at(TX_OFF) as u32,
                  total, TX_BUDGET_MS, &mut nic.pid_out, Some(&mut why),
                  Some(&mut ping)).is_some();
    // Terminate an exact-multiple transfer with a zero-length packet, per FLAG_SEND_ZLP above. It is
    // a normal OUT of length 0 on the same endpoint, so it advances the data toggle like any other
    // packet - which the toggle readback in `bulk` already handles.
    let ok = if ok && nic.mps != 0 && total % (nic.mps as u32) == 0 {
        let zlp = bulk(ctx, mmio, t, nic.mps, false, nic.ep_out, dma.phys_at(TX_OFF) as u32,
                       0, TX_BUDGET_MS, &mut nic.pid_out, Some(&mut why), Some(&mut ping));
        if zlp.is_none() {
            ctx.log_fmt(format_args!(
                "dwc2-svc: ZLP after a {}-byte transfer FAILED - the device may hold its buffer open",
                total));
        }
        zlp.is_some()
    } else {
        ok
    };
    nic.ping_out = ping;
    if ok {
        nic.stats.tx_ok += 1;
        // One success is proof the device is willing: drop the backoff entirely rather than decaying
        // it, so a transient refusal costs one window and not a slow climb back to full rate.
        if nic.tx_fail_run >= TX_FAIL_RUN {
            ctx.log_fmt(format_args!(
                "dwc2-svc: NIC accepting frames again after {} refusals", nic.tx_fail_run));
        }
        nic.tx_fail_run = 0;
    } else {
        nic.stats.tx_fail += 1;
        nic.tx_hcint = why.0;
        nic.tx_nohalt = why.1;
        nic.tx_nptxsts = mmio.read32(crate::regs::GNPTXSTS);
        nic.tx_fail_run = nic.tx_fail_run.saturating_add(1);
        if nic.tx_fail_run == TX_FAIL_RUN {
            nic.tx_backoff_at = now;
            ctx.log_fmt(format_args!(
                "dwc2-svc: NIC refused {} frames in a row (HCINT={:#010x}{}) - backing off to one probe                  every {} ms. Frames are dropped meanwhile; input and storage stay responsive.",
                TX_FAIL_RUN, why.0, if why.1 != 0 { ", channel never halted" } else { "" },
                TX_BACKOFF_MS));
        }
    }
    ok
}

/// Receive one burst and hand each complete frame to `deliver`.
///
/// Returns the number of frames delivered. Zero is the ordinary answer on a quiet network and is not
/// a failure.
pub fn rx(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, nic: &mut Nic,
    mut deliver: impl FnMut(&[u8]),
) -> u32 {
    // ARM THE IN AND LEAVE IT ARMED.
    //
    // This is the whole bug, and the comment on SMSC_HW_CFG_BIR describes it exactly: with BIR set an
    // empty bulk-IN is NAKed, the DWC2 core retries the NAK IN HARDWARE, and the channel does not halt.
    // An idle device is therefore SILENT by design - not broken, not erroring, just waiting.
    //
    // The previous shape programmed the channel, waited 50 ms for a halt that only comes when a frame
    // actually arrives, gave up, and reprogrammed from scratch on the next call. It restarted the wait
    // 650 times and never once left an IN armed across the moment a frame turned up. The counters said
    // it plainly: HCINT 0x00000000, nohalt 650, rx 0.
    //
    // So arm once and CHECK, never re-arm a transfer that is still legitimately pending. This is the
    // kernel driver's background-armed IN, in the shape this service can use: a non-blocking poll per
    // pass instead of an interrupt, because the frame path here is driven by the serve loop.
    if !nic.in_armed {
        // Clear HCINT before arming: it is write-1-to-clear and holds whatever the LAST transfer on
        // this channel left behind. Arming without clearing means the first check reads a stale
        // completion and harvests a buffer the device has not written yet.
        mmio.write32(chan::hcint_at(CH_NET_RX), 0xFFFF_FFFF);
        chan::program(mmio, &Target { addr: t.addr, mps: nic.mps, low_speed: false }, CH_NET_RX,
                      true, nic.pid_in, RX_BURST as u32, dma.phys_at(RX_OFF) as u32,
                      nic.ep_in as u32, 2, 0);
        // Unmask this channel's terminal halt, exactly as the working kernel driver does before it arms
        // its background IN. `chan::program` zeroes HCINTMSK for every channel, which is invisible for
        // a channel programmed and polled in the same breath - and this is the only channel left
        // RUNNING UNATTENDED, so it is the one place the assumption carries weight. Diffing against the
        // working driver is what surfaced it; it should have been the first thing I did.
        mmio.write32(chan::hcintmsk_at(CH_NET_RX),
                     crate::regs::HCINT_CHHLTD | crate::regs::HCINT_XFERCOMPL);
        mmio.write32(crate::regs::HAINTMSK,
                     mmio.read32(crate::regs::HAINTMSK) | (1 << CH_NET_RX));
        nic.in_armed = true;
        return 0;   // nothing yet - the device answers when it has something
    }
    // COMPLETION IS ChEna GOING CLEAR, not an HCINT bit.
    //
    // This is the kernel driver's own tick watchdog, ported as-is: it does not consult HCINT to decide
    // whether the background IN finished - it tests HCCHAR.ChEna, and harvests when the channel is no
    // longer enabled ("ChEna clear - the IN is not outstanding"). Its ISR path reaches the same place
    // via CHHLTD, but the ISR is a route this polled service does not have.
    //
    // Waiting on HCINT.XFERCOMPL instead is what produced nohalt climbing past 21000 with HCINT stuck
    // at 0x00000000 across three boots. Reading the working code for its DESIGN rather than porting its
    // comments would have got here first; that is the whole lesson of this slice.
    let hcchar = mmio.read32(chan::hcchar_at(CH_NET_RX));
    let hcint  = mmio.read32(chan::hcint_at(CH_NET_RX));
    if hcchar & (1 << 31) != 0 {
        // Still enabled: the IN is outstanding and the device simply has nothing yet. With BIR set the
        // core NAK-retries in hardware without halting, so this is the ordinary quiet case.
        nic.stats.rx_nohalt = nic.stats.rx_nohalt.wrapping_add(1);
        nic.stats.rx_hcint = hcint;
        return 0;
    }
    if hcint & crate::regs::HCINT_STALL != 0 {
        // A halted endpoint is a hard failure, never retried by re-arming into the same condition.
        nic.stats.rx_hcint = hcint;
        nic.in_armed = false;
        return 0;
    }
    // W1C the channel's interrupts so the next armed transfer starts from a clean slate.
    mmio.write32(chan::hcint_at(CH_NET_RX), hcint);
    let left = mmio.read32(chan::hctsiz_at(CH_NET_RX)) & 0x7_FFFF;
    nic.pid_in = chan::pid_from_hctsiz(mmio, CH_NET_RX);
    nic.stats.rx_hcint = hcint;
    nic.in_armed = false;                 // consumed - the next pass arms a fresh one
    let got = (RX_BURST as u32).saturating_sub(left) as usize;
    if got == 0 { return 0; }
    nic.stats.rx_bursts += 1;
    nic.stats.rx_bytes = nic.stats.rx_bytes.wrapping_add(got as u32);

    // A burst is [4-byte RX status][frame incl FCS][DWORD pad], REPEATED - one transfer can carry
    // several frames, so this is a parse rather than a copy.
    let mut pos = 0usize;
    let mut frames = 0u32;
    let mut buf = [0u8; FRAME_MAX];
    while pos + 4 <= got {
        let status = u32::from_le_bytes([
            dma.read8(RX_OFF + pos), dma.read8(RX_OFF + pos + 1),
            dma.read8(RX_OFF + pos + 2), dma.read8(RX_OFF + pos + 3),
        ]);
        pos += 4;
        let flen = ((status >> 16) & 0x3FFF) as usize; // length INCLUDING the 4-byte FCS

        // The floor is a full ethernet header PLUS the FCS, not merely 4. A device-supplied `flen == 4`
        // would deliver a ZERO-length frame, and zero is the "nothing received" sentinel everywhere
        // above - so one malformed length would look like an empty network rather than a bad frame.
        if flen < 4 + 14 || flen > FRAME_MAX + 4 || pos + flen > got {
            // Count ONLY a burst whose FIRST status word is unparseable. A break after some frames
            // were delivered is the ordinary end of a burst, not a fault, and counting it would bury
            // the signal this is here to find.
            if frames == 0 {
                nic.stats.rx_bad += 1;
                // Show the bytes for the FIRST few rejects only. A burst arrived and completed cleanly,
                // so the device and the transfer are fine and the disagreement is about LAYOUT - which
                // is settled by looking at what actually landed, not by re-reading the parse. Bounded to
                // three so a persistent mismatch cannot flood the console.
                if nic.stats.rx_bad <= 3 {
                    ctx.log_fmt(format_args!(
                        "dwc2-svc: RX burst {} bytes, unparsed at {}: status=0x{:08x} flen={} |                          first 12 bytes {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}",
                        got, pos - 4, status, flen,
                        dma.read8(RX_OFF), dma.read8(RX_OFF + 1), dma.read8(RX_OFF + 2),
                        dma.read8(RX_OFF + 3), dma.read8(RX_OFF + 4), dma.read8(RX_OFF + 5),
                        dma.read8(RX_OFF + 6), dma.read8(RX_OFF + 7), dma.read8(RX_OFF + 8),
                        dma.read8(RX_OFF + 9), dma.read8(RX_OFF + 10), dma.read8(RX_OFF + 11)));
                }
            }
            break; // invalid, or a frame split across the burst boundary - give up on this burst
        }
        let n = flen - 4; // strip the FCS; the stack does not want it
        for i in 0..n.min(FRAME_MAX) {
            buf[i] = dma.read8(RX_OFF + pos + i);
        }
        deliver(&buf[..n.min(FRAME_MAX)]);
        frames += 1;
        nic.stats.rx_frames += 1;
        pos += flen;
        pos += (4 - (flen % 4)) % 4; // each frame is followed by DWORD padding
    }
    frames
}

/// One bulk transfer for the NIC. Shares the disk's shape but its OWN channel, so a frame in flight
/// cannot leave state on the channel a block transfer inherits - the rule the kernel driver learned
/// by corrupting block transfers with an abandoned interrupt split.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn bulk(
    ctx: &ServiceContext, mmio: &Mmio, t: &Target, mps: u16,
    dir_in: bool, ep: u8, buf_phys: u32, len: u32, budget_ms: u64, pid: &mut u32,
    why: Option<&mut (u32, u32)>, ping: Option<&mut bool>,
) -> Option<u32> {
    let bt = Target { addr: t.addr, mps, low_speed: false };
    let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(budget_ms));
    let mut last = 0u32;
    let mut halted = 0u32;
    // PING applies to an OUT endpoint only; an IN never pings (Linux: `ep_is_in` => do_ping = 0).
    let mut do_ping = !dir_in && ping.as_ref().map_or(false, |p| **p);
    loop {
        chan::program_ping(mmio, &bt, CH_NET, dir_in, *pid, len, buf_phys, ep as u32, 2, 0, do_ping);
        match chan::wait_halt(ctx, mmio, CH_NET, 50) {
            Some(hcint) if hcint & crate::regs::HCINT_XFERCOMPL != 0 => {
                let left = mmio.read32(chan::hctsiz_at(CH_NET)) & 0x7_FFFF;
                *pid = chan::pid_from_hctsiz(mmio, CH_NET);
                // ACK clears the ping state: the device has taken data, so the next transfer may go
                // straight out (Linux clears `ping_state` on ACK, hcd_intr.c).
                if let Some(p) = ping { *p = false; }
                if let Some(w) = why { *w = (hcint, 0); }
                return Some(len.saturating_sub(left));
            }
            Some(hcint) if hcint & crate::regs::HCINT_STALL != 0 => { last = hcint; halted += 1; break; }
            Some(hcint) => {
                last = hcint;
                halted += 1;
                // NAK or NYET on a HIGH-SPEED OUT means "ping me before sending data again" - USB 2.0
                // §8.5.1, and the exact transition Linux makes in `dwc2_hc_nak_intr` /
                // `dwc2_hc_nyet_intr`. Re-sending the data instead is what left transmit dead: the
                // endpoint was asking for a PING and got another data packet, every time, forever.
                if !dir_in
                    && hcint & (crate::regs::HCINT_NAK | crate::regs::HCINT_NYET) != 0
                {
                    do_ping = true;
                }
            }
            None => {}
        }
        if ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
            break;
        }
    }
    // READ HCINT ON THE WAY OUT, do not report the initial value as a measurement.
    //
    // `last` starts at 0 and was only ever assigned when `wait_halt` returned - so a transfer that
    // TIMED OUT reported "HCINT=0x00000000", which is a default masquerading as a reading. I drew
    // conclusions from that zero for two boots. If nothing was captured, capture it now.
    if last == 0 {
        last = mmio.read32(chan::hcint_at(CH_NET));
    }
    // Carry the ping requirement out to the caller so the NEXT attempt starts with it set.
    if let Some(p) = ping { *p = do_ping; }
    // READ THE TOGGLE BACK ON THE FAILURE PATH TOO.
    //
    // The success path above already does this, and the reason is written up on the keyboard poll:
    // the DWC2 core advances HCTSIZ.PID itself, and flipping or freezing it in software makes the two
    // disagree the moment they ever differ. A toggle mismatch does not raise an error - the transfer
    // is simply ignored or retransmitted forever - which is precisely the shape of "tx_ok froze at
    // 637 while failures climbed to 1481 and never recovered".
    //
    // A transmit that timed out is exactly the ambiguous case: the device may have taken the data and
    // advanced its toggle while the host never saw the ACK. Reading back what the CORE believes keeps
    // software in step with the only party that watched the wire, instead of preserving a stale guess
    // for the rest of the device's life.
    *pid = chan::pid_from_hctsiz(mmio, CH_NET);
    if let Some(w) = why { *w = (last, if halted == 0 { 1 } else { 0 }); }
    None
}

/// The NIC's own channel. Separate from CH_BULK (disk/control) and CH_KBD for the same reason those
/// are separate from each other: an abandoned transfer must not leave its state on a channel another
/// stream inherits.
pub const CH_NET: u32 = 2;

/// The armed bulk-IN's OWN channel, separate from CH_NET which carries TX.
///
/// Sharing one channel is what made the receive path harvest fill bytes with a confident length. An
/// armed IN waits across many serve passes by design, and every `tx()` in that window reprogrammed the
/// same channel - destroying the armed transfer and leaving its own XFERCOMPL|ACK in HCINT, which the
/// next receive check then read as ITS completion and harvested from a buffer nothing had written. The
/// giveaway was in the counters all along: bursts tracked TX almost one for one (12/13, 25/26, 38/39).
///
/// A channel can hold exactly one transfer. A transfer that outlives a single call therefore needs a
/// channel nothing else touches - the same rule that already keeps the disk, the keyboard and the NIC
/// apart, applied one level down to the two directions of the NIC itself.
pub const CH_NET_RX: u32 = 3;

// --- The frame IPC protocol -------------------------------------------------------------------
//
// **These opcodes must not collide with the BLOCK ones.** `dwc2` serves `block-driver` and
// `nic-driver` on the SAME endpoint, and a shared opcode space with two independent protocols in it
// is a bug waiting for the first person to add an op to either. Block uses 1..5; net starts at 0x10.
pub const OP_NET_INFO: u8 = 0x10;
pub const OP_NET_TX: u8 = 0x11;
pub const OP_NET_RX: u8 = 0x12;

/// Serve one frame request. Returns false if the message was not one.
pub fn serve(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, nic: &mut Nic,
    msg: &godspeed_sdk::Message, reply: godspeed_sdk::CapHandle,
) -> bool {
    let p = msg.payload_bytes();
    if p.is_empty() {
        return false;
    }
    let mut out = [0u8; FRAME_MAX + 4];
    let n = match p[0] {
        OP_NET_INFO => {
            // [ok, mac(6), link]. The link bit is reported as UP: this driver does not yet read the
            // PHY, and saying so here rather than inventing a "down" keeps net-stack from concluding
            // the cable is out. Reading it properly is the remaining piece of this slice.
            out[0] = 1;
            out[1..7].copy_from_slice(&nic.mac);
            // The PHY's own link bit, not an assumption. Reporting a hardcoded UP made net-stack
            // spend its DHCP budget against a cable that may not be there, and made "no link" and
            // "link up but silent" indistinguishable from the outside - the two things a diagnosis
            // most needs to tell apart. Unreadable counts as DOWN: an unanswerable question is not
            // a yes.
            // This is a fresh read of the PHY, so let the transmit cache learn from it too - the
            // link question and the transmit guard must never disagree about the same cable.
            let up = link_up(ctx, mmio, dma, t);
            nic.link_up = up;
            nic.link_at = ctx.read_tsc();
            nic.stats.bmsr = mii_read(ctx, mmio, dma, t, SMSC_MII_BMSR).map_or(0xFFFF, u32::from);
            nic.stats.rx_fifo = smsc_read_or0(ctx, mmio, dma, t, SMSC_RX_FIFO_INF);
            nic.stats.int_sts = smsc_read_or0(ctx, mmio, dma, t, SMSC_INT_STS);
            out[7] = u8::from(up);
            8
        }
        OP_NET_TX if p.len() > 1 => {
            out[0] = u8::from(tx(ctx, mmio, dma, t, nic, &p[1..]));
            1
        }
        OP_NET_RX => {
            // [n_lo, n_hi, frame...]. Zero length is "nothing received", which on a quiet network is
            // the ordinary answer and not a failure.
            let mut got = 0usize;
            rx(ctx, mmio, dma, t, nic, |f| {
                // ONE frame per reply: the protocol has no framing of its own, so a second frame in
                // the same message would be indistinguishable from the first one's payload. The rest
                // of the burst is dropped rather than mis-delivered, and the client polls again.
                if got == 0 {
                    let take = f.len().min(FRAME_MAX);
                    out[2..2 + take].copy_from_slice(&f[..take]);
                    got = take;
                }
            });
            out[0] = (got & 0xFF) as u8;
            out[1] = ((got >> 8) & 0xFF) as u8;
            got + 2
        }
        // ANSWER, do not return. The comment here used to read "not a net op - the block server gets a
        // look at it", and that was false: `dispatch` takes the reply cap BEFORE calling this, so
        // returning false meant the message was consumed, no reply was sent, and the cap was leaked -
        // one table entry per hit. The caller (`nic-driver`, in an undeadlined request_with_reply)
        // then waited forever, `net-stack` behind it, and the shell behind that.
        //
        // Reachable two ways: any first byte >= OP_NET_INFO that is not a known op, and OP_NET_TX
        // with a one-byte payload - which is exactly what `nic-driver` sends for a zero-length frame,
        // because the `p.len() > 1` guard above falls through to here.
        //
        // A one-byte error reply is the same answer the no-NIC path gives fifteen lines away in
        // `dispatch`, and for the same stated reason: silence would hang the client.
        _ => {
            out[0] = 0;
            1
        }
    };
    let _ = ctx.try_send_by_handle(reply, &godspeed_sdk::Message::from_bytes(&out[..n]));
    ctx.remove_cap(reply);
    true
}
