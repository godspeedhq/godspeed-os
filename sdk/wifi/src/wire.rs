// SPDX-License-Identifier: GPL-2.0-only
//! The request/reply vocabulary between a radio driver and the shell's `wifi` utility. One definition, read
//! by both sides (the frame ops `nic-driver` uses, 0x10-0x12, are the Broadcom driver's own, in its
//! `frames.rs`) - it used to be the
//! driver's `scan::reply` and a hand-kept mirror of it in the shell (`wifi_wire`), the same fact twice.
//!
//! A reply's first byte is its status. A request may be TAGGED (`TAGGED`): see that constant.

/// A TAGGED request: `[TAGGED, tag, op, ...]`. The driver serves `[op, ...]` as usual and its reply
/// goes back as `[TAGGED, tag, status, ...]`, so the asker can tell its own answer from a late one.
/// The shell tags every request; `nic-driver`'s frame ops do not, and are served as they always were.
///
/// Why it exists (backlog/70): a `Call` takes the oldest reply FROM this driver, not the reply to the
/// request just sent, so an answer the shell had stopped waiting for was read as the next request's.
/// The shell tried to count what it was owed, and could not: its reply mailbox takes every peer's
/// replies, and nothing on a message says who sent it. A tag is a fact in the reply itself. Chosen
/// outside every request op (1-11 here, 0x10-0x12 in `frames`).
pub const TAGGED: u8 = 0xE7;

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

// ---- Request shapes the SHELL builds, kept beside the replies so both sides read one definition. ----

/// The tag tab completion asks with. It runs without the shell's state, so it cannot draw from the
/// shell's tag counter - which never hands out 0, so the two cannot collide.
pub const COMPLETION_TAG: u8 = 0;
/// The most records the driver holds, and so the most a sweep can number.
pub const MAX_RECORDS: usize = 32;
/// `IEEE80211_MAX_SSID_LEN`: an SSID is at most 32 bytes, the size of the field in the beacon.
pub const SSID_MAX: usize = 32;
/// `CYW43_WPA_MAX_PASSWORD_LEN`, the longest passphrase a join request carries.
pub const PASS_MAX: usize = 64;
/// Request layout for a join: `[op, ssid_len, ssid[32], pass_len, pass[64]]`.
pub const JOIN_REQ: usize = 1 + 1 + SSID_MAX + 1 + PASS_MAX;
/// Security bytes, the driver's reading of the beacon.
pub const SEC_WEP: u8 = 1;
