// SPDX-License-Identifier: GPL-2.0-only
//! A scan's result, as every radio reports it: the networks heard, what their beacons say about how they
//! are secured, and the fixed record layout a `wifi list` reply carries them in.
//!
//! How a firmware hands a scan to its host differs - Broadcom digests each beacon into its own BSS-info record, the
//! AIC8800 forwards the raw beacon - but the list it fills does not. Each driver turns what its firmware
//! says into [`Network`]s and [`Scan::keep`]s them; the serve loop serves the list.

use crate::wire::{self as reply, SSID_MAX as MAX_SSID};

/// How many networks to keep. Bounded on purpose: a fixed array whose limit is readable here (§26.6.1).
pub const MAX_RESULTS: usize = 32;

/// What a network's beacon says about how it is secured. Wire value in a `wifi list` record's byte 43.
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
    pub fn blank() -> Self {
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
pub const CAPINFO_PRIVACY: u16 = 0x0010;

/// Classify a network from its beacon's information elements and capability field.
///
/// The elements are a walk of `[id][len][body]` records, quoted from the reference exactly as net80211
/// does it: an RSN element is WPA2; a vendor element whose body opens with the Microsoft OUI and type 1 is
/// WPA; the Privacy capability bit with neither element is WEP; nothing is open. A walk that runs off the
/// end stops rather than reads past it - the length bytes come from the air and are not trusted.
pub fn classify(ies: &[u8], capability: u16) -> u8 {
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
    pub nets: [Network; MAX_RESULTS],
    pub count: usize,
    /// Results the firmware reported that did not fit. Counted rather than dropped silently (§26.7).
    pub dropped: u32,
    /// The DRIVER'S tallies of what its firmware sent during the sweep, for its own log; their meaning is
    /// the driver's (on the Broadcom: events seen, escan results among them, glommed superframes not read).
    pub events: u32,
    /// Result events specifically (the Broadcom's escan results).
    pub results: u32,
    /// Frames seen and not read (the Broadcom's glommed superframes).
    pub glom: u32,
    /// Frames on a channel this driver does not read at all.
    pub other: u32,
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
    pub fn keep(&mut self, n: Network) {
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

/// Serialise a scan into a reply: `[status, count, record * count]`. Returns the bytes written.
///
/// A FIXED layout with no framing to parse on the far side - the shell indexes into it. 32 records of 45
/// bytes plus two is 1442 bytes, well inside a 4096-byte message, and `Scan` already bounds the count.
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
        // Byte 43, once a pad, carries the `sec::` value; byte 44, the last, is the note.
        out[at + 43] = n.security;
        out[at + 44] = note(n);
        at += reply::RECORD;
    }
    at
}

