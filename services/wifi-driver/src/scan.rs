// SPDX-License-Identifier: GPL-2.0-only
//! Scanning: the first thing the firmware does that the host did not ask for synchronously.
//!
//! **This is a different shape from everything before it.** Every exchange so far is send-a-command,
//! read-the-answer. A scan is not: `escan` is a *set* that returns immediately, and the results arrive
//! afterwards as a stream of **events the firmware sends unprompted**. `docs/wifi.md` §30 is the full
//! design; this module is it in code.
//!
//! ## Which channel a frame came from
//!
//! The SDIO software header's channel tells the three cases apart, quoted from OpenBSD's `bwfm`:
//!
//! ```c
//! switch (swhdr->chanflag & BWFM_SDIO_SWHDR_CHANNEL_MASK) {
//! case BWFM_SDIO_SWHDR_CHANNEL_CONTROL:
//! 	sc->sc_sc.sc_proto_ops->proto_rxctl(...);
//! case BWFM_SDIO_SWHDR_CHANNEL_EVENT:
//! case BWFM_SDIO_SWHDR_CHANNEL_DATA:
//! 	sc->sc_sc.sc_proto_ops->proto_rx(&sc->sc_sc, m, &ml);
//! ```
//!
//! Channel 0 is a BCDC control reply, which `ctrl` reads. Channels 1 and 2 are frames, and an event is a
//! **pseudo-ethernet frame** carrying ethertype `BWFM_ETHERTYPE_LINK_CTL` (`0x886c`) - not a bare struct.
//!
//! ## The layouts, and why the offsets are constants here rather than a struct
//!
//! These are wire layouts defined by someone else's `__packed` C structs. Rust cannot express them as a
//! struct without `repr(packed)` and byte-order wrappers on every field, so they are read by explicit offset
//! from a byte slice - which is also what makes every offset checkable against `docs/wifi.md` §30 by eye.
//! The declarations they were computed from, quoted:
//!
//! ```c
//! struct bwfm_ethhdr {
//! 	uint16_t subtype;  uint16_t length;  uint8_t version;
//! 	uint8_t oui[3];    uint16_t usr_subtype;
//! } __packed;
//!
//! struct bwfm_event_msg {
//! 	uint16_t version;  uint16_t flags;    uint32_t event_type;
//! 	uint32_t status;   uint32_t reason;   uint32_t auth_type;
//! 	uint32_t datalen;  struct ether_addr addr;
//! 	char ifname[IFNAMSIZ];  uint8_t ifidx;  uint8_t bsscfgidx;
//! } __packed;
//! ```
//!
//! `ether_header` is 14, `bwfm_ethhdr` is 10 packed, `bwfm_event_msg` is 48 packed, so the event payload
//! begins at 72.
//!
//! ## What is OURS rather than the firmware's (§26.14)
//!
//! The layouts and values above are the firmware's requirement, copied exactly. How a scan is *organised* is
//! not, and Linux's answers to Linux's constraints are deliberately left behind:
//!
//! - **A fixed array of results, no heap** (§26.6.1). Linux grows a list; this keeps 32 and says loudly when
//!   it is full. The bound is readable off the source.
//! - **No callback and no work queue** (§26.4). The driver reads frames in its own loop. There is nothing to
//!   call back into - a `cfg80211`-shaped layer is a stated non-goal (§9).
//! - **Scan state is owned by the call doing the scan** (§3.8), not held driver-wide.
//! - **A deadline, and a partial answer labelled partial** (§26.6). The truth being waited on is the
//!   firmware reporting the scan complete; the deadline is the bound underneath it, and which of the two
//!   ended the wait is printed.
//!
//! ## The ladder
//!
//! Three separable unknowns - the event offsets, the request layout, the result parsing - so they are
//! reported separately (§30.6). A `datalen` consistent with the frame length is the self-check: if the
//! offsets are wrong, `datalen` is nonsense, exactly as `frmlen ^ cksum` catches a frame that is not there.
//!
//! **None of this has run on hardware.** It was written away from the bench; §30 says so and so does this.

use godspeed_sdk::ServiceContext;

use crate::backplane::Window;
use crate::ctrl;
use crate::host::Host;

/// `BWFM_SDIO_SWHDR_CHANNEL_MASK`.
pub const CHANNEL_MASK: u8 = 0x0F;
/// `BWFM_SDIO_SWHDR_CHANNEL_EVENT`.
pub const CHANNEL_EVENT: u8 = 0x01;
/// `BWFM_SDIO_SWHDR_CHANNEL_DATA`.
pub const CHANNEL_DATA: u8 = 0x02;
/// `BWFM_SDIO_SWHDR_CHANNEL_GLOM` - several frames aggregated into one by the firmware.
///
/// **Ignored, and ignored on purpose.** The reference's receive path switches on CONTROL, EVENT and DATA and
/// has **no GLOM case at all** - a glommed frame falls through and is dropped - and its scans work. So the
/// escan results do not arrive this way, and deaggregating would be work with nothing waiting on it (§26.2).
///
/// It is COUNTED rather than silently skipped, because "the firmware sent 17 frames this driver does not
/// read" is a fact worth seeing, and because if results ever fail to arrive this is the first number to look
/// at. There is no iovar to turn it off: `bus:txglom` exists and is TX-only, and no `bus:rxglom` appears in
/// the reference.
pub const CHANNEL_GLOM: u8 = 0x03;

/// `BWFM_ETHERTYPE_LINK_CTL` - the ethertype that marks a frame as an event rather than traffic.
const ETHERTYPE_LINK_CTL: u16 = 0x886C;

/// `BCDC_HEADER_LEN` - the header a DATA or EVENT frame carries before its ethernet frame.
///
/// ```c
/// struct brcmf_proto_bcdc_header { u8 flags; u8 priority; u8 flags2; u8 data_offset; };
/// #define BCDC_HEADER_LEN 4
///
/// skb_pull(pktbuf, BCDC_HEADER_LEN);
/// ...
/// skb_pull(pktbuf, h->data_offset << 2);
/// ```
///
/// **Two pulls, not one**, and the second is scaled by four because `data_offset` counts words.
///
/// **This is a DIFFERENT header from the one a control reply carries.** A CONTROL frame's body begins with
/// the 16-byte `brcmf_proto_bcdc_dcmd` (cmd/len/flags/status); a DATA or EVENT frame's begins with this
/// 4-byte one. Same protocol, two shapes, chosen by the SDPCM channel - which is why treating "the BCDC
/// header" as a single thing put every event's ethertype 4 bytes out and produced `0x541c`, two bytes of the
/// device's own MAC.
const BDC_HEADER: usize = 4;

/// Where `data_offset` sits in that header, in words.
const BDC_DATA_OFFSET: usize = 3;

/// Offsets into an event frame, computed from the quoted declarations. See `docs/wifi.md` §30.2.
mod ev {
    /// `ether_header` is 14 bytes; the ethertype is its last field.
    pub const ETHERTYPE: usize = 12;
    /// `bwfm_ethhdr` follows the ethernet header.
    pub const ETHHDR: usize = 14;
    /// `bwfm_event_msg` follows it - 2+2+1+3+2 = 10 bytes packed.
    pub const MSG: usize = ETHHDR + 10;
    /// `event_type`, 4 bytes into the message.
    pub const EVENT_TYPE: usize = MSG + 4;
    /// `status`.
    pub const STATUS: usize = MSG + 8;
    /// `datalen`.
    pub const DATALEN: usize = MSG + 20;
    /// The event payload - 2+2+4+4+4+4+4+6+16+1+1 = 48 bytes of message.
    pub const PAYLOAD: usize = MSG + 48;
}

/// Event codes, quoted from `bwfmreg.h`.
mod code {
    /// `BWFM_E_SET_SSID`.
    pub const SET_SSID: u32 = 0;
    /// `BWFM_E_ASSOC`.
    pub const ASSOC: u32 = 7;
    /// `BWFM_E_LINK`.
    pub const LINK: u32 = 16;
    /// `BWFM_E_ESCAN_RESULT`.
    pub const ESCAN_RESULT: u32 = 69;

    /// A name for the log, so an unexpected event is legible rather than a bare number.
    pub fn name(c: u32) -> &'static str {
        match c {
            SET_SSID => "SET_SSID",
            ASSOC => "ASSOC",
            LINK => "LINK",
            ESCAN_RESULT => "ESCAN_RESULT",
            _ => "(not an event this driver names)",
        }
    }
}

/// Offsets into `struct bwfm_escan_results`. See `docs/wifi.md` §30.2.
mod res {
    pub const BUFLEN: usize = 0;
    pub const BSS_COUNT: usize = 10;
    pub const FIRST: usize = 12;
}

/// Offsets into `struct bwfm_bss_info`, computed from the quoted declaration.
mod bss {
    /// **Step to the next entry with THIS, never a compiled-in size.** The struct is versioned, so a
    /// `sizeof` would walk off alignment the moment the firmware sends a longer one.
    pub const LENGTH: usize = 4;
    pub const BSSID: usize = 8;
    pub const SSID_LEN: usize = 18;
    pub const SSID: usize = 19;
    pub const CHANSPEC: usize = 72;
    pub const RSSI: usize = 78;
    /// The shortest entry this driver will read a full record out of.
    pub const MIN: usize = 80;
}

/// Offsets into the `escan` request. See `docs/wifi.md` §30.3.
mod req {
    pub const VERSION: usize = 0;
    pub const ACTION: usize = 4;
    pub const SYNC_ID: usize = 6;
    pub const SSID_LEN: usize = 8;
    pub const BSSID: usize = 44;
    pub const BSS_TYPE: usize = 50;
    pub const SCAN_TYPE: usize = 51;
    pub const NPROBES: usize = 52;
    pub const ACTIVE_TIME: usize = 56;
    pub const PASSIVE_TIME: usize = 60;
    pub const HOME_TIME: usize = 64;
    pub const CHANNEL_NUM: usize = 68;
    /// Total with no channel list, which means "every channel".
    pub const SIZE: usize = 72;
}

/// `WL_ESCAN_ACTION_START`.
const ACTION_START: u16 = 1;
/// `BWFM_SCANTYPE_ACTIVE`.
const SCANTYPE_ACTIVE: u8 = 0;
/// `DOT11_BSSTYPE_ANY`.
const BSSTYPE_ANY: u8 = 2;
/// The `sync_id` the reference uses, echoed back in every result so a stale scan's events are recognisable.
const SYNC_ID: u16 = 0x1234;

/// `BWFM_MAX_SSID_LEN`.
const MAX_SSID: usize = 32;
/// How many networks to keep. Bounded on purpose: a fixed array whose limit is readable here (§26.6.1).
const MAX_RESULTS: usize = 32;

/// One network, as much of it as this driver reports.
#[derive(Clone, Copy)]
pub struct Network {
    pub bssid: [u8; 6],
    pub ssid: [u8; MAX_SSID],
    pub ssid_len: u8,
    pub chanspec: u16,
    pub rssi: i16,
}

impl Network {
    fn blank() -> Self {
        Network { bssid: [0; 6], ssid: [0; MAX_SSID], ssid_len: 0, chanspec: 0, rssi: 0 }
    }
}

/// The results of one scan. Owned by the call that ran it, not held driver-wide (§3.8).
pub struct Scan {
    nets: [Network; MAX_RESULTS],
    count: usize,
    /// Results the firmware reported that did not fit. Counted rather than dropped silently (§26.7).
    dropped: u32,
    /// Events seen, so rung A is reportable even when nothing parses.
    events: u32,
    /// Escan-result events specifically.
    results: u32,
    /// Glommed frames seen and not read. See `CHANNEL_GLOM`.
    glom: u32,
    /// Frames on a channel this driver does not read at all.
    other: u32,
}

impl Scan {
    pub fn new() -> Self {
        Scan {
            nets: [Network::blank(); MAX_RESULTS],
            count: 0,
            dropped: 0,
            events: 0,
            results: 0,
            glom: 0,
            other: 0,
        }
    }

    /// Keep a network, deduplicating by BSSID - the same AP is reported once per channel it beacons on.
    fn keep(&mut self, n: Network) {
        for i in 0..self.count {
            if self.nets[i].bssid == n.bssid {
                // A later sighting with a stronger signal is the better one to report.
                if n.rssi > self.nets[i].rssi {
                    self.nets[i] = n;
                }
                return;
            }
        }
        if self.count == MAX_RESULTS {
            self.dropped += 1;
            return;
        }
        self.nets[self.count] = n;
        self.count += 1;
    }

    pub fn networks(&self) -> &[Network] {
        &self.nets[..self.count]
    }
}

/// Build the `escan` request. Values quoted from `brcmf_escan_prep`; see `docs/wifi.md` §30.3.
///
/// Every `-1` means "firmware default". A scan tuned by hand is an optimisation of something that does not
/// work yet (§26.2), so the defaults stand.
fn build_request(out: &mut [u8; req::SIZE]) {
    out.fill(0);
    // `version`: the v0 params this driver targets. §30.4 records that Linux and OpenBSD disagree about
    // this struct's shape across versions, and that v0 is the choice being tested first.
    out[req::VERSION..req::VERSION + 4].copy_from_slice(&1u32.to_le_bytes());
    out[req::ACTION..req::ACTION + 2].copy_from_slice(&ACTION_START.to_le_bytes());
    out[req::SYNC_ID..req::SYNC_ID + 2].copy_from_slice(&SYNC_ID.to_le_bytes());
    // `ssid.len` 0 with a zero SSID is a broadcast scan - every network, not a named one.
    out[req::SSID_LEN..req::SSID_LEN + 4].copy_from_slice(&0u32.to_le_bytes());
    // `eth_broadcast_addr(params_le->bssid)`.
    for b in out[req::BSSID..req::BSSID + 6].iter_mut() {
        *b = 0xFF;
    }
    out[req::BSS_TYPE] = BSSTYPE_ANY;
    out[req::SCAN_TYPE] = SCANTYPE_ACTIVE;
    let default = (-1i32) as u32;
    out[req::NPROBES..req::NPROBES + 4].copy_from_slice(&default.to_le_bytes());
    out[req::ACTIVE_TIME..req::ACTIVE_TIME + 4].copy_from_slice(&default.to_le_bytes());
    out[req::PASSIVE_TIME..req::PASSIVE_TIME + 4].copy_from_slice(&default.to_le_bytes());
    out[req::HOME_TIME..req::HOME_TIME + 4].copy_from_slice(&default.to_le_bytes());
    // `channel_num` 0: every channel, and no channel list follows.
    out[req::CHANNEL_NUM..req::CHANNEL_NUM + 4].copy_from_slice(&0u32.to_le_bytes());
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Big-endian, for the EVENT MESSAGE HEADER only.
///
/// ```c
/// struct brcmf_event_msg_be {
/// 	__be16 version;  __be16 flags;    __be32 event_type;
/// 	__be32 status;   __be32 reason;   __be32 auth_type;
/// 	__be32 datalen;  ...
/// } __packed;
/// ```
///
/// The `_be` is the whole point: an event rides inside an ethernet frame and is in network byte order. The
/// escan RESULT BODY it carries is a firmware structure and stays little-endian (`brcmf_escan_result_le`,
/// `brcmf_bss_info_le`), so exactly one layer flips and the others do not.
///
/// Observed before this existed: `event_type read as 1157627904` - which is `0x45000000`, the bytes
/// `00 00 00 45`, which is **69**, `ESCAN_RESULT`, read the wrong way round. Every one of the twelve
/// frames the self-check rejected was a scan result with its length in the right place.
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Parse one event frame. Returns `(event_type, status)` when it really is an event.
///
/// **The self-check is `datalen`.** If the offsets in `ev` are wrong, `datalen` is nonsense against the
/// frame length, so the arithmetic checks itself the way `frmlen ^ cksum` checks a frame's existence. A
/// frame that fails it is reported rather than parsed.
fn parse_event(body: &[u8], which: u32, ctx: &ServiceContext) -> Option<(u32, u32, usize, usize)> {
    // THE BDC HEADER FIRST. A data or event frame is not an ethernet frame: it carries four bytes of BDC
    // header and then `data_offset << 2` more before the ethernet header starts.
    if body.len() < BDC_HEADER {
        ctx.log_fmt(format_args!(
            "wifi-driver: a frame on the event channel is only {} bytes, shorter than its {}-byte BDC header",
            body.len(),
            BDC_HEADER
        ));
        return None;
    }
    let pad = (body[BDC_DATA_OFFSET] as usize) << 2;
    let eth = BDC_HEADER + pad;
    if which <= 2 {
        // Visible rather than asserted, for the first couple of frames only.
        ctx.log_fmt(format_args!(
            "wifi-driver:     BDC header: flags {:#04x} priority {} flags2 {:#04x} data_offset {} \
             (+{} bytes) -> ethernet frame at +{}",
            body[0], body[1], body[2], body[BDC_DATA_OFFSET], pad, eth
        ));
    }
    if body.len() <= eth {
        ctx.log_fmt(format_args!(
            "wifi-driver: a frame on the event channel has {} bytes but its BDC header puts the ethernet \
             frame at +{}",
            body.len(),
            eth
        ));
        return None;
    }
    let frame = &body[eth..];

    if frame.len() < ev::PAYLOAD {
        ctx.log_fmt(format_args!(
            "wifi-driver: a frame arrived on the event channel but is only {} bytes, shorter than the {} \
             bytes of headers an event has - not parsed",
            frame.len(),
            ev::PAYLOAD
        ));
        return None;
    }
    let ethertype = u16::from_be_bytes([frame[ev::ETHERTYPE], frame[ev::ETHERTYPE + 1]]);
    if ethertype != ETHERTYPE_LINK_CTL {
        ctx.log_fmt(format_args!(
            "wifi-driver: a frame arrived on the event channel with ethertype {:#06x}, not the {:#06x} an \
             event carries - so this is traffic, not an event",
            ethertype, ETHERTYPE_LINK_CTL
        ));
        return None;
    }
    // BIG-ENDIAN. See `be32`: the header is network byte order, the body it carries is not.
    let event_type = be32(frame, ev::EVENT_TYPE);
    let status = be32(frame, ev::STATUS);
    let datalen = be32(frame, ev::DATALEN) as usize;
    let avail = frame.len() - ev::PAYLOAD;
    if datalen > avail {
        ctx.log_fmt(format_args!(
            "wifi-driver: the event header parsed but claims a {} byte payload where only {} bytes follow \
             it. THE OFFSETS IN `ev` ARE WRONG - this is the self-check firing, not a short frame \
             (event_type read as {}, docs/wifi.md 30.2 has the arithmetic)",
            datalen, avail, event_type
        ));
        return None;
    }
    // The payload offset is reported relative to the BODY the caller holds, not to the ethernet frame, so
    // the BDC skip is included rather than left for the caller to remember.
    Some((event_type, status, eth + ev::PAYLOAD, datalen))
}

/// Pull the networks out of one escan-result payload.
fn parse_results(payload: &[u8], scan: &mut Scan, ctx: &ServiceContext) {
    if payload.len() < res::FIRST {
        ctx.log("wifi-driver: an escan-result event carried no result header");
        return;
    }
    let buflen = le32(payload, res::BUFLEN) as usize;
    let bss_count = le16(payload, res::BSS_COUNT) as usize;
    if buflen > payload.len() {
        ctx.log_fmt(format_args!(
            "wifi-driver: the escan result claims {} bytes but the event carried {} - not parsed",
            buflen, payload.len()
        ));
        return;
    }
    let mut at = res::FIRST;
    for i in 0..bss_count {
        if at + bss::MIN > payload.len() {
            ctx.log_fmt(format_args!(
                "wifi-driver: the result said {} network(s) but entry {} runs past the {} bytes that \
                 arrived - keeping the {} parsed so far",
                bss_count, i, payload.len(), scan.count
            ));
            return;
        }
        // STEP BY THE FIRMWARE'S OWN `length`, never by a compiled-in size: the struct is versioned.
        let entry_len = le32(payload, at + bss::LENGTH) as usize;
        let mut n = Network::blank();
        n.bssid.copy_from_slice(&payload[at + bss::BSSID..at + bss::BSSID + 6]);
        let sl = core::cmp::min(payload[at + bss::SSID_LEN] as usize, MAX_SSID);
        if at + bss::SSID + sl <= payload.len() {
            n.ssid[..sl].copy_from_slice(&payload[at + bss::SSID..at + bss::SSID + sl]);
            n.ssid_len = sl as u8;
        }
        n.chanspec = le16(payload, at + bss::CHANSPEC);
        n.rssi = le16(payload, at + bss::RSSI) as i16;
        scan.keep(n);

        if entry_len < bss::MIN || entry_len > payload.len() - at {
            ctx.log_fmt(format_args!(
                "wifi-driver: entry {} declares length {}, which is impossible (minimum {}, {} bytes left) \
                 - stopping here rather than walking off into the rest of the buffer",
                i, entry_len, bss::MIN, payload.len() - at
            ));
            return;
        }
        at += entry_len;
    }
}

/// Read event frames for up to `ms`, feeding anything that parses into `scan`.
///
/// **Bounded, and it says which bound ended it.** The truth being waited on is the firmware reporting the
/// scan complete; the deadline underneath is the bound §26.6 requires of every wait. Reporting which one
/// finished is the difference between a result and a guess.
pub fn collect(h: &Host, w: &mut Window, scan: &mut Scan, ms: u32, ctx: &ServiceContext) {
    let mut frame = [0u8; ctrl::FRAME];
    for _ in 0..ms {
        match ctrl::read_frame(h, w, &mut frame, ctx) {
            Some(f) => {
                let channel = f.chanflag & CHANNEL_MASK;
                if channel == CHANNEL_GLOM {
                    // Counted, not read. See `CHANNEL_GLOM` for why the reference drops these too.
                    scan.glom += 1;
                    continue;
                }
                if channel != CHANNEL_EVENT && channel != CHANNEL_DATA {
                    scan.other += 1;
                    continue;
                }
                scan.events += 1;
                let p = f.off;
                if let Some((event_type, status, at, datalen)) =
                    parse_event(&frame[p..p + f.len], scan.events, ctx)
                {
                    ctx.log_fmt(format_args!(
                        "wifi-driver:   event {} ({}), status {}, {} byte payload",
                        event_type,
                        code::name(event_type),
                        status,
                        datalen
                    ));
                    if event_type == code::ESCAN_RESULT {
                        scan.results += 1;
                        parse_results(&frame[p + at..p + at + datalen], scan, ctx);
                    }
                }
            }
            None => ctx.sleep_ms(1),
        }
    }
}

/// Run a scan and report what came back.
///
/// Returns false when nothing was heard at all, which is a different outcome from "no networks here" and is
/// reported as such.
pub fn run(h: &Host, w: &mut Window, ctx: &ServiceContext) -> bool {
    /// How long to listen. A scan genuinely takes time - the radio dwells on each channel - so this is a
    /// real duration rather than a stand-in for a condition (§30.5).
    const LISTEN_MS: u32 = 4000;

    ctx.log("wifi-driver: stage 14 - scanning. UNVERIFIED ON HARDWARE: designed at the desk from the \
             references, see docs/wifi.md 30");

    let mut request = [0u8; req::SIZE];
    build_request(&mut request);

    // THE INTERFACE MUST BE UP FIRST. A scan on a down interface is refused with `BCME_NOTUP` (-4), which
    // is exactly what this driver was told the first time it tried.
    let mut session = ctrl::Session::new();

    // THE CLM BLOB FIRST, and it is first for a reason rather than by habit. `bwfm_init` is preceded by
    // `bwfm_preinit`, whose brcmfmac twin `brcmf_c_preinit_dcmds` calls `brcmf_c_process_clm_blob`. CLM is
    // the Country Locale Matrix - which channels may be used at what power - and a radio with no regulatory
    // data cannot lawfully transmit or scan. That is why every configuration command was accepted and
    // `escan` still answered `BCME_NOTUP`.
    if !ctrl::download_blob(
        h,
        w,
        &mut session,
        "clmload",
        ctrl::DL_TYPE_CLM,
        crate::firmware::CLM,
        ctx,
    ) {
        ctx.log(
            "wifi-driver: the CLM regulatory blob was refused, so the radio has no channel rules and will \
             not come up - not attempting a scan",
        );
        return false;
    }

    // ASK FOR THE EVENTS NEXT. The firmware sends NONE until the host sets this mask, so a scan accepted
    // without it would run and report nothing - which is indistinguishable from an empty room.
    if !ctrl::enable_events(
        h,
        w,
        &mut session,
        &[code::ESCAN_RESULT, code::LINK, code::SET_SSID, code::ASSOC],
        ctx,
    ) {
        ctx.log(
            "wifi-driver: the event mask was not set, so a scan would produce no results even if accepted \
             - not attempting one",
        );
        return false;
    }

    // THE BRING-UP CHAIN LAST, which is the order `bwfm_init` uses: the event mask and the scan timings are
    // set BEFORE `BWFM_C_UP`, not after. Reading that function in full rather than asking for particular
    // lines is what showed it - a list of call sites came back in the order they were found, not the order
    // they run.
    //
    // Not sent, and each for a reason rather than for now (§26.2): `mpc`, `join_pref`, `txbf`, the three
    // scan timings and `SET_PM` are all tuning of something that does not work yet, and the scan timings
    // have firmware defaults this driver is content with.
    if !ctrl::interface_up(h, w, &mut session, ctx) {
        ctx.log("wifi-driver: the interface would not come up, so no scan is attempted");
        return false;
    }

    let mut scan = Scan::new();
    if !ctrl::set_iovar(h, w, &mut session, "escan", &request, ctx) {
        // NO VERSION HINT HERE. This message used to say the params VERSION was the first thing to change,
        // which was written for the "accepted but silent" case and is wrong for a refusal: the firmware
        // states what it objected to, and the decoded error is printed one line above. The version matters
        // only where the request is ACCEPTED and no results follow.
        ctx.log(
            "wifi-driver: the firmware refused the `escan` request, so no scan started. The decoded error \
             above says what it objected to",
        );
        return false;
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: `escan` accepted - listening for results for {} ms",
        LISTEN_MS
    ));
    collect(h, w, &mut scan, LISTEN_MS, ctx);

    ctx.log_fmt(format_args!(
        "wifi-driver: the scan window saw {} event/data frame(s), {} escan-result event(s), {} glommed \
         frame(s) ignored, {} on other channels",
        scan.events, scan.results, scan.glom, scan.other
    ));

    if scan.events == 0 {
        ctx.log(
            "wifi-driver: the scan was accepted and NO event frame arrived at all. That is the event path, \
             not the scan: rung A of docs/wifi.md 30.6 is what to prove next, and it needs no scan",
        );
        return false;
    }
    if scan.results == 0 {
        ctx.log_fmt(format_args!(
            "wifi-driver: {} event(s) arrived but none was an ESCAN_RESULT ({}). The request was ACCEPTED \
             and produced no results, which is the case where the params VERSION is the first thing to \
             change - v0 is what this tried (docs/wifi.md 30.4)",
            scan.events,
            code::ESCAN_RESULT
        ));
        return false;
    }
    if scan.count == 0 {
        ctx.log_fmt(format_args!(
            "wifi-driver: {} escan-result event(s) arrived and no network parsed out of them - so the \
             result offsets are what to check, docs/wifi.md 30.2",
            scan.results
        ));
        return false;
    }

    ctx.log_fmt(format_args!(
        "wifi-driver: {} network(s) from {} escan-result event(s){}",
        scan.count,
        scan.results,
        if scan.dropped > 0 { " (and more than this build keeps)" } else { "" }
    ));
    for n in scan.networks() {
        // The SSID is whatever bytes the AP beacons; it is NOT trusted to be text, so it is rendered one
        // byte at a time with anything unprintable shown as a dot.
        let mut shown = [b'.'; MAX_SSID];
        let len = n.ssid_len as usize;
        for i in 0..len {
            let c = n.ssid[i];
            shown[i] = if (0x20..0x7F).contains(&c) { c } else { b'.' };
        }
        let name = core::str::from_utf8(&shown[..len]).unwrap_or("(unprintable)");
        ctx.log_fmt(format_args!(
            "wifi-driver:   {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  {:>4} dBm  chanspec {:#06x}  {}",
            n.bssid[0], n.bssid[1], n.bssid[2], n.bssid[3], n.bssid[4], n.bssid[5],
            n.rssi, n.chanspec,
            if len == 0 { "(hidden)" } else { name }
        ));
    }
    if scan.dropped > 0 {
        ctx.log_fmt(format_args!(
            "wifi-driver: {} further network(s) were reported and NOT kept - this build holds {}. Counted \
             rather than dropped quietly, because a scan that silently truncates looks like a quiet room",
            scan.dropped, MAX_RESULTS
        ));
    }
    true
}
