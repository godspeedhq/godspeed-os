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
    /// Frames parsed out of a burst but never delivered because the receive queue was full. The honest
    /// name for packet loss inside this driver, and the reason the queue below is not a silent buffer.
    pub rx_dropped: u32,
    /// HOW EACH FRAME WAS ADDRESSED: to this station, to broadcast, or to neither (multicast).
    ///
    /// These exist because "no network" hid a question nobody could answer: has this port EVER received
    /// a frame addressed to it? Everything that worked on this board - DHCP, and other hosts' ARP
    /// requests - is broadcast, and everything that did not - ARP replies, ICMP echo replies - is
    /// unicast. Those are two different capabilities and the logs could not tell them apart, so a
    /// receive path that accepts only broadcast looks exactly like a network where nothing answers.
    ///
    /// Counted HERE, in the driver, and not in the stack above it: this is where the frame comes off
    /// the wire, before anything interprets it, and `mac` below is the same value written into the
    /// device's unicast filter - so the classification is made against the address the hardware is
    /// actually matching on, not against a copy of it learned over IPC.
    pub rx_unicast: u32,
    pub rx_bcast:   u32,
    pub rx_other:   u32,
    /// Of the frames addressed to this station, how many were ARP and how many IPv4.
    ///
    /// This settles a question two runs have turned on and neither answered: frames addressed to us DO
    /// arrive - the count climbs during an ARP dance - while net-stack, scanning the same stream, reports
    /// zero ARP replies. Either those frames are ARP replies and something between here and there is
    /// losing them, or they are not ARP at all and the gateway is genuinely not answering. Those need
    /// opposite fixes and the addressing counters alone cannot tell them apart, so this splits them.
    ///
    /// Counted at the same point and for the same reason as the addressing counts: this is where the
    /// frame exists before anything else has had a chance to drop it.
    pub rx_uni_arp:  u32,
    pub rx_uni_ipv4: u32,
    /// Frames actually HANDED OUT in an OP_NET_RX reply.
    ///
    /// The gap this exists to locate: the parse counters say 26 frames arrived; net-stack says it
    /// scanned 10. Somewhere between the two, two thirds of the traffic disappears, and every stage in
    /// between reports success - `rx_dropped` is 0, the queue is empty, no send fails. Counting parses
    /// and counting deliveries at the same boundary turns "somewhere" into "this side or that side":
    /// popped ~= parsed means this driver did its job and the loss is above it; popped << parsed means
    /// the loss is here, in the one-frame-per-request drain.
    pub rx_popped: u32,
    /// Register reads that FAILED and were reported as zero.
    ///
    /// Without this a dead control path and a genuinely idle chip print the same line. Non-zero means
    /// the register values beside it are partly fiction, which is a different problem from whatever
    /// they appear to say.
    pub reg_read_fails: u32,
    /// Transfers that ended with a DATA TOGGLE ERROR: the device's packet was rejected by the core and
    /// the frame destroyed. Counted so the fix for it is checkable rather than believed - this should
    /// read 0, and any other number is receive loss with a name on it.
    pub rx_tglerr: u32,
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
    /// Frames parsed from a burst but not yet collected by the client, oldest first.
    ///
    /// A bulk-IN burst from this device can carry SEVERAL ethernet frames, and the serve protocol hands
    /// back one frame per reply - it has no framing of its own, so two frames in one message would be
    /// indistinguishable from one frame's payload. That is a wire-format constraint, and the old code
    /// treated it as licence to DROP the rest of the burst, reasoning that "the client polls again". A
    /// client polling again cannot recover frames already consumed from the burst and thrown away; on
    /// hardware this lost a steady few percent of everything received, and it looked like network loss.
    ///
    /// Bounded and stack-resident (no heap, 26.6.1); a full queue counts `stats.rx_dropped` rather than
    /// losing frames quietly.
    pub rxbuf: [u8; RXQ_BYTES],
    pub rxbuf_fill: usize,   // bytes written by the current burst
    pub rxbuf_pos: usize,    // read cursor into those bytes
    pub rxq_count: usize,    // frames still waiting to be collected
    /// Is a bulk-IN currently armed on CH_NET, waiting for the device to have a frame?
    pub in_armed: bool,
    /// Consecutive polls that found the armed IN still enabled and not complete, and how many times we
    /// have reported a long run of them. Diagnostic only - nothing acts on these.
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
    /// The device's TX FIFO free space as of the last heartbeat.
    ///
    /// Sampled periodically rather than only at a refusal, because the question that is left is
    /// whether the FIFO DRAINS: a figure that falls steadily and never recovers says frames are not
    /// reaching the wire, while one that moves up and down says they are.
    pub tx_fifo_free: u32,
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
    Some(Nic { ep_in, ep_out, mps, mac, stats: Stats::default(),
               rxbuf: [0u8; RXQ_BYTES], rxbuf_fill: 0, rxbuf_pos: 0, rxq_count: 0,
               in_armed: false,
               pid_in: chan::PID_DATA0, pid_out: chan::PID_DATA0,
               link_up: false, link_at: 0,
               tx_fail_run: 0, tx_backoff_at: 0, tx_hcint: 0, tx_nohalt: 0, tx_nptxsts: 0,
               ping_out: false, tx_fifo_free: 0 })
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
// Reimplemented from the working kernel driver in `arch/arm/dwc2.rs` (since DELETED - this service
// replaced it; `git show 8c6a42ab~1:kernel/src/arch/arm/dwc2.rs`), which was itself written from the
// u-boot/Linux `smsc95xx` reference (driver doctrine: behaviour cited, code reimplemented).

const SMSC_TX_CFG: u16 = 0x10;
const SMSC_TX_CFG_ON: u32 = 0x0000_0004;
/// TX_CFG bit 0: flush the DEVICE's transmit FIFO. Self-clearing.
///
/// The way out of a partial frame stuck at the head of that FIFO. Note WHICH FIFO: an earlier fix
/// flushed the HOST controller's non-periodic FIFO for this same symptom, which is the wrong box - the
/// same error as re-enumerating a device to repair a hub's transaction translator. Linux never needs
/// this because usbnet never leaves a short transfer behind; we did, so we need the way back.
const SMSC_TX_CFG_FIFO_FLUSH: u32 = 0x0000_0001;
const SMSC_INT_STS:     u16 = 0x08;
/// RX FIFO information: bits [15:0] are the bytes the MAC has PUT IN the receive FIFO. This separates
/// the two halves of "the device NAKs": a MAC that never saw a frame (link/PHY/receive engine) from a
/// MAC holding frames it will not hand to the bulk endpoint (a USB-side problem). Nothing else can.
const SMSC_RX_FIFO_INF: u16 = 0x18;
/// TX Data FIFO FREE space (Linux `TX_FIFO_INF`, 0x1C, low 16 bits).
///
/// The register that answers the only question that ever mattered here: does the device have room for
/// this frame? A NAK on a bulk OUT means "no buffer space" and nothing else, so this separates the two
/// remaining explanations outright - zero free means frames really are not draining to the wire, while
/// plenty free means the chip has room and the refusal is not about space at all.
///
/// We have been reading its RECEIVE twin since bring-up and never this one.
const SMSC_TX_FIFO_INF: u16 = 0x1C;
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
/// Flow control (Linux `FLOW`, 0x11C). Bit 1 FCEN enables it, bit 0 FCBSY says it is busy, the top
/// half is the pause time.
///
/// This driver never wrote it, and Linux writes it TWICE: once at reset under its own `/* Init Tx */`
/// heading, and again on EVERY link change from `smsc95xx_phy_update_flowcontrol`. A register the
/// vendor driver initialises as part of starting the transmitter, left at whatever reset leaves it,
/// on a port that was otherwise followed closely.
const SMSC_FLOW: u16 = 0x11C;
/// MAC_CR bit 23. Linux sets it in HALF duplex and clears it in full (`smsc95xx_mac_update_fullduplex`).
const SMSC_MAC_CR_RCVOWN: u32 = 0x0080_0000;
/// PHY auto-negotiation link-partner ability (MII register 5), and our advertisement (register 4).
/// Their intersection is the negotiated mode - the same thing Linux gets from `phydev->duplex`.
const SMSC_MII_LPA: u32 = 5;
/// 100BASE-TX full duplex, 10BASE-T full duplex, in both ADVERTISE and LPA.
const MII_FULL_DUPLEX: u16 = 0x0100 | 0x0040;
const SMSC_BURST_CAP: u16 = 0x38;
const SMSC_BULK_IN_DLY: u16 = 0x6C;
const SMSC_MAC_CR: u16 = 0x100;
/// Registers `smsc95xx_reset` writes and this driver did not, found by listing the reference driver's
/// write sequence against ours rather than by reasoning about which ones "should" matter.
///
/// `VLAN1` is the one with teeth. It holds the ethertype the MAC treats as a **VLAN tag**, and Linux
/// sets it to `ETH_P_8021Q` (0x8100) explicitly. Left unwritten it keeps whatever the reset left, and a
/// MAC that thinks some other ethertype is a VLAN tag will parse those frames as tagged - reading the
/// two bytes after it as tag control information and the two after that as the real ethertype. That is
/// silent misparsing of one protocol while every other protocol on the wire is unaffected, which is the
/// shape of the fault here: DHCP (0x0800) is answered and ARP (0x0806) never is, both unicast to the
/// same address, on a receive path that reports losing nothing.
///
/// `INT_STS` clears whatever interrupt status survived the reset, and `HASHH`/`HASHL` are the multicast
/// hash table, which `smsc95xx_set_multicast` zeroes for an interface with no multicast groups. Ours has
/// none, and leaving the table holding reset garbage is a filter configured by accident.
///
/// Deliberately NOT ported: `LED_GPIO_CFG` (LED pin assignment - cosmetic, and writing a value guessed
/// from memory into a pin-mux register is worse than leaving it) and `INT_EP_CTL`'s PHY-interrupt enable
/// (that arms link-change notification on the USB interrupt endpoint, which this driver does not read -
/// it polls BMSR instead, see `link_update` - so enabling it would queue notifications nothing collects).
const SMSC_HASHH: u16 = 0x10C;
const SMSC_HASHL: u16 = 0x110;
const SMSC_VLAN1: u16 = 0x120;
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

/// Reconfigure the MAC for the duplex the PHY actually negotiated, and set flow control to match.
///
/// Ported from `smsc95xx_mac_update_fullduplex` + `smsc95xx_phy_update_flowcontrol`, which Linux runs
/// on EVERY link change. This driver ran neither: MAC_CR was written once at bring-up, while the PHY
/// was still auto-negotiating, and then never revisited. So the MAC kept whatever duplex we guessed
/// before the link existed, and FLOW kept whatever reset left.
///
/// Linux's rules, exactly:
///   full duplex -> MAC_CR |= FDPX, MAC_CR &= ~RCVOWN, AFC_CFG &= ~0xF   (no pause negotiated)
///   half duplex -> MAC_CR &= ~FDPX, MAC_CR |= RCVOWN, AFC_CFG |= 0xF
///   FLOW = 0 unless pause was negotiated, which we do not advertise.
///
/// Whether this is the transmit fault is NOT established - it is a missing step in the port, found by
/// diffing against the vendor driver, and it happens at exactly the moment transmit dies. The device
/// registers logged beside it are what will settle that, rather than another argument.
fn link_reconfigure(ctx: &ServiceContext, m: &Mmio, d: &Dma, t: &Target) {
    // Negotiated duplex = what both ends advertised. Linux reads it from the PHY layer; the raw form
    // is the intersection of our ADVERTISE (4) and the partner's LPA (5).
    let adv = mii_read(ctx, m, d, t, SMSC_MII_ADVERTISE).unwrap_or(0);
    let lpa = mii_read(ctx, m, d, t, SMSC_MII_LPA).unwrap_or(0);
    let full = adv & lpa & MII_FULL_DUPLEX != 0;

    // A READ-MODIFY-WRITE MUST NOT PROCEED ON A READ THAT DID NOT HAPPEN.
    //
    // These were `smsc_read_or0`, which fabricates 0 when the control transfer fails. The next lines
    // OR bits into that value and write it back, so a failed read did not merely mislead - it wrote
    // ZERO over every other bit in MAC_CR, silently turning off receive, transmit and duplex while
    // reporting that it had configured the link. A fabricated value is bad in a log and destructive in
    // a register.
    //
    // If either read fails there is nothing safe to write, so write nothing and say so. The link stays
    // as it was, which is a state the rest of the driver already copes with.
    let (Some(mut mac_cr), Some(mut afc)) =
        (smsc_read(ctx, m, d, t, SMSC_MAC_CR), smsc_read(ctx, m, d, t, SMSC_AFC_CFG)) else {
        ctx.log("dwc2-svc: link update SKIPPED - could not read MAC_CR/AFC_CFG, refusing to write a config built on a fabricated value (link left as it was)");
        return;
    };
    if full {
        mac_cr |= SMSC_MAC_CR_FDPX;
        mac_cr &= !SMSC_MAC_CR_RCVOWN;
        afc &= !0xF;
    } else {
        mac_cr &= !SMSC_MAC_CR_FDPX;
        mac_cr |= SMSC_MAC_CR_RCVOWN;
        afc |= 0xF;
    }
    smsc_write(ctx, m, d, t, SMSC_MAC_CR, mac_cr);
    // No pause is advertised, so FLOW stays 0 - the branch Linux takes when neither side asked for it.
    smsc_write(ctx, m, d, t, SMSC_FLOW, 0);
    smsc_write(ctx, m, d, t, SMSC_AFC_CFG, afc);
    ctx.log_fmt(format_args!(
        "dwc2-svc: link up - {} duplex (adv {:#06x} lpa {:#06x}); MAC_CR={:#010x} AFC_CFG={:#010x} FLOW=0",
        if full { "FULL" } else { "HALF" }, adv, lpa, mac_cr, afc));
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
        let up = link_up(ctx, m, d, t);
        link_observed(ctx, m, d, t, nic, up, now);
    }
    nic.link_up
}

/// Record an observation of the link, from WHEREVER it was made, and act on a down->up edge.
///
/// Every path that learns the link state must come through here. The first version let `OP_NET_INFO`
/// write `link_up`/`link_at` directly - to share its fresh read with the cache, which was the right
/// instinct - and that quietly consumed the edge: by the time `link_fresh` looked, the state was
/// already UP and there was no transition left to see. `link_reconfigure` then ran exactly zero times
/// on hardware while appearing, in the source, to be wired up.
///
/// One writer, one place the edge is decided. Two callers updating the same cached state is how a
/// transition goes missing.
fn link_observed(
    ctx: &ServiceContext, m: &Mmio, d: &Dma, t: &Target, nic: &mut Nic, up: bool, now: u64,
) {
    let was = nic.link_up;
    let first = nic.link_at == 0;
    nic.link_up = up;
    nic.link_at = now;
    if up && (!was || first) {
        link_reconfigure(ctx, m, d, t);
    }
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

/// Read a register FOR DISPLAY, fabricating 0 if the transfer fails.
///
/// **Zero from this function may mean "the register is zero" OR "the read did not happen", and nothing
/// distinguishes them.** That is why the name says what it is for. Use it only where the value is
/// printed and a human is reading it. NEVER use it for:
///
///   - a **decision** - a failed read then looks like whichever answer zero happens to mean. A poll
///     here read `HW_CFG & LRST == 0` as "the reset completed", the one conclusion it must not reach by
///     accident, and everything after it assumed a reset chip.
///   - a **read-modify-write** - the fabricated zero is written BACK. A failed MAC_CR read wrote zero
///     over receive, transmit and duplex while reporting that it had configured the link. A fabricated
///     value is misleading in a log and destructive in a register.
///
/// Both of those call sites are gone; they use `smsc_read` and handle `None` (§26.7 - a failure is
/// reported, never swallowed). What remains is the honest residual: a register dump prints 0 for a
/// register it could not read, and `reg_read_fails` in the periodic report is how you know.
fn smsc_read_for_log(ctx: &ServiceContext, m: &Mmio, d: &Dma, t: &Target, index: u16) -> u32 {
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
    // A READ THAT FAILED IS NOT A BIT THAT CLEARED. This polled `smsc_read_or0`, so a failed control
    // transfer returned 0, the LRST bit read as clear, and the driver concluded the reset had completed
    // - the one conclusion it must not reach by accident, since everything after it assumes a reset
    // chip. `None` now means "no answer yet", which is what it is, and the loop keeps waiting until the
    // bound decides.
    let mut cleared = false;
    for _ in 0..SMSC_POLLS {
        if let Some(v) = smsc_read(ctx, m, d, t, SMSC_HW_CFG) {
            if v & SMSC_HW_CFG_LRST == 0 { cleared = true; break; }
        }
    }
    if !cleared {
        ctx.log("dwc2-svc: smsc lite-reset never cleared - NIC stays down (loud, not silently degraded)");
        return None;
    }

    // Reset the PHY.
    // Same read-modify-write hazard as MAC_CR above, and the same rule: no read, no write.
    let Some(pm) = smsc_read(ctx, m, d, t, SMSC_PM_CTRL) else {
        ctx.log("dwc2-svc: smsc PM_CTRL unreadable - cannot reset the PHY without overwriting it blind");
        return None;
    };
    smsc_write(ctx, m, d, t, SMSC_PM_CTRL, pm | SMSC_PM_CTRL_PHY_RST);
    for _ in 0..SMSC_POLLS {
        // A failed read is "not yet", never "done" - see the lite-reset loop above.
        if matches!(smsc_read(ctx, m, d, t, SMSC_PM_CTRL), Some(v) if v & SMSC_PM_CTRL_PHY_RST == 0) {
            break;
        }
    }

    // MAC: ask the chip, else a locally-administered address (bit 1 of byte 0 set).
    let lo = smsc_read_for_log(ctx, m, d, t, SMSC_ADDRL);
    let hi = smsc_read_for_log(ctx, m, d, t, SMSC_ADDRH);
    let from_chip = [lo as u8, (lo >> 8) as u8, (lo >> 16) as u8, (lo >> 24) as u8, hi as u8, (hi >> 8) as u8];
    // THREE SOURCES, BEST FIRST - and say which one answered.
    //
    // 1. The BOARD's own MAC, via kernel query 23. This is the address on the sticker and the one the
    //    network already knows. The in-kernel driver this service replaced used it (it could read the
    //    VideoCore mailbox directly) and networking worked; the port lost it, because a userspace driver
    //    is granted the controller's register window and nothing else. The query hands over the fact
    //    without the window.
    // 2. Whatever the firmware left in the chip's own filter registers.
    // 3. A locally-administered address, invented here.
    //
    // Case 3 is a genuine last resort and was until now the case this board always took. The address is
    // HARDCODED, so every machine running this system claims the same one: fine on a bench with a single
    // board, wrong the moment there are two on a network, and impossible to reconcile with a router that
    // has been told about the real one. It stays as the fallback because a NIC with no address at all is
    // worse, and it is logged as the compromise it is rather than passed off as normal.
    let mac = if let Some(board) = ctx.board_mac() {
        ctx.log_fmt(format_args!(
            "dwc2-svc: NIC using the board MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            board[0], board[1], board[2], board[3], board[4], board[5]));
        board
    } else if from_chip == [0u8; 6] || from_chip == [0xFFu8; 6] {
        ctx.log("dwc2-svc: NIC has no MAC - not from the board, not from the chip - so it is using a hardcoded locally-administered address, which every board running this system shares");
        [0x02, 0x00, 0x00, 0x12, 0x34, 0x56]
    } else {
        ctx.log_fmt(format_args!(
            "dwc2-svc: NIC using the MAC the firmware left in the chip {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            from_chip[0], from_chip[1], from_chip[2], from_chip[3], from_chip[4], from_chip[5]));
        from_chip
    };
    smsc_write(ctx, m, d, t, SMSC_ADDRL,
               (mac[0] as u32) | ((mac[1] as u32) << 8) | ((mac[2] as u32) << 16) | ((mac[3] as u32) << 24));
    smsc_write(ctx, m, d, t, SMSC_ADDRH, (mac[4] as u32) | ((mac[5] as u32) << 8));
    // READ THE FILTER BACK. This is the address the chip matches UNICAST frames against, and nothing
    // has ever confirmed it took.
    //
    // It matters because of an asymmetry visible on hardware: DHCP works and ARP does not. A DHCP
    // offer reaches a client with no IP as a BROADCAST, which this chip accepts regardless of its
    // address filter, while an ARP reply is UNICAST to our MAC and only arrives if the filter holds
    // the right one. So "DHCP round-trips but ARP never answers" is exactly what a wrong or unwritten
    // unicast filter looks like, and exactly what these two registers settle.
    let rl = smsc_read_for_log(ctx, m, d, t, SMSC_ADDRL);
    let rh = smsc_read_for_log(ctx, m, d, t, SMSC_ADDRH);
    let back = [rl as u8, (rl >> 8) as u8, (rl >> 16) as u8, (rl >> 24) as u8, rh as u8, (rh >> 8) as u8];
    ctx.log_fmt(format_args!(
        "dwc2-svc: unicast filter reads back {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} - {}",
        back[0], back[1], back[2], back[3], back[4], back[5],
        if back == mac { "matches the station MAC" } else { "DOES NOT MATCH - unicast will be dropped" }));

    // Turbo RX: the chip aggregates MANY frames into one bulk-IN burst, each with its 4-byte status word
    // and DWORD-aligned - which is exactly what `rx` already parses. Read-modify-write, because a bare
    // write clears the power-on defaults; clear RXDOFF so the frame sits immediately after its status.
    let hw = (smsc_read_for_log(ctx, m, d, t, SMSC_HW_CFG) | SMSC_HW_CFG_BIR | SMSC_HW_CFG_MEF | SMSC_HW_CFG_BCE)
             & !SMSC_HW_CFG_RXDOFF;
    smsc_write(ctx, m, d, t, SMSC_HW_CFG, hw);
    smsc_write(ctx, m, d, t, SMSC_BURST_CAP, SMSC_BURST_PKTS);
    // 0x800, the value Linux's `smsc95xx` uses (`DEFAULT_BULK_IN_DELAY`). This was 0x2000 - four times
    // the reference - which makes the device hold a received frame longer while it waits to batch more
    // behind it. That is invisible on bulk traffic and costly on sparse traffic, which is exactly the
    // shape of a ping: one small frame, nothing behind it. The doctrine for this port is that the
    // working driver tells us what the silicon wants; departing from it needs a reason, and there was
    // none recorded for this one.
    smsc_write(ctx, m, d, t, SMSC_BULK_IN_DLY, 0x800);
    // Clear whatever interrupt status survived the reset, as `smsc95xx_reset` does before going on.
    smsc_write(ctx, m, d, t, SMSC_INT_STS, 0xFFFF_FFFF);
    // Linux's `/* Init Tx */` is two writes and we only ever did one of them.
    smsc_write(ctx, m, d, t, SMSC_FLOW, 0);
    smsc_write(ctx, m, d, t, SMSC_AFC_CFG, 0x00F8_30A1);
    // THE VLAN TAG ETHERTYPE. See `SMSC_VLAN1`: this tells the MAC which ethertype means "this frame is
    // VLAN-tagged". Linux sets it to ETH_P_8021Q; we never set it at all, leaving the MAC to decide from
    // whatever the reset left - and a MAC that mistakes a protocol's ethertype for a VLAN tag misparses
    // exactly that protocol and nothing else.
    smsc_write(ctx, m, d, t, SMSC_VLAN1, 0x0000_8100);
    // The multicast hash table, zeroed. `smsc95xx_set_multicast` writes both halves on every filter
    // change; for an interface that has joined no multicast groups - which is this one - the correct
    // contents are zero, and reset garbage here is a receive filter configured by accident.
    smsc_write(ctx, m, d, t, SMSC_HASHH, 0);
    smsc_write(ctx, m, d, t, SMSC_HASHL, 0);

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
    let cr = (smsc_read_for_log(ctx, m, d, t, SMSC_MAC_CR)
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
    let cr  = smsc_read_for_log(ctx, m, d, t, SMSC_MAC_CR);
    let txc = smsc_read_for_log(ctx, m, d, t, SMSC_TX_CFG);
    let hwc = smsc_read_for_log(ctx, m, d, t, SMSC_HW_CFG);
    let bmsr = mii_read(ctx, m, d, t, SMSC_MII_BMSR);
    ctx.log_fmt(format_args!(
        "dwc2-svc: smsc readback MAC_CR=0x{:08x} (TXEN {} RXEN {}) TX_CFG=0x{:08x} HW_CFG=0x{:08x} BMSR={} link {}",
        cr, cr & SMSC_MAC_CR_TXEN != 0, cr & SMSC_MAC_CR_RXEN != 0, txc, hwc,
        match bmsr { Some(v) => v, None => 0xFFFF },
        match bmsr {
            Some(v) if v & 0x0020 == 0 => "NEGOTIATING (this instant, not a verdict - autonegotiation was restarted microseconds ago and takes seconds; ask again with `net`)",
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

/// Bytes held for the client to collect, sized to ONE BURST so it cannot overflow.
///
/// **Twice I sized this as a COUNT of frames and twice hardware disproved the count.** Four, on the
/// reasoning that a burst carries "two or three": 62 frames arrived in 24 bursts and 32 were dropped.
/// Sixteen, "with room to spare": 43 frames arrived in 7 bursts - six per burst - and 9 were dropped,
/// which came out as ping loss again. The mistake was not the number. It was storing frames in a fixed
/// number of MAX-SIZED slots when what actually arrives is a fixed number of BYTES.
///
/// A burst is `RX_BURST` bytes, so frames back-to-back with a two-byte length each cannot exceed
/// `RX_BURST` plus that overhead - and the smallest possible frame bounds the overhead at 2 bytes per
/// ~60, so 256 is generous. Sized this way the buffer holds ANY burst by construction: no count to get
/// wrong, and `rx_dropped` becomes a counter that should now never move.
///
/// It is also SMALLER: about 4 KiB, where a slot array covering the same worst case (a burst of minimum
/// frames, ~60 of them) would be 93 KiB of a 256 KiB stack. This is 26.6.1 exactly - the fix for a
/// working set that will not fit is to change its shape, not to grow the allocation.
///
/// Still sized to one BURST, not to a backlog: a client that stops collecting must see loss, counted,
/// rather than have the driver grow to hide it.
pub const RXQ_BYTES: usize = RX_BURST + 256;
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
        // CARRY THE DATA TOGGLE ACROSS THE HALT.
        //
        // A bulk endpoint's toggle alternates per packet and BOTH ends track it; if they disagree the
        // device's next packet is rejected as a DATATGLERR and the frame is destroyed. The completion
        // path below reads the toggle back out of HCTSIZ for exactly this reason. This path did not,
        // and it is the one that runs most: every transmit stands the armed IN down, so every transmit
        // re-armed the receive channel with whatever toggle was current BEFORE the halt.
        //
        // On hardware that showed as `HCINT=0x00000410` - DATATGLERR|NAK - on a channel that had been
        // outstanding for forty polls, with the device's own RX FIFO reading EMPTY. It was not that the
        // reply had not arrived; the device sent it, the core rejected it on the toggle, and the frame
        // was gone. Intermittent, because the toggle only disagrees on half the sequences.
        //
        // The in-kernel driver this service replaced never hit it: it transmitted on its own channel and
        // never stood the receive channel down at all. The halt is this port's addition - it is needed,
        // because an armed IN occupies the request-queue entry a transmit needs - so the toggle has to
        // be carried across it rather than the halt being removed.
        nic.pid_in = chan::pid_from_hctsiz(mmio, CH_NET_RX);
        nic.in_armed = false;
    }
    let given = frame.len().min(FRAME_MAX);
    // PAD TO THE ETHERNET MINIMUM. IEEE 802.3 sets the smallest legal frame at 64 bytes including the
    // 4-byte FCS, so 60 bytes of frame. Anything shorter is a RUNT, and a runt is not a small frame -
    // it is an error, discarded by the first switch that sees it.
    //
    // This matters because of exactly one caller: an ARP request is 42 bytes (14 ethernet + 28 ARP) and
    // is the ONLY thing this stack transmits that is under the minimum. Everything else clears it
    // comfortably - a DHCP DISCOVER is 286, an ICMP echo 74 - and everything else gets answered. On this
    // board DHCP completed on a unicast reply while ARP for the gateway never drew one, through repeated
    // runs, which is the same split.
    //
    // It is fixed HERE rather than in the callers because it is a link-layer property, not something
    // ARP should know about: every frame leaving this driver must be a legal frame, whoever built it.
    // Higher layers hand over an ARP request or an ICMP echo; the minimum length on the wire is the
    // driver's business, and putting it here means a future short frame cannot reintroduce this.
    //
    // Most MACs pad automatically and Linux relies on it (`smsc95xx_tx_fixup` does not pad, and neither
    // does usbnet). Relying on it is the part worth dropping: padding costs 18 zero bytes on the one
    // frame type that needs it, and removes a dependency on undocumented silicon behaviour that cannot
    // be confirmed from here.
    const ETH_MIN: usize = 60;
    let n = given.max(ETH_MIN);
    // TX_CMD_A = len | FIRST_SEG | LAST_SEG, TX_CMD_B = len. Both little-endian.
    let a = (n as u32) | 0x0000_2000 | 0x0000_1000;
    for (i, b) in a.to_le_bytes().iter().enumerate() {
        dma.write8(TX_OFF + i, *b);
    }
    for (i, b) in (n as u32).to_le_bytes().iter().enumerate() {
        dma.write8(TX_OFF + 4 + i, *b);
    }
    for i in 0..given {
        dma.write8(TX_OFF + 8 + i, frame[i]);
    }
    // The pad itself must be written, not merely counted. The DMA arena is reused between transmits, so
    // whatever the previous frame left in these bytes would otherwise go out on the wire as the tail of
    // this one - a leak of the last frame's contents, and not the zeros the padding is supposed to be.
    for i in given..n {
        dma.write8(TX_OFF + 8 + i, 0);
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
    // COUNT THE BYTES. `.is_some()` treated a SHORT transfer as a complete one, and that is how the
    // device ends up wedged: the 8-byte command tells it a frame of `n` bytes is coming, we deliver
    // fewer, and it holds the incomplete frame at the head of its transmit FIFO waiting for a
    // remainder that never arrives. Nothing behind it can go out, so the MAC has nothing complete to
    // send while its FIFO stays nearly full - exactly what the device reported at the refusal:
    // TX_FIFO_FREE=508, TX_ON set, INT_STS clear, MAC_CR healthy, link full duplex.
    //
    // One short transfer therefore kills transmit permanently, which is why it always died abruptly
    // after a few hundred good frames and never came back.
    let ok = match bulk(ctx, mmio, t, nic.mps, false, nic.ep_out, dma.phys_at(TX_OFF) as u32,
                        total, TX_BUDGET_MS, &mut nic.pid_out, Some(&mut why), Some(&mut ping)) {
        Some(got) if got == total => true,
        Some(got) => {
            ctx.log_fmt(format_args!(
                "dwc2-svc: SHORT transmit - {} of {} bytes went; the device now holds a partial frame",
                got, total));
            false
        }
        None => false,
    };
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
            // ASK THE DEVICE, do not infer it. The USB endpoint NAKing means the chip has no room for
            // the frame - but "no room" has several causes on this part and they are distinguishable
            // from its own registers, which nothing has ever read at the moment of failure:
            //   TX_CFG  bit 2 ON, bit 1 STOP, bit 0 FIFO_FLUSH - is the transmitter even running?
            //   INT_STS bit 17 TX_STOP          - did the chip stop it and tell us?
            //   MAC_CR  TXEN / FDPX / RCVOWN    - is the MAC configured for the link it got?
            //   FLOW / AFC_CFG                  - is it holding itself off with flow control?
            // Four theories died for want of these five words.
            // FLUSH THE DEVICE'S TRANSMIT FIFO. A partial frame at its head blocks everything
            // behind it forever and only the device can drop it. Frames queued behind are discarded,
            // which costs nothing when not one of them was going out anyway.
            smsc_write(ctx, mmio, dma, t, SMSC_TX_CFG, SMSC_TX_CFG_FIFO_FLUSH);
            smsc_write(ctx, mmio, dma, t, SMSC_TX_CFG, SMSC_TX_CFG_ON);
            ctx.log("dwc2-svc: flushed the DEVICE's TX FIFO - a partial frame blocks every frame behind it");
            ctx.log_fmt(format_args!(
                "dwc2-svc: device TX state at refusal - TX_FIFO_FREE={} RX_FIFO_USED={} TX_CFG={:#010x} INT_STS={:#010x} MAC_CR={:#010x} FLOW={:#010x} AFC_CFG={:#010x}",
                smsc_read_for_log(ctx, mmio, dma, t, SMSC_TX_FIFO_INF) & 0xFFFF,
                smsc_read_for_log(ctx, mmio, dma, t, SMSC_RX_FIFO_INF) & 0xFFFF,
                smsc_read_for_log(ctx, mmio, dma, t, SMSC_TX_CFG),
                smsc_read_for_log(ctx, mmio, dma, t, SMSC_INT_STS),
                smsc_read_for_log(ctx, mmio, dma, t, SMSC_MAC_CR),
                smsc_read_for_log(ctx, mmio, dma, t, SMSC_FLOW),
                smsc_read_for_log(ctx, mmio, dma, t, SMSC_AFC_CFG)));
            ctx.log_fmt(format_args!(
                "dwc2-svc: NIC refused {} frames in a row (HCINT={:#010x}{}) - backing off to one probe every {} ms. Frames are dropped meanwhile; input and storage stay responsive.",
                TX_FAIL_RUN, why.0, if why.1 != 0 { ", channel never halted" } else { "" },
                TX_BACKOFF_MS));
        }
    }
    // RE-ARM THE RECEIVE CHANNEL NOW, not on the next poll.

    // The stand-down above is necessary - a NAK-retrying IN fills the core's non-periodic request
    // queue and the transmit cannot be scheduled at all until it stands aside. Leaving the re-arm to
    // the next `rx` call is what was not: that is up to a poll interval away, and the poll that gets
    // there only ARMS (it returns 0 and harvests on the call after), so the channel was unarmed for
    // tens of milliseconds after every transmit.
    //
    // A ping reply arrives about 16 ms after its request. That lands squarely in the gap, and the
    // device then holds the frame in its own FIFO - which is what the counters showed: 91 polls over
    // a 905 ms window seeing one to four frames and never the reply, `nohalt` climbing, and
    // `RX_FIFO_INF` non-zero with data waiting. The frame was never lost; we simply were not
    // listening when it came, and then asked a device that had already decided to batch it.
    //
    // Arming here closes the gap to the length of the transmit itself.
    if !nic.in_armed {
        arm_in(mmio, dma, t, nic);
    }
    ok
}

/// Receive one burst and hand each complete frame to `deliver`.
///
/// Returns the number of frames delivered. Zero is the ordinary answer on a quiet network and is not
/// a failure.
impl Nic {
    /// Append a frame, stored as `[len:u16 LE][bytes]`. Sized to a whole burst, so the overflow arm
    /// should never run - it counts rather than hides if it ever does.
    pub fn rxq_push(&mut self, f: &[u8]) {
        let n = f.len().min(FRAME_MAX);
        if self.rxbuf_fill + 2 + n > RXQ_BYTES {
            self.stats.rx_dropped = self.stats.rx_dropped.saturating_add(1);
            return;
        }
        let at = self.rxbuf_fill;
        self.rxbuf[at] = (n & 0xFF) as u8;
        self.rxbuf[at + 1] = ((n >> 8) & 0xFF) as u8;
        self.rxbuf[at + 2..at + 2 + n].copy_from_slice(&f[..n]);
        self.rxbuf_fill = at + 2 + n;
        self.rxq_count += 1;
    }

    /// Copy the oldest queued frame into `out` and remove it. Returns its length, 0 if empty.
    ///
    /// Emptying resets the cursor, which is what lets the next burst reuse the whole buffer - `rx` is
    /// only called when nothing is queued, so exactly one burst is ever resident.
    pub fn rxq_pop(&mut self, out: &mut [u8]) -> usize {
        if self.rxq_count == 0 {
            self.rxbuf_pos = 0;
            self.rxbuf_fill = 0;
            return 0;
        }
        let at = self.rxbuf_pos;
        let n = ((self.rxbuf[at] as usize) | ((self.rxbuf[at + 1] as usize) << 8)).min(out.len());
        out[..n].copy_from_slice(&self.rxbuf[at + 2..at + 2 + n]);
        self.rxbuf_pos = at + 2 + n;
        self.rxq_count -= 1;
        if self.rxq_count == 0 {
            self.rxbuf_pos = 0;
            self.rxbuf_fill = 0;
        }
        n
    }
}

/// Arm the background bulk-IN so the device has somewhere to put the next frame.
///
/// Extracted so `tx` can call it the moment it finishes, rather than leaving the channel unarmed until
/// the next receive poll - see the call there for what that gap cost.
fn arm_in(mmio: &Mmio, dma: &Dma, t: &Target, nic: &mut Nic) {
    // Clear HCINT before arming: it is write-1-to-clear and holds whatever the LAST transfer on this
    // channel left behind. Arming without clearing means the first check reads a stale completion and
    // harvests a buffer the device has not written yet.
    mmio.write32(chan::hcint_at(CH_NET_RX), 0xFFFF_FFFF);
    chan::program(mmio, &Target { addr: t.addr, mps: nic.mps, low_speed: false }, CH_NET_RX,
                  true, nic.pid_in, RX_BURST as u32, dma.phys_at(RX_OFF) as u32,
                  nic.ep_in as u32, 2, 0);
    // Unmask this channel's terminal halt, exactly as the working kernel driver does before it arms its
    // background IN. `chan::program` zeroes HCINTMSK for every channel, which is invisible for a channel
    // programmed and polled in the same breath - and this is the only channel left RUNNING UNATTENDED,
    // so it is the one place the assumption carries weight.
    mmio.write32(chan::hcintmsk_at(CH_NET_RX),
                 crate::regs::HCINT_CHHLTD | crate::regs::HCINT_XFERCOMPL);
    mmio.write32(crate::regs::HAINTMSK,
                 mmio.read32(crate::regs::HAINTMSK) | (1 << CH_NET_RX));
    nic.in_armed = true;
}

pub fn rx(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, nic: &mut Nic,
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
        arm_in(mmio, dma, t, nic);
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
        // A long run of this used to be logged here, on the way to finding the data toggle error.
        // It is gone, and the reason is worth keeping: the line fired on the ORDINARY quiet case, so it
        // read as an alarm when nothing was wrong. Everything it printed is already in the periodic
        // report (`net IN HCINT=... nohalt N`, which carries the same interrupt state and the same
        // outstanding-poll count as a plain fact rather than a warning), and the fault it was built to
        // find now has `rx_tglerr` - silent when healthy, non-zero only when frames are actually being
        // destroyed, and always on rather than capped at four reports.
        //
        // A warning that fires when nothing is wrong is worse than no warning: it teaches the reader to
        // skip the line, which is how a real one gets skipped later.
        return 0;
    }
    if hcint & crate::regs::HCINT_STALL != 0 {
        // A halted endpoint is a hard failure, never retried by re-arming into the same condition.
        nic.stats.rx_hcint = hcint;
        nic.in_armed = false;
        return 0;
    }
    if hcint & crate::regs::HCINT_DATATGLERR != 0 {
        nic.stats.rx_tglerr = nic.stats.rx_tglerr.saturating_add(1);
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
        // Classify by destination address before the frame goes anywhere. See `Stats::rx_unicast`.
        if n >= 6 {
            if buf[..6] == nic.mac[..] {
                nic.stats.rx_unicast += 1;
                if n >= 14 {
                    match (buf[12], buf[13]) {
                        (0x08, 0x06) => nic.stats.rx_uni_arp += 1,
                        (0x08, 0x00) => nic.stats.rx_uni_ipv4 += 1,
                        _ => {}
                    }
                }
            } else if buf[..6] == [0xFF; 6] {
                nic.stats.rx_bcast += 1;
            } else {
                nic.stats.rx_other += 1;
            }
        }
        // Straight into the receive queue. `rx` already holds `&mut Nic`, so there is no reason to
        // hand the frame out through a closure and copy it into a staging array first - and the
        // staging array had to be bounded separately, which is where frames were being lost.
        nic.rxq_push(&buf[..n.min(FRAME_MAX)]);
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
                // HOW MANY BYTES WENT - and for an OUT the answer is NOT in HCTSIZ.
                //
                // Linux `dwc2_get_actual_xfer_length`: on completion it uses `xfer_len - count` for
                // an IN and plain `chan->xfer_len` for a non-split OUT, and the comment beside it
                // says why - "hctsiz.xfersize reflects the number of bytes transferred via the AHB,
                // not the USB". Reading it for an OUT reports a residue that never had the meaning
                // being asked of it.
                //
                // I did read it, called every transmit short, and drove tx_ok to zero while the
                // device was ACKing every frame (HCINT 0x23 = XFERCOMPL|CHHLTD|ACK). Sixty-nine
                // phantom "SHORT transmit" lines in one boot, all of them mine.
                let left = if dir_in {
                    mmio.read32(chan::hctsiz_at(CH_NET)) & 0x7_FFFF
                } else {
                    0
                };
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
    // EVERY REPLY IS TAGGED WITH THE OP IT ANSWERS.
    //
    // This endpoint carries three net ops - INFO, TX, RX - on one channel, and until now a reply said
    // nothing about which one it was answering. That is safe only while request and reply stay in
    // lockstep, and they do not: `nic-driver` bounds its wait, so a reply that arrives after its
    // deadline is still queued when the NEXT request goes out. From then on every answer is one behind,
    // permanently, and the damage is not merely a late answer - an RX reply read as an INFO reply is a
    // FRAME consumed as a status word. The frame is destroyed and the status is garbage.
    //
    // That is what the counters were showing: six unicast ARP frames arrived and were parsed here,
    // `0 DROPPED, 0 queued`, and net-stack reported no ARP replies at all. The gateway was answering
    // the whole time.
    //
    // It is also why this worked before the driver left the kernel. `nic-driver` used to reach the
    // device through the `net_frame_rx` SYSCALL - one call, one frame, no reply stream to misalign.
    // The IPC hop that replaced it introduced the failure, so the hop is where the fix belongs.
    //
    // `fs` has had exactly this and exactly this fix since the "run ls twice" desync
    // (`docs/net-tags-design.md` is the design for this path). The tag costs one byte per reply.
    let mut out = [0u8; FRAME_MAX + 5];
    out[0] = p[0];
    let body = &mut out[1..];
    let n = match p[0] {
        OP_NET_INFO => {
            // [ok, mac(6), link]. The link bit is reported as UP: this driver does not yet read the
            // PHY, and saying so here rather than inventing a "down" keeps net-stack from concluding
            // the cable is out. Reading it properly is the remaining piece of this slice.
            body[0] = 1;
            body[1..7].copy_from_slice(&nic.mac);
            // The PHY's own link bit, not an assumption. Reporting a hardcoded UP made net-stack
            // spend its DHCP budget against a cable that may not be there, and made "no link" and
            // "link up but silent" indistinguishable from the outside - the two things a diagnosis
            // most needs to tell apart. Unreadable counts as DOWN: an unanswerable question is not
            // a yes.
            // This is a fresh read of the PHY, so let the transmit cache learn from it too - the
            // link question and the transmit guard must never disagree about the same cable.
            let up = link_up(ctx, mmio, dma, t);
            let now = ctx.read_tsc();
            link_observed(ctx, mmio, dma, t, nic, up, now);
            nic.stats.bmsr = mii_read(ctx, mmio, dma, t, SMSC_MII_BMSR).map_or(0xFFFF, u32::from);
            // Fallible here: these three feed the periodic report, where a fabricated zero reads as a
            // fact about the device. Counting the failures makes a zero attributable.
            match smsc_read(ctx, mmio, dma, t, SMSC_RX_FIFO_INF) {
                Some(v) => nic.stats.rx_fifo = v,
                None => nic.stats.reg_read_fails = nic.stats.reg_read_fails.saturating_add(1),
            }
            match smsc_read(ctx, mmio, dma, t, SMSC_TX_FIFO_INF) {
                Some(v) => nic.tx_fifo_free = v & 0xFFFF,
                None => nic.stats.reg_read_fails = nic.stats.reg_read_fails.saturating_add(1),
            }
            match smsc_read(ctx, mmio, dma, t, SMSC_INT_STS) {
                Some(v) => nic.stats.int_sts = v,
                None => nic.stats.reg_read_fails = nic.stats.reg_read_fails.saturating_add(1),
            }
            body[7] = u8::from(up);
            8
        }
        OP_NET_TX if p.len() > 1 => {
            body[0] = u8::from(tx(ctx, mmio, dma, t, nic, &p[1..]));
            1
        }
        OP_NET_RX => {
            // [n_lo, n_hi, frame...]. Zero length is "nothing received", which on a quiet network is
            // the ordinary answer and not a failure.
            // Only touch the wire when nothing is already waiting: a burst can carry several frames
            // and the client collects them one reply at a time.
            if nic.rxq_count == 0 {
                rx(ctx, mmio, dma, t, nic);
            }
            let mut frame = [0u8; FRAME_MAX];
            let got = nic.rxq_pop(&mut frame);
            if got > 0 { nic.stats.rx_popped = nic.stats.rx_popped.saturating_add(1); }
            body[2..2 + got].copy_from_slice(&frame[..got]);
            body[0] = (got & 0xFF) as u8;
            body[1] = ((got >> 8) & 0xFF) as u8;
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
            body[0] = 0;
            1
        }
    };
    // `n` is the BODY length; the tag at byte 0 rides in front of it.
    let _ = ctx.try_send_by_handle(reply, &godspeed_sdk::Message::from_bytes(&out[..n + 1]));
    ctx.remove_cap(reply);
    true
}
