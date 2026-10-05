// SPDX-License-Identifier: GPL-2.0-only
//! THE USB RADIO'S FUNCTION PROTOCOL: what a USB host service (`dwc2`, later `xhci`) answers for the one
//! device it has bound as a WiFi radio, and what `wifi-usb` asks. ONE definition, read by both sides.
//!
//! **Narrow on purpose.** A host binds a radio by its VID:PID - a vendor-class device, so there is no class
//! to match on - and then serves exactly that device: who it is, and its control transfers. It is not a
//! passthrough to the bus: a host never lets a client address another device, and knows nothing of the
//! chip behind the requests. The register file, the firmware and the 802.11 above them are `wifi-usb`'s.
//! Bulk transfers join when frames do (`docs/wifi-usb.md`).
//!
//! Every reply starts `[op, status]`; the status is one of the `ST_` values, so a reply to the wrong op, or a
//! host that does not speak this protocol, is told apart from an answer.

/// `[op]` -> `[op, status, vid lo, vid hi, pid lo, pid hi]`: which device is bound as the radio, if any.
pub const OP_INFO: u8 = 0x20;
/// `[op, setup(8), data out...]` -> `[op, status, data in...]`. One control transfer to the bound radio: the
/// 8-byte setup packet as USB defines it; for an OUT transfer the data follows it, `wLength` bytes; for an
/// IN transfer the reply carries what the device returned.
pub const OP_CONTROL: u8 = 0x21;

/// `[NOTE_RADIO]`, sent BY the host TO the driver, with no reply expected: the radio's binding changed -
/// a dongle was bound or removed - so ask `OP_INFO`. The host sends it with `try_send`, so it never blocks
/// on a driver that is behind; a notice lost to a full queue costs only that change, and the driver asks
/// `OP_INFO` once when it starts, so a respawn on either side begins from the truth. This is what lets the
/// driver block in `recv` instead of asking on a timer (`docs/wifi-usb.md`, U1b).
pub const NOTE_RADIO: u8 = 0x2F;

/// The transfer was done (or the device is bound, for `OP_INFO`).
pub const ST_OK: u8 = 0;
/// No radio is bound on this host - none plugged in, or this host has not enumerated it yet.
pub const ST_NO_DEVICE: u8 = 1;
/// The host tried the transfer and the device or the bus did not complete it.
pub const ST_FAILED: u8 = 2;
/// The request was malformed: too short, or a `wLength` over `CONTROL_MAX`.
pub const ST_BAD_REQUEST: u8 = 3;

/// The most data one control transfer may carry either way. The hosts' control buffers are this size or
/// larger, and the Realtek's register file is reached a word at a time, its firmware 128 bytes at a time.
pub const CONTROL_MAX: usize = 256;
