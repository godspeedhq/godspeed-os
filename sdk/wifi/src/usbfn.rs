// SPDX-License-Identifier: GPL-2.0-only
//! THE USB RADIO'S FUNCTION PROTOCOL: what a USB host service (`dwc2`, later `xhci`) answers for the one
//! device it has bound as a WiFi radio, and what `wifi-usb` asks. ONE definition, read by both sides.
//!
//! **Narrow on purpose.** A host binds a radio by its VID:PID - a vendor-class device, so there is no class
//! to match on - and then serves exactly that device: who it is, and its control transfers. It is not a
//! passthrough to the bus: a host never lets a client address another device, and knows nothing of the
//! chip behind the requests. The register file, the firmware and the 802.11 above them are `wifi-usb`'s.
//! Bulk transfers carry the frames: one IN the host keeps armed (`OP_BULK_IN`), and an OUT per frame sent
//! (`OP_BULK_OUT`, `docs/wifi-usb.md`).
//!
//! Every reply starts `[op, status]`; the status is one of the `ST_` values, so a reply to the wrong op, or a
//! host that does not speak this protocol, is told apart from an answer.

/// `[op]` -> `[op, status, vid lo, vid hi, pid lo, pid hi]`: which device is bound as the radio, if any.
pub const OP_INFO: u8 = 0x20;
/// `[op, setup(8), data out...]` -> `[op, status, data in...]`. One control transfer to the bound radio: the
/// 8-byte setup packet as USB defines it; for an OUT transfer the data follows it, `wLength` bytes; for an
/// IN transfer the reply carries what the device returned.
pub const OP_CONTROL: u8 = 0x21;
/// `OP_CONTROL`, attempted EXACTLY ONCE: the host does not retry it, and a failure is `ST_FAILED`. For a
/// transfer that must not reach the device twice. A firmware block is one: the host retrying a block whose
/// data arrived and whose status stage failed sends it again, and the chip's checksum is then never
/// reported (seen on a Pi 2 replug, R2). Linux sends each block once and restarts the whole download on a
/// failure (`rtl8xxxu_download_firmware`'s `-EAGAIN`), which needs the failure to be seen.
pub const OP_CONTROL_ONCE: u8 = 0x22;
/// `[op]` -> `[op, status, transfer...]`: the bulk IN transfer the host has taken from the radio, if it holds
/// one, and the host's IN armed again; `ST_OK` with no bytes when it holds none, which also arms the IN if it
/// was not armed - so the driver's first ask, once the chip's receive is set up, is what starts receiving.
/// The host keeps one IN armed in the background and takes its completion on the USB interrupt, then sends
/// `NOTE_BULK_IN`; it does not arm again until the transfer is collected, so the chip holds what arrives
/// meanwhile and nothing is dropped between the two. `ST_FAILED` when the radio has no bulk IN endpoint.
pub const OP_BULK_IN: u8 = 0x23;
/// `[op, out, transfer...]` -> `[op, status]`: one bulk OUT transfer to the radio - a frame with the chip's
/// transmit descriptor in front of it. `out` is the endpoint's POSITION among the radio's bulk OUT endpoints
/// in its configuration descriptor, 0 first: the order Linux's `rtl8xxxu_parse_usb` fills `out_ep[]` in, so
/// the driver maps its queues to endpoints as `rtl8xxxu_init_queue_priority` does without knowing any
/// endpoint's number. `ST_BAD_REQUEST` for an `out` the radio does not have or an empty transfer;
/// `ST_FAILED` when the device or the bus did not take it.
pub const OP_BULK_OUT: u8 = 0x24;

/// `[NOTE_BULK_IN]`, sent BY the host TO the driver, no reply expected: a bulk IN transfer is held, ask
/// `OP_BULK_IN`. Sent with `try_send`; one the driver's full queue refused is sent again on the host's next
/// pass, because the host arms nothing until the transfer is collected and a lost notice would stop receive.
pub const NOTE_BULK_IN: u8 = 0x2E;

// 0x29 is NOT free in this range: `dwc2` receives the USB interrupt as the one-byte message `[0x29]`, told
// apart from a request only by carrying no reply cap, so no op may take that value.

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

/// The most one bulk IN transfer carries: seven 512-byte high-speed packets, the most whole packets that fit
/// one IPC message beside the reply's two bytes. It holds one unaggregated receive transfer, which Linux
/// sizes as `IEEE80211_MAX_FRAME_LEN` (2352, `include/linux/ieee80211.h`) plus the receive descriptor
/// (`rtl8xxxu_submit_rx_urb`); a driver that turns the chip's receive aggregation on must keep its batches
/// under it. A whole number of packets, because a device that sends more than the host asked for is babble.
pub const BULK_IN_MAX: usize = 3584;
const _: () = assert!(BULK_IN_MAX + 2 <= godspeed_sdk::ipc::MAX_PAYLOAD);
const _: () = assert!(BULK_IN_MAX % 512 == 0);

/// The most one bulk OUT transfer carries: the rest of an IPC message after the op and the endpoint byte. A
/// full-length frame with its 32-byte descriptor is about 2,400 bytes, so this is room, not a squeeze.
pub const BULK_OUT_MAX: usize = godspeed_sdk::ipc::MAX_PAYLOAD - 2;
