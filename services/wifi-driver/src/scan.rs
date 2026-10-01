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
pub(crate) mod ev {
    /// `ether_header` is 14 bytes; the ethertype is its last field.
    pub const ETHERTYPE: usize = 12;
    /// `bwfm_ethhdr` follows the ethernet header.
    pub const ETHHDR: usize = 14;
    /// `bwfm_event_msg` follows it - 2+2+1+3+2 = 10 bytes packed.
    pub const MSG: usize = ETHHDR + 10;
    /// `event_type`, 4 bytes into the message.
    pub const EVENT_TYPE: usize = MSG + 4;
    /// `flags`, `__be16`, 2 bytes into the message.
    pub const FLAGS: usize = MSG + 2;
    /// `status`.
    pub const STATUS: usize = MSG + 8;
    /// `reason`.
    pub const REASON: usize = MSG + 12;
    /// `datalen`.
    pub const DATALEN: usize = MSG + 20;
    /// The event payload - 2+2+4+4+4+4+4+6+16+1+1 = 48 bytes of message.
    pub const PAYLOAD: usize = MSG + 48;
}

/// Event codes, quoted from `bwfmreg.h`.
pub(crate) mod code {
    /// `BWFM_E_SET_SSID`.
    pub const SET_SSID: u32 = 0;
    /// `BWFM_E_ASSOC`.
    pub const ASSOC: u32 = 7;
    /// `BWFM_E_LINK`.
    pub const LINK: u32 = 16;
    /// `BWFM_E_ESCAN_RESULT`.
    pub const ESCAN_RESULT: u32 = 69;
    /// `BWFM_E_JOIN`.
    pub const JOIN: u32 = 1;
    /// `BWFM_E_AUTH`.
    pub const AUTH: u32 = 3;
    /// `BWFM_E_DEAUTH`.
    pub const DEAUTH: u32 = 5;
    /// `BWFM_E_DEAUTH_IND` - the access point deauthenticated us.
    pub const DEAUTH_IND: u32 = 6;
    /// `BWFM_E_DISASSOC`.
    pub const DISASSOC: u32 = 11;
    /// `BWFM_E_DISASSOC_IND`.
    pub const DISASSOC_IND: u32 = 12;
    /// `BWFM_E_PSK_SUP` - the firmware's own supplicant reporting on the 4-way handshake.
    pub const PSK_SUP: u32 = 46;

    /// The `Stats::events` bucket for a code: the nine this driver names in order, then "other".
    pub fn index(c: u32) -> usize {
        match c {
            SET_SSID => 0,
            JOIN => 1,
            AUTH => 2,
            DEAUTH_IND => 3,
            ASSOC => 4,
            DISASSOC_IND => 5,
            LINK => 6,
            PSK_SUP => 7,
            ESCAN_RESULT => 8,
            _ => 9,
        }
    }
    /// The code a bucket stands for (`index` inverted), for the shell's `wifi debug events`.
    pub const BUCKETS: [u32; 9] = [SET_SSID, JOIN, AUTH, DEAUTH_IND, ASSOC, DISASSOC_IND, LINK, PSK_SUP, ESCAN_RESULT];

    /// A name for the log, so an unexpected event is legible rather than a bare number.
    pub fn name(c: u32) -> &'static str {
        match c {
            SET_SSID => "SET_SSID",
            JOIN => "JOIN",
            AUTH => "AUTH",
            DEAUTH => "DEAUTH",
            DEAUTH_IND => "DEAUTH_IND",
            ASSOC => "ASSOC",
            DISASSOC => "DISASSOC",
            DISASSOC_IND => "DISASSOC_IND",
            LINK => "LINK",
            PSK_SUP => "PSK_SUP",
            ESCAN_RESULT => "ESCAN_RESULT",
            _ => "(not an event this driver names)",
        }
    }
}

/// Event status values, quoted from `bwfmreg.h`. An `ESCAN_RESULT` carrying `PARTIAL` is a batch of networks;
/// one carrying `SUCCESS` says the scan is over. Both were observed on hardware in exactly that order.
pub(crate) mod status {
    /// `BWFM_E_STATUS_SUCCESS`.
    pub const SUCCESS: u32 = 0;
    /// `BWFM_E_STATUS_FAIL`.
    pub const FAIL: u32 = 1;
    /// `BWFM_E_STATUS_TIMEOUT`.
    pub const TIMEOUT: u32 = 2;
    /// `BWFM_E_STATUS_NO_NETWORKS`.
    pub const NO_NETWORKS: u32 = 3;
    /// `BWFM_E_STATUS_ABORT`.
    pub const ABORT: u32 = 4;
    /// `BWFM_E_STATUS_PARTIAL`.
    pub const PARTIAL: u32 = 8;

    /// Is this status the END of a scan, and what does it say?
    pub fn terminal(s: u32) -> Option<&'static str> {
        match s {
            SUCCESS => Some("complete"),
            FAIL => Some("FAILED"),
            TIMEOUT => Some("TIMED OUT in the firmware"),
            NO_NETWORKS => Some("complete - no networks"),
            ABORT => Some("ABORTED"),
            _ => None,
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
    /// `capability`, `__le16`: the 802.11 capability information field. Bit 0x0010 is Privacy.
    pub const CAPABILITY: usize = 16;
    pub const CHANSPEC: usize = 72;
    pub const RSSI: usize = 78;
    /// `ie_offset`, `__le16`: where the beacon's information elements start, FROM THE START OF THIS RECORD.
    /// The reference copies `((uint8_t *)bss) + iesoff` for `ieslen` bytes, and that is the base used here.
    pub const IE_OFFSET: usize = 116;
    /// `ie_length`, `__le32`.
    pub const IE_LENGTH: usize = 120;
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
/// `WLC_SCAN` / `BRCMF_C_SCAN` - the plain scan command. Not used to scan: it is how an escan is ABORTED.
///
/// `brcmf_notify_escan_complete(.., fw_abort = true)`:
///
/// ```c
/// /* E-Scan (or anyother type) can be aborted by SCAN */
/// brcmf_escan_prep(cfg, &params_v2_le, NULL);   /* channel_num = 1, channel_list[0] = -1 */
/// err = brcmf_fil_cmd_data_set(ifp, BRCMF_C_SCAN, &params_le, sizeof(params_le));
/// ```
///
/// A scan of one channel numbered -1 is not a scan; the firmware reads it as "stop". The params are the
/// escan request's own, without the 8-byte escan header and with the one-entry channel list appended.
const CMD_SCAN: u32 = 50;
/// Where the scan params begin inside the escan request: after `version` (4), `action` (2), `sync_id` (2).
const SCAN_PARAMS_AT: usize = 8;
/// `sizeof(struct brcmf_scan_params_le)` with its one-entry channel list: 64 bytes of params plus a u16.
const ABORT_SIZE: usize = req::SIZE - SCAN_PARAMS_AT + 2;
/// `BWFM_SCANTYPE_ACTIVE`.
const SCANTYPE_ACTIVE: u8 = 0;
/// `DOT11_BSSTYPE_ANY`.
const BSSTYPE_ANY: u8 = 2;
/// The `sync_id` the reference uses, echoed back in every result so a stale scan's events are recognisable.
const SYNC_ID: u16 = 0x1234;

/// `BWFM_MAX_SSID_LEN` / `IEEE80211_MAX_SSID_LEN`. The ONE home for this fact; `join` re-exports it.
pub const MAX_SSID: usize = 32;
/// How many networks to keep. Bounded on purpose: a fixed array whose limit is readable here (§26.6.1).
const MAX_RESULTS: usize = 32;

/// What a network's beacon says about how it is secured. Wire value in a `wifi list` record's last byte.
pub mod sec {
    /// No Privacy bit, no RSN element, no WPA element.
    pub const OPEN: u8 = 0;
    /// Privacy bit set and neither WPA nor RSN element - which leaves WEP.
    pub const WEP: u8 = 1;
    /// A WPA vendor element (Microsoft OUI, type 1) and no RSN element.
    pub const WPA: u8 = 2;
    /// An RSN element (WPA2).
    pub const WPA2: u8 = 3;
    /// Both elements: a mixed-mode network.
    pub const WPA2_WPA: u8 = 4;
}

/// One network, as much of it as this driver reports.
#[derive(Clone, Copy)]
pub struct Network {
    pub bssid: [u8; 6],
    pub ssid: [u8; MAX_SSID],
    pub ssid_len: u8,
    pub chanspec: u16,
    pub rssi: i16,
    /// A `sec::` value.
    pub security: u8,
}

impl Network {
    fn blank() -> Self {
        Network { bssid: [0; 6], ssid: [0; MAX_SSID], ssid_len: 0, chanspec: 0, rssi: 0, security: sec::OPEN }
    }
}

/// `IEEE80211_ELEMID_RSN`.
const ELEMID_RSN: u8 = 48;
/// `IEEE80211_ELEMID_VENDOR`.
const ELEMID_VENDOR: u8 = 221;
/// `MICROSOFT_OUI` - `{ 0x00, 0x50, 0xf2 }` - followed by the type byte `1` for a WPA element:
/// `if (memcmp(frm + 2, MICROSOFT_OUI, 3) == 0) { if (frm[5] == 1) wpaie = frm;`
const MICROSOFT_OUI: [u8; 3] = [0x00, 0x50, 0xf2];
const WPA_TYPE: u8 = 1;
/// `IEEE80211_CAPINFO_PRIVACY`.
const CAPINFO_PRIVACY: u16 = 0x0010;

/// Classify a network from its beacon's information elements and capability field.
///
/// The elements are a walk of `[id][len][body]` records, quoted from the reference exactly as net80211
/// does it: an RSN element is WPA2; a vendor element whose body opens with the Microsoft OUI and type 1 is
/// WPA; the Privacy capability bit with neither element is WEP; nothing is open. A walk that runs off the
/// end stops rather than reads past it - the length bytes come from the air and are not trusted.
fn classify(ies: &[u8], capability: u16) -> u8 {
    let mut rsn = false;
    let mut wpa = false;
    let mut at = 0usize;
    while at + 2 <= ies.len() {
        let id = ies[at];
        let len = ies[at + 1] as usize;
        let body_end = at + 2 + len;
        if body_end > ies.len() {
            break;
        }
        let body = &ies[at + 2..body_end];
        if id == ELEMID_RSN {
            rsn = true;
        } else if id == ELEMID_VENDOR && body.len() >= 4 && body[..3] == MICROSOFT_OUI && body[3] == WPA_TYPE {
            wpa = true;
        }
        at = body_end;
    }
    match (rsn, wpa) {
        (true, true) => sec::WPA2_WPA,
        (true, false) => sec::WPA2,
        (false, true) => sec::WPA,
        (false, false) => {
            if capability & CAPINFO_PRIVACY != 0 {
                sec::WEP
            } else {
                sec::OPEN
            }
        }
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

    /// How many networks are held so far - "heard so far" while a sweep runs, the total once it ends.
    pub fn count(&self) -> usize {
        self.count
    }

    /// The network of this name, if the last sweep heard one. Two access points with the same name are one
    /// network; the first heard is returned, and its security byte is what a join without a passphrase is
    /// decided on.
    pub fn find(&self, ssid: &[u8]) -> Option<&Network> {
        self.networks()
            .iter()
            .find(|n| n.ssid_len as usize == ssid.len() && &n.ssid[..ssid.len()] == ssid)
    }
}

/// Reply status bytes for a `wifi list` request. One byte, first in the reply, read by the shell.
pub mod reply {
    /// Networks follow.
    pub const OK: u8 = 0;
    /// The scan ran and failed; the driver's log says where.
    pub const SCAN_FAILED: u8 = 1;
    /// The radio is not up (at boot or after a respawn), so there is nothing to scan with; byte 1 says why.
    pub const RADIO_DOWN: u8 = 2;
    /// Byte 1 of a `RADIO_DOWN` answer: WHY the radio is down, so the shell can say it rather than guess.
    /// 0 means a driver too old to say.
    ///
    /// `DOWN_TRAPPED`: the firmware was loaded and trapped at start (`SDPCM_SHARED_TRAP`).
    pub const DOWN_TRAPPED: u8 = 1;
    /// The bring-up stopped at a stage other than the firmware's start; the serial log names the stage.
    pub const DOWN_BRINGUP: u8 = 2;
    /// No working radio answered on this driver's bus.
    pub const DOWN_NO_RADIO: u8 = 3;
    /// Byte 3 of an `off` / `off hard` answer: the driver checked, and the radio IS off.
    pub const OFF_VERIFIED: u8 = 1;
    /// The check could not be made (the firmware did not answer the question); the off was not confirmed.
    pub const OFF_UNVERIFIED: u8 = 2;
    /// The check CONTRADICTS the off: the firmware still says it is up, or the chip still answers its bus.
    pub const OFF_CONTRADICTED: u8 = 3;
    /// Not a request this driver understands.
    pub const UNKNOWN_OP: u8 = 3;

    /// Bytes per network record: bssid[6] rssi(i16 LE) chanspec(u16 LE) ssid_len ssid[32] security note.
    pub const RECORD: usize = 45;
    /// The record's NOTE byte: bit 0 - a key for this name is held; bit 1 - this is the network joined.
    pub const NOTE_SAVED: u8 = 1;
    pub const NOTE_JOINED: u8 = 2;
    /// Request op byte: scan and list.
    pub const OP_LIST: u8 = 1;
    /// Request op byte: join a network. Payload: `ssid_len, ssid[32], pass_len, pass[64]`. A `pass_len`
    /// of 0 means "with what you have": the held key for that name, or open if the last sweep heard the
    /// network as open, else `NEEDS_PASSPHRASE`.
    pub const OP_CONNECT: u8 = 2;
    /// Request op byte: start a sweep and return at once. Reply `[OK, 0]`, or `[SCANNING, heard]` when one
    /// is already running (the caller attaches to it), or `[SCAN_FAILED]` / `[RADIO_DOWN]`.
    pub const OP_SCAN_START: u8 = 3;
    /// Request op byte: `[4, from]` - the records heard so far from index `from`. Reply
    /// `[SCANNING | SCAN_DONE, total, records from..total]`, `[SCAN_FAILED]` if the last sweep died by the
    /// poll bound, `[NO_SCAN_YET]` if nothing has ever been swept.
    pub const OP_SCAN_POLL: u8 = 4;
    /// Request op byte: stop the sweep. The partial hearing is DISCARDED - the cache keeps the last complete
    /// scan. Reply `[OK, heard]`.
    pub const OP_SCAN_ABORT: u8 = 5;
    /// Request op byte: what the radio is doing. Reply `[OK, sweeping(0|1), heard, has_cache(0|1),
    /// cache_count, age_secs u32 LE, radio_on(0|1), joined_len, joined_ssid[32]]`.
    pub const OP_STATUS: u8 = 6;
    /// Request op byte: leave the current network; the radio stays up. Reply `[OK, was_joined(0|1)]`.
    pub const OP_DISCONNECT: u8 = 7;
    /// Request op byte: `[8, mode]` - power the radio. `mode` 0 = off (disconnects first), 1 = on (rejoins
    /// the network last joined), `RADIO_POWERCYCLE` = cut and restore the CHIP's power through the kernel's
    /// `DevicePower` and leave this instance to be killed and respawned onto the cold chip. Reply
    /// `[status, was_joined, changed, rejoin_status, len, name...]`.
    pub const OP_RADIO: u8 = 8;
    /// The third `OP_RADIO` mode: `wifi radio powercycle`. Not a radio switch at all - the chip's power.
    /// `[8, 2, units]`: `units` of 100 ms to hold the power off, 0 for the driver's default.
    pub const RADIO_POWERCYCLE: u8 = 2;
    /// `OP_RADIO` mode 3: `wifi radio off hard` - cut the chip's power and stay powered down. `on` or
    /// `powercycle` restores the power and answers `COLD_START`.
    pub const RADIO_HARD_OFF: u8 = 3;
    /// Reply status while the chip is powered down: every op except status and the radio op gets this one
    /// byte. `wifi radio on` powers the chip up.
    pub const RADIO_POWERED_OFF: u8 = 18;
    /// `OP_RADIO` reply byte 3 after `on` on a powered-down chip: the power is back and this instance has
    /// no firmware to serve, so the caller restarts the driver and the respawn takes the boot's cold path.
    pub const COLD_START: u8 = 19;
    /// Reply status when the KERNEL refused to drive the device's power - this machine has no control
    /// over it. Distinct from `RADIO_DOWN`, which says the radio is down and nothing about power; the two
    /// were one byte on 2026-10-01 and a shell read a down radio as a powerless machine.
    pub const NO_POWER_CONTROL: u8 = 20;
    /// The radio was powered off by `wifi radio off`; a sweep or a join is refused until `radio on`. Distinct
    /// from `RADIO_DOWN`, which is a radio that never came up.
    pub const RADIO_OFF: u8 = 7;
    /// Request op byte: which networks a key is held for. Reply `[OK, count, (len, ssid[32]) * count]` - names
    /// only, never a key (`utilities/56_wifi.md` §3); 64 slots at most.
    pub const OP_STORED: u8 = 9;
    /// Request op byte: `[10, len, ssid[32]]` - drop the held key for that network. Reply `[OK, dropped(0|1)]`.
    pub const OP_FORGET: u8 = 10;
    /// `OP_CONNECT` with no passphrase, for a network that is neither open (by the cache) nor stored: the
    /// shell must ask for one and send again. Never a guess about which it is.
    pub const NEEDS_PASSPHRASE: u8 = 16;
    /// `OP_CONNECT` for the network the radio is already on, checked live (`GET_BSSID`), not from memory.
    /// Nothing is sent to the firmware.
    pub const ALREADY_JOINED: u8 = 17;
    /// Request op byte: `[11, sub]` - the driver's own account of itself, for `wifi debug` (`dbg::*`).
    pub const OP_DEBUG: u8 = 11;

    /// Sub-codes of `OP_DEBUG`, and their reply layouts.
    pub mod dbg {
        /// `[OK, 30 x u32 LE]`: ctrl sent/accepted/refused/unanswered, rx ctrl/event/data/glom/header-only/
        /// other, tx_bytes, rx_bytes, rx skipped in a control wait, the 10 event buckets, last_event_code,
        /// last_event_status, last_refused_cmd, last_refused_status (i32), session ms, frames ever traced,
        /// sub-frames delivered out of superframes.
        /// `wifi debug`, `wifi debug stats`, `wifi debug events` and `wifi debug transport` all read this;
        /// they print different rows of it.
        pub const STATS: u8 = 0;
        /// `[OK, count u8, entries x 18 bytes]` - the trace ring, oldest first. Entry: ms u32, kind u8,
        /// chanflag u8, id u16, what u32, status i32, len u16.
        pub const TRACE: u8 = 1;
        /// `[OK, ver_len u8, ver[128], cap_len u16 LE, cap[512], mac[6]]` - asked of the firmware now.
        pub const FIRMWARE: u8 = 2;
    }

    /// A sweep is running. For `OP_LIST` this is a REFUSAL: the cache is not served while it is about to be
    /// replaced (`utilities/56_wifi.md` §3, Commandment III). Byte 1 is the count heard so far.
    pub const SCANNING: u8 = 4;
    /// Nothing has been swept since the driver started, so there is no list to give - an error, not an
    /// empty room.
    pub const NO_SCAN_YET: u8 = 5;
    /// `OP_SCAN_POLL` only: the sweep ended and these are its records.
    pub const SCAN_DONE: u8 = 6;

    // CONNECT statuses start at 10 so they never share a byte with the list statuses above:
    // RADIO_DOWN (2) is answered to BOTH ops and must mean one thing.
    /// Reply status for `OP_CONNECT`: associated and the handshake completed.
    pub const JOINED: u8 = 10;
    /// No network of that name answered the join.
    pub const NOT_FOUND: u8 = 11;
    /// The network refused the passphrase - the handshake timed out or the AP deauthenticated us.
    pub const PASSPHRASE_REFUSED: u8 = 12;
    /// A command in the join sequence was refused; the driver's log names it.
    pub const JOIN_FAILED: u8 = 13;
    /// Nothing decisive arrived within the bound.
    pub const JOIN_TIMEOUT: u8 = 14;
    /// Kept for the table: the reply the join gave while the host handshake was unbuilt (2026-09-29, before
    /// the evening). No path produces it now; a shell reading it names the state it stood for.
    pub const HANDSHAKE_UNIMPLEMENTED: u8 = 15;
}

/// Serialise a scan into a reply: `[status, count, record * count]`. Returns the bytes written.
///
/// A FIXED layout with no framing to parse on the far side - the shell indexes into it. 32 records of 44
/// bytes plus two is 1410 bytes, well inside a 4096-byte message, and `Scan` already bounds the count.
pub fn write_reply(scan: &Scan, note: &dyn Fn(&Network) -> u8, out: &mut [u8]) -> usize {
    write_records(scan, 0, reply::OK, note, out)
}

/// The same reply from record `from` onward, under a chosen status byte. Byte 1 is always the TOTAL held,
/// so a poller knows both how many it has been given and how many exist: a `wifi scan` surface asks for
/// what it has not yet printed, and a `from` past the end yields the two status bytes alone.
///
/// `note` is the serve loop's knowledge of each network - a key held for it, the one joined - which the scan
/// itself cannot know; it is asked per record so this stays a serialiser and the loop stays the owner.
pub fn write_records(scan: &Scan, from: usize, status: u8, note: &dyn Fn(&Network) -> u8, out: &mut [u8]) -> usize {
    out[0] = status;
    out[1] = scan.count as u8;
    let mut at = 2;
    for n in scan.networks().iter().skip(from) {
        if at + reply::RECORD > out.len() {
            break;
        }
        out[at..at + 6].copy_from_slice(&n.bssid);
        out[at + 6..at + 8].copy_from_slice(&n.rssi.to_le_bytes());
        out[at + 8..at + 10].copy_from_slice(&n.chanspec.to_le_bytes());
        out[at + 10] = n.ssid_len;
        out[at + 11..at + 11 + MAX_SSID].copy_from_slice(&n.ssid);
        // The last byte was a pad; it carries the `sec::` value now. Same record size, same offsets.
        out[at + 43] = n.security;
        out[at + 44] = note(n);
        at += reply::RECORD;
    }
    at
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
/// One decoded event: the header fields the driver acts on, and where its payload sits in the body.
pub(crate) struct Event {
    pub event_type: u32,
    pub status: u32,
    pub reason: u32,
    /// `flags`: for `LINK`, bit 0x01 is link up (`BRCMF_EVENT_MSG_LINK`).
    pub flags: u16,
    /// Payload offset within the body handed in, BDC header included.
    pub at: usize,
    pub datalen: usize,
}

/// Where the ethernet frame starts inside a DATA or EVENT body: past the 4-byte BDC header and then
/// `data_offset << 2` more (`bwfm_proto_bcdc_rx`: `m_adj(m, sizeof(*hdr) + (hdr->data_offset << 2))`).
///
/// One place for the arithmetic, because the event path and the traffic path both need it and two copies
/// is how one of them drifts. `None` when the body cannot hold what its own header claims; the caller says
/// so in its own words, since "event" and "traffic" are different sentences.
pub(crate) fn ethernet_at(body: &[u8]) -> Option<usize> {
    if body.len() < BDC_HEADER {
        return None;
    }
    let eth = BDC_HEADER + ((body[BDC_DATA_OFFSET] as usize) << 2);
    if body.len() <= eth {
        None
    } else {
        Some(eth)
    }
}

/// The event code and status out of a body on the event channel, with none of `parse_event`'s reporting -
/// for the trace, which must never log. `None` for anything that is not an event frame.
pub(crate) fn event_head(body: &[u8]) -> Option<(u32, u32)> {
    let eth = ethernet_at(body)?;
    let frame = &body[eth..];
    if frame.len() < ev::PAYLOAD {
        return None;
    }
    if u16::from_be_bytes([frame[ev::ETHERTYPE], frame[ev::ETHERTYPE + 1]]) != ETHERTYPE_LINK_CTL {
        return None;
    }
    Some((be32(frame, ev::EVENT_TYPE), be32(frame, ev::STATUS)))
}

pub(crate) fn parse_event(body: &[u8], which: u32, ctx: &ServiceContext) -> Option<Event> {
    // THE BDC HEADER FIRST. A data or event frame is not an ethernet frame: it carries four bytes of BDC
    // header and then `data_offset << 2` more before the ethernet header starts.
    let eth = match ethernet_at(body) {
        Some(eth) => eth,
        None => {
            ctx.log_fmt(format_args!(
                "wifi-driver: a frame on the event channel is {} bytes, which cannot hold its {}-byte BDC \
                 header plus the {} words of offset it declares",
                body.len(),
                BDC_HEADER,
                body.get(BDC_DATA_OFFSET).copied().unwrap_or(0)
            ));
            return None;
        }
    };
    if which <= 2 {
        // Visible rather than asserted, for the first couple of frames only.
        ctx.log_fmt(format_args!(
            "wifi-driver:     BDC header: flags {:#04x} priority {} flags2 {:#04x} data_offset {} \
             -> ethernet frame at +{}",
            body[0], body[1], body[2], body[BDC_DATA_OFFSET], eth
        ));
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
    let reason = be32(frame, ev::REASON);
    let flags = u16::from_be_bytes([frame[ev::FLAGS], frame[ev::FLAGS + 1]]);
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
    Some(Event { event_type, status, reason, flags, at: eth + ev::PAYLOAD, datalen })
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
        // The information elements live INSIDE this record, at `ie_offset` from its start. Bounded by the
        // record's own declared length and by the buffer, since both numbers came from the air.
        if at + bss::IE_LENGTH + 4 <= payload.len() {
            let capability = le16(payload, at + bss::CAPABILITY);
            let ie_off = le16(payload, at + bss::IE_OFFSET) as usize;
            let ie_len = le32(payload, at + bss::IE_LENGTH) as usize;
            let entry_len = le32(payload, at + bss::LENGTH) as usize;
            let start = at + ie_off;
            let end = start.saturating_add(ie_len);
            let limit = core::cmp::min(payload.len(), at.saturating_add(entry_len));
            n.security = if ie_off >= bss::MIN && end <= limit {
                classify(&payload[start..end], capability)
            } else {
                // Elements the record does not actually contain are not parsed; the Privacy bit alone
                // still says whether the network is encrypted at all.
                if capability & CAPINFO_PRIVACY != 0 { sec::WEP } else { sec::OPEN }
            };
        }
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
/// What one turn of the sweep did.
pub enum Step {
    /// A frame arrived and was accounted for (read, counted or skipped - all three are progress).
    Frame,
    /// Nothing arrived. The caller decides how many of these it will tolerate; this does not sleep.
    Empty,
    /// The firmware said the sweep is over, and why.
    Ended(&'static str),
}

/// Advance a running sweep by ONE frame. This is the body `collect` used to loop over, split out so the
/// serve loop can do the same one frame at a time and still answer requests between frames - which is what
/// lets `wifi list`, `wifi status` and an abort be heard mid-sweep (rule 11), and a background sweep exist
/// at all. `frame` is the caller's buffer, so the serve loop does not put 2 KiB on its stack per turn.
pub fn step(
    h: &Host,
    w: &mut Window,
    s: &mut ctrl::Session,
    scan: &mut Scan,
    frame: &mut [u8; ctrl::FRAME],
    ctx: &ServiceContext,
) -> Step {
    let f = match ctrl::read_frame(h, w, frame, ctx) {
        Some(f) => f,
        None => return Step::Empty,
    };
    s.note_frame(ctx, &f, frame, false);
    if f.chanflag & CHANNEL_MASK == CHANNEL_GLOM {
        // A superframe is READ now, its sub-frames handled below like any other frame; the descriptor that
        // precedes it yields nothing. Counted either way, so `glom` still says how many arrived.
        scan.glom += 1;
    }
    // EVERY FRAME INSIDE THE FRAME. A plain frame is one; a superframe is each of its sub-frames - which is
    // where the association events turned out to travel (docs/wifi.md 38).
    let mut subs = [ctrl::Sub::default(); ctrl::MAX_SUBS];
    let n = ctrl::subframes(&f, frame, s.glom_descriptor(), &mut subs, ctx);
    for sub in subs.iter().take(n) {
        let channel = sub.chanflag & CHANNEL_MASK;
        if channel != CHANNEL_EVENT && channel != CHANNEL_DATA {
            scan.other += 1;
            continue;
        }
        scan.events += 1;
        let p = sub.off;
        if let Some(e) = parse_event(&frame[p..p + sub.len], scan.events, ctx) {
            let (event_type, status, at, datalen) = (e.event_type, e.status, e.at, e.datalen);
            ctx.log_fmt(format_args!(
                "wifi-driver:   event {} ({}), status {}, {} byte payload{}",
                event_type,
                code::name(event_type),
                status,
                datalen,
                if sub.glommed { " (glommed)" } else { "" }
            ));
            if event_type == code::ESCAN_RESULT {
                scan.results += 1;
                if status == status::PARTIAL {
                    parse_results(&frame[p + at..p + at + datalen], scan, ctx);
                }
                if let Some(why) = status::terminal(status) {
                    // THE FIRMWARE SAID SO. Stop asking.
                    return Step::Ended(why);
                }
            }
        }
    }
    Step::Frame
}

/// Ask the firmware to begin a sweep. True when `escan` was accepted; the results then arrive as events,
/// which `step` reads. The decoded refusal, if any, is logged by `set_iovar` one line above this one's.
pub fn start(h: &Host, w: &mut Window, s: &mut ctrl::Session, ctx: &ServiceContext) -> bool {
    let mut request = [0u8; req::SIZE];
    build_request(&mut request);
    if !ctrl::set_iovar(h, w, s, "escan", &request, ctx) {
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
    ctx.log("wifi-driver: `escan` accepted - listening until the firmware says the scan is over");
    true
}

/// Stop a running sweep, the way Linux does (`CMD_SCAN`): the escan params with `channel_num` 1 and a single
/// channel of -1. True when the firmware accepted the command. The results heard so far are the caller's
/// to discard - and it does, because a half-heard room is not the room.
pub fn abort(h: &Host, w: &mut Window, s: &mut ctrl::Session, ctx: &ServiceContext) -> bool {
    let mut full = [0u8; req::SIZE];
    build_request(&mut full);
    let mut params = [0u8; ABORT_SIZE];
    params[..req::SIZE - SCAN_PARAMS_AT].copy_from_slice(&full[SCAN_PARAMS_AT..]);
    let channel_num = req::CHANNEL_NUM - SCAN_PARAMS_AT;
    params[channel_num..channel_num + 4].copy_from_slice(&1u32.to_le_bytes());
    params[channel_num + 4..channel_num + 6].copy_from_slice(&(-1i16 as u16).to_le_bytes());
    ctrl::set_cmd(h, w, s, CMD_SCAN, &params, "scan abort (a one-channel scan of channel -1)", ctx)
}

pub fn collect(
    h: &Host,
    w: &mut Window,
    s: &mut ctrl::Session,
    scan: &mut Scan,
    max_empty_polls: u32,
    ctx: &ServiceContext,
) {
    let mut frame = [0u8; ctrl::FRAME];
    // THE TRUTH ENDS THE WAIT; THE COUNT ONLY BOUNDS IT. The firmware announces a finished scan with an
    // ESCAN_RESULT carrying SUCCESS - observed on hardware after twelve PARTIAL batches - and the loop used
    // to ignore that and run to a fixed iteration count instead. That count was named `ms` and was not
    // milliseconds: every empty poll is a CMD53 of tens of microseconds plus a sleep, so "4000 ms" ran for
    // about two minutes and the network list printed after the operator stopped watching. A count is not a
    // duration; it is a bound, and it is named as one now.
    let mut empty = 0u32;
    let mut ended_by = "the poll bound - the firmware never said the scan was over";
    while empty < max_empty_polls {
        match step(h, w, s, scan, &mut frame, ctx) {
            Step::Frame => {}
            Step::Empty => {
                empty += 1;
                ctx.sleep_ms(1);
            }
            Step::Ended(why) => {
                ended_by = why;
                break;
            }
        }
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: listening ended: {} ({} empty poll(s) of a {} bound)",
        ended_by, empty, max_empty_polls
    ));
}

/// Run a scan and report what came back.
///
/// Returns false when nothing was heard at all, which is a different outcome from "no networks here" and is
/// reported as such.
/// The bound on EMPTY polls before giving up on a firmware that never says the scan is over. Not a
/// duration: each empty poll is one CMD53 and a 1 ms sleep, so this is on the order of tens of seconds
/// of silence, and the log says which of the two ended the wait. The scan itself ends when the firmware
/// says it does (`status::SUCCESS`), which on hardware was about 2.6 s after it started.
pub const MAX_EMPTY_POLLS: u32 = 500;

/// Bring the radio up for scanning - ONCE, at boot. Returns the session the rest of the driver's life
/// runs on, so request ids keep counting across every later scan.
///
/// Split out of the boot self-test because `wifi scan` starts a sweep on request, and re-sending the CLM
/// blob and the UP chain on every request would be wrong - the interface is already up. The bring-up
/// happens here, once; `start` and `step` do the part that repeats, from the serve loop.
pub fn bring_up(h: &Host, w: &mut Window, adopted: bool, ctx: &ServiceContext) -> Option<ctrl::Session> {
    let mut session = ctrl::Session::new(ctx);

    // THE CLM BLOB FIRST, and it is first for a reason rather than by habit. `bwfm_init` is preceded by
    // `bwfm_preinit`, whose brcmfmac twin `brcmf_c_preinit_dcmds` calls `brcmf_c_process_clm_blob`. CLM is
    // the Country Locale Matrix - which channels may be used at what power - and a radio with no regulatory
    // data cannot lawfully transmit or scan. That is why every configuration command was accepted and
    // `escan` still answered `BCME_NOTUP`.
    //
    // ONCE PER FIRMWARE, and an ADOPTED firmware has had it. The blob is accepted only while the
    // interface is down; a fresh firmware starts down, and the firmware an earlier instance loaded is
    // up and associated - which is exactly why it is worth adopting. Sent again it is refused with
    // `BCME_NOTDOWN` (boot 2026-10-01 00:53, the first successful adoption), and taking that refusal
    // as "no channel rules" threw away a working radio. Everything after this point is idempotent on
    // a running firmware and runs either way.
    if adopted {
        ctx.log("wifi-driver: adopted firmware - its CLM was loaded by the instance that started it and is not sent again (refused while up)");
    } else if !ctrl::download_blob(
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
        return None;
    }

    // ASK FOR THE EVENTS NEXT. The firmware sends NONE until the host sets this mask, so a scan accepted
    // without it would run and report nothing - which is indistinguishable from an empty room.
    if !ctrl::enable_events(
        h,
        w,
        &mut session,
        &[
            code::ESCAN_RESULT,
            code::LINK,
            code::SET_SSID,
            code::ASSOC,
            code::JOIN,
            code::AUTH,
            code::DEAUTH_IND,
            code::DISASSOC_IND,
        ],
        ctx,
    ) {
        ctx.log(
            "wifi-driver: the event mask was not set, so a scan would produce no results even if accepted \
             - not attempting one",
        );
        return None;
    }

    // WHAT THE FIRMWARE SAYS IT IS, asked where `brcmf_feat_attach` asks - after preinit, before UP. This
    // decides nothing; it prints the three answers a later decision rests on (`ctrl::report_firmware`).
    ctrl::report_firmware(h, w, &mut session, ctx);

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
        return None;
    }
    Some(session)
}

/// One whole scan, blocking, on a radio `bring_up` has already raised - the boot self-test's form. The
/// serve loop does not use it: `wifi scan` runs `start` and then `step` one frame at a time, so it can
/// answer requests between frames, and `wifi list` prints the cache the last such sweep left.
///
/// Returns the `Scan` whatever it holds - an empty room is a result, not a failure - and `None` only when
/// the firmware refused to start the scan at all, which its decoded error explains one line above.
pub fn scan_once(
    h: &Host,
    w: &mut Window,
    s: &mut ctrl::Session,
    ctx: &ServiceContext,
) -> Option<Scan> {
    let mut scan = Scan::new();
    if !start(h, w, s, ctx) {
        return None;
    }
    collect(h, w, s, &mut scan, MAX_EMPTY_POLLS, ctx);

    ctx.log_fmt(format_args!(
        "wifi-driver: the scan window saw {} event/data frame(s), {} escan-result event(s), {} glommed \
         frame(s) ignored, {} on other channels",
        scan.events, scan.results, scan.glom, scan.other
    ));
    Some(scan)
}

/// The boot self-test: bring the radio up, scan once, and print what it found to the log.
///
/// Returns the session on ANY outcome past bring-up, because a scan that found nothing - or whose results
/// did not parse - is still a radio that is up and can be asked again by the shell. Only a failed bring-up
/// returns `None`, and then there is genuinely nothing to serve.
pub fn run(h: &Host, w: &mut Window, adopted: bool, ctx: &ServiceContext) -> Option<ctrl::Session> {
    // The "UNVERIFIED ON HARDWARE" banner that stood here was true when written and would have been a
    // lie from the first successful boot on. Ten networks, names and all, on 2026-09-28.
    ctx.log("wifi-driver: stage 14 - scanning");

    let mut session = bring_up(h, w, adopted, ctx)?;
    let scan = match scan_once(h, w, &mut session, ctx) {
        Some(s) => s,
        None => return Some(session),
    };

    if scan.events == 0 {
        ctx.log(
            "wifi-driver: the scan was accepted and NO event frame arrived at all. That is the event path, \
             not the scan: rung A of docs/wifi.md 30.6 is what to prove next, and it needs no scan",
        );
        return Some(session);
    }
    if scan.results == 0 {
        ctx.log_fmt(format_args!(
            "wifi-driver: {} event(s) arrived but none was an ESCAN_RESULT ({}). The request was ACCEPTED \
             and produced no results, which is the case where the params VERSION is the first thing to \
             change - v0 is what this tried (docs/wifi.md 30.4)",
            scan.events,
            code::ESCAN_RESULT
        ));
        return Some(session);
    }
    if scan.count == 0 {
        ctx.log_fmt(format_args!(
            "wifi-driver: {} escan-result event(s) arrived and no network parsed out of them - so the \
             result offsets are what to check, docs/wifi.md 30.2",
            scan.results
        ));
        return Some(session);
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
    Some(session)
}
