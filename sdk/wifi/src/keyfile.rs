// SPDX-License-Identifier: GPL-2.0-only
//! `/wifi.keys`: the credential table on disk, so a machine that boots with a radio joins the network it
//! last joined and is ready to go (`utilities/56_wifi.md` 6, decided by the operator 2026-09-30).
//!
//! **What is on the card, said plainly.** The DERIVED KEY of each network - the 32-byte pairwise master
//! key the passphrase becomes the moment it arrives (`crypto::psk`) - never the passphrase itself. A key
//! joins the network exactly as the passphrase would, so whoever holds the card can join it; that is
//! accepted. It does not reveal the passphrase text, which people reuse elsewhere. Network names are in
//! plain text. Nothing is encrypted at rest. There is no per-machine secret to encrypt with, and pretending
//! otherwise would be the silent substitution 26.4 names.
//!
//! **The in-memory table stays the working set** (`main.rs`, `Stored`). This file is where it is loaded
//! from when the radio comes up and written to after every change - a join that added or re-ordered a key,
//! a `forget` - and where `fs` is absent or mid-restart the driver runs on the table alone, exactly as it
//! did before the file existed. A load that cannot reach `fs` is retried a bounded number of times and
//! then given up with a line, never waited for.
//!
//! **Format**, version 1, little-endian, most recently used first: `"GSWK"`, version byte, count byte,
//! then `count` entries of `[len, ssid[32], security, pmk[32]]` (66 bytes). At most `MAX_SAVED` entries,
//! which keeps the whole file inside one `fs` write; the in-memory table's 64 is the larger bound and the
//! sixteen least recent are simply not saved. Open networks hold no key and are not saved.

use godspeed_sdk::{Message, ServiceContext};

pub const PATH: &[u8] = b"/wifi.keys";
const MAGIC: [u8; 4] = *b"GSWK";
const FILE_VERSION: u8 = 1;
pub use crate::crypto::PMK_LEN;
pub use crate::wire::SSID_MAX;
pub const MAX_SAVED: usize = 48;
const HEADER: usize = 6;
const ENTRY: usize = 1 + SSID_MAX + 1 + PMK_LEN;

/// `fs`'s whole-file ops: `[tag, op, path_len, path, data...]` in, `[tag, status, ...]` out; a read answers
/// `[tag, FS_OK, len:u32, bytes]`. The tag is echoed and interpreted no further.
const FS_OP_WRITE: u8 = 10;
const FS_OP_READ: u8 = 11;
const FS_OK: u8 = 0;
const TAG: u8 = 0xA7;
/// One exchange with `fs`. A read of a 3 KiB file is milliseconds; two seconds is the loud floor.
const FS_SECS: i64 = 2;

#[derive(Clone, Copy)]
pub struct Entry {
    pub ssid: [u8; SSID_MAX],
    pub len: u8,
    pub sec: u8,
    pub pmk: [u8; PMK_LEN],
}

impl Entry {
    pub const EMPTY: Entry = Entry { ssid: [0; SSID_MAX], len: 0, sec: 0, pmk: [0; PMK_LEN] };
}

pub enum Load {
    /// This many entries read, most recent first.
    Loaded(usize),
    /// `fs` answered and there is no file, or one this version cannot read: settled, nothing to adopt.
    NoFile,
    /// `fs` did not answer: not settled, worth asking again.
    Unreachable,
}

/// One request to `fs`, matched to its own reply, with one reacquire-and-retry when the send itself failed
/// (an `fs` respawned since this driver was wired). A deadline is never re-sent.
fn ask(ctx: &ServiceContext, req: &[u8]) -> Option<Message> {
    let msg = Message::from_bytes(req);
    match ctx.request_with_reply_call_err("fs", &msg, FS_SECS) {
        Ok(r) => r,
        Err(_) => {
            if !ctx.reacquire_by_name("fs") {
                return None;
            }
            match ctx.request_with_reply_call_err("fs", &msg, FS_SECS) {
                Ok(r) => r,
                Err(_) => None,
            }
        }
    }
}

pub fn load(ctx: &ServiceContext, out: &mut [Entry; MAX_SAVED]) -> Load {
    let mut req = [0u8; 3 + 32];
    req[0] = TAG;
    req[1] = FS_OP_READ;
    req[2] = PATH.len() as u8;
    req[3..3 + PATH.len()].copy_from_slice(PATH);
    let r = match ask(ctx, &req[..3 + PATH.len()]) {
        Some(r) => r,
        None => return Load::Unreachable,
    };
    let p = r.payload_bytes();
    if p.len() < 6 || p[1] != FS_OK {
        return Load::NoFile;
    }
    let n = u32::from_le_bytes([p[2], p[3], p[4], p[5]]) as usize;
    let data = &p[6..core::cmp::min(p.len(), 6 + n)];
    if data.len() < HEADER || data[..4] != MAGIC || data[4] != FILE_VERSION {
        ctx.log_fmt(format_args!(
            "wifi-driver: /wifi.keys is {} bytes and not a version {} key file - ignored, and it will be rewritten by the next join",
            data.len(), FILE_VERSION
        ));
        return Load::NoFile;
    }
    let count = core::cmp::min(data[5] as usize, MAX_SAVED);
    let mut got = 0usize;
    for i in 0..count {
        let at = HEADER + i * ENTRY;
        if at + ENTRY > data.len() {
            break;
        }
        let e = &data[at..at + ENTRY];
        let len = e[0] as usize;
        if len == 0 || len > SSID_MAX {
            continue;
        }
        let mut entry = Entry::EMPTY;
        entry.len = len as u8;
        entry.ssid.copy_from_slice(&e[1..1 + SSID_MAX]);
        entry.sec = e[1 + SSID_MAX];
        entry.pmk.copy_from_slice(&e[2 + SSID_MAX..2 + SSID_MAX + PMK_LEN]);
        out[got] = entry;
        got += 1;
    }
    Load::Loaded(got)
}

/// Write the table, most recent first, at most `MAX_SAVED` entries. True when `fs` accepted it.
pub fn save(ctx: &ServiceContext, entries: &[Entry]) -> bool {
    let count = core::cmp::min(entries.len(), MAX_SAVED);
    let mut req = [0u8; 3 + 32 + HEADER + MAX_SAVED * ENTRY];
    req[0] = TAG;
    req[1] = FS_OP_WRITE;
    req[2] = PATH.len() as u8;
    req[3..3 + PATH.len()].copy_from_slice(PATH);
    let mut at = 3 + PATH.len();
    req[at..at + 4].copy_from_slice(&MAGIC);
    req[at + 4] = FILE_VERSION;
    req[at + 5] = count as u8;
    at += HEADER;
    for e in entries.iter().take(count) {
        req[at] = e.len;
        req[at + 1..at + 1 + SSID_MAX].copy_from_slice(&e.ssid);
        req[at + 1 + SSID_MAX] = e.sec;
        req[at + 2 + SSID_MAX..at + 2 + SSID_MAX + PMK_LEN].copy_from_slice(&e.pmk);
        at += ENTRY;
    }
    let ok = match ask(ctx, &req[..at]) {
        Some(r) => {
            let p = r.payload_bytes();
            p.len() >= 2 && p[1] == FS_OK
        }
        None => false,
    };
    // The keys do not linger in this buffer once the write is decided.
    req.fill(0);
    if !ok {
        ctx.log_fmt(format_args!(
            "wifi-driver: /wifi.keys was NOT written ({} entr{}) - fs refused or did not answer; the table in memory is unchanged and the next join tries again",
            count,
            if count == 1 { "y" } else { "ies" }
        ));
    }
    ok
}
