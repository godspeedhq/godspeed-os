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
    pub pid_in: u32,
    pub pid_out: u32,
}

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
    Some(Nic { ep_in, ep_out, mps, mac, stats: Stats::default(),
               pid_in: chan::PID_DATA0, pid_out: chan::PID_DATA0 })
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
/// RX burst size in 512-byte high-speed packets. Must stay within `RX_BURST` in the arena layout.
const SMSC_BURST_PKTS: u32 = 4;

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
    //
    // DIAGNOSTIC, and temporary: PRMS (promiscuous) is set below instead of cleared. With the filter
    // narrowed to our-unicast + broadcast, the only traffic this chip accepts is addressed to
    // 02:00:00:12:34:56 - a locally-administered address nobody on the LAN sends to - so a quiet
    // network is indistinguishable from a broken receive path. Both read as "rx 0 bursts". Opening the
    // filter separates them with one bit: if bursts appear, RX works and the fault is that our
    // transmitted frames never reach the wire; if it stays at zero, RX itself is broken.
    //
    // This MUST come back out once that is known - dropping the multicast flood at the chip rather
    // than in our ring is why the filter was narrowed in the first place.
    let cr = (smsc_read_or0(ctx, m, d, t, SMSC_MAC_CR)
              & !(SMSC_MAC_CR_MCPAS | SMSC_MAC_CR_HPFILT))
             | SMSC_MAC_CR_PRMS | SMSC_MAC_CR_TXEN | SMSC_MAC_CR_RXEN | SMSC_MAC_CR_FDPX;
    if !smsc_write(ctx, m, d, t, SMSC_MAC_CR, cr) {
        ctx.log("dwc2-svc: smsc MAC_CR enable FAILED - NIC stays down");
        return None;
    }
    smsc_write(ctx, m, d, t, SMSC_TX_CFG, SMSC_TX_CFG_ON);
    ctx.log("dwc2-svc: NIC filter is PROMISCUOUS (diagnostic - splits a quiet network from a dead receive path; revert once known)");

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
pub const FRAME_MAX: usize = 1514;
/// One IN transfer can carry SEVERAL frames, so the receive burst is larger than one frame.
pub const RX_BURST: usize = 2048;
const _: () = assert!(TX_OFF >= crate::msc::DATA_OFF + crate::msc::DATA_MAX);
const _: () = assert!(RX_OFF >= TX_OFF + FRAME_MAX + 8);

/// Send one ethernet frame.
///
/// **The LAN9514 is NOT plain CDC-ECM.** It wants an 8-byte TX command in front of the frame, and a
/// driver that sends the raw frame instead is not rejected - the device silently drops it, which on a
/// NIC is the worst failure shape there is.
pub fn tx(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, nic: &mut Nic, frame: &[u8],
) -> bool {
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
    // No ZLP needed: the smsc95xx carries an explicit length in its TX command, so it does not rely on
    // a short packet to find the frame boundary the way CDC-ECM does.
    let ok = bulk(ctx, mmio, t, nic.mps, false, nic.ep_out, dma.phys_at(TX_OFF) as u32,
                  (n + 8) as u32, 2_000, &mut nic.pid_out).is_some();
    if ok { nic.stats.tx_ok += 1; } else { nic.stats.tx_fail += 1; }
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
    let got = match bulk(ctx, mmio, t, nic.mps, true, nic.ep_in, dma.phys_at(RX_OFF) as u32,
                         RX_BURST as u32, 50, &mut nic.pid_in) {
        Some(n) => n as usize,
        None => return 0, // NAK on a quiet network is the normal case, not an error
    };
    if got > 0 {
        nic.stats.rx_bursts += 1;
        nic.stats.rx_bytes = nic.stats.rx_bytes.wrapping_add(got as u32);
    }

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
            if frames == 0 { nic.stats.rx_bad += 1; }
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
fn bulk(
    ctx: &ServiceContext, mmio: &Mmio, t: &Target, mps: u16,
    dir_in: bool, ep: u8, buf_phys: u32, len: u32, budget_ms: u64, pid: &mut u32,
) -> Option<u32> {
    let bt = Target { addr: t.addr, mps, low_speed: false };
    let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(budget_ms));
    loop {
        chan::program(mmio, &bt, CH_NET, dir_in, *pid, len, buf_phys, ep as u32, 2, 0);
        match chan::wait_halt(ctx, mmio, CH_NET, 50) {
            Some(hcint) if hcint & crate::regs::HCINT_XFERCOMPL != 0 => {
                let left = mmio.read32(chan::hctsiz_at(CH_NET)) & 0x7_FFFF;
                *pid = chan::pid_from_hctsiz(mmio, CH_NET);
                return Some(len.saturating_sub(left));
            }
            Some(hcint) if hcint & crate::regs::HCINT_STALL != 0 => return None,
            _ => {}
        }
        if ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
            return None;
        }
    }
}

/// The NIC's own channel. Separate from CH_BULK (disk/control) and CH_KBD for the same reason those
/// are separate from each other: an abandoned transfer must not leave its state on a channel another
/// stream inherits.
pub const CH_NET: u32 = 2;

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
            out[7] = u8::from(link_up(ctx, mmio, dma, t));
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
        _ => return false, // not a net op - the block server gets a look at it
    };
    let _ = ctx.try_send_by_handle(reply, &godspeed_sdk::Message::from_bytes(&out[..n]));
    ctx.remove_cap(reply);
    true
}
