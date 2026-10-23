// SPDX-License-Identifier: Apache-2.0
//! The USB FUNCTION PROTOCOL's numbers: what a USB host service (`xhci`, `dwc2`) answers for the one device it
//! binds for a driver of its own, and what that driver asks. A driver package speaks it through
//! `gs::driver::bus`; the WiFi dongle's driver through `godspeed_wifi::usbfn`, which re-exports these and says
//! what each one means in full (`sdk/wifi/src/usbfn.rs`). Every request starts with its op, and every reply with
//! `[op, status]`.

/// `[op]` -> `[op, status, vid (2 LE), pid (2 LE), gen (4 LE), ...]`: which device is bound, and which binding of it.
pub const OP_INFO: u8 = 0x20;
/// Where an `OP_INFO` answer's binding generation starts.
pub const INFO_GEN_AT: usize = 6;
/// Where the optional part of an `OP_INFO` answer starts (where the device is, from a host that says).
pub const INFO_WHERE_AT: usize = 10;
/// `[op, setup (8), data out...]` -> `[op, status, data in...]`: one control transfer on the default endpoint.
pub const OP_CONTROL: u8 = 0x21;
/// `OP_CONTROL`, attempted exactly once: the host does not retry it.
pub const OP_CONTROL_ONCE: u8 = 0x22;
/// `[op]` -> `[op, status, transfer...]`: the bulk IN transfer the host holds, if any; the IN armed again.
pub const OP_BULK_IN: u8 = 0x23;
/// `[op, out, transfer...]` -> `[op, status]`: one bulk OUT transfer to the device's `out`th bulk OUT endpoint.
pub const OP_BULK_OUT: u8 = 0x24;
/// `[op, notice]`, never answered: names a notice that arrived in place of an answer.
pub const OP_SYNC: u8 = 0x25;
/// Sent by the host, unanswered: a bulk IN transfer is held - ask `OP_BULK_IN`.
pub const NOTE_BULK_IN: u8 = 0x2E;
/// `[op]` -> `[op, status]`: reset the device's port and bind it afresh.
pub const OP_REPLUG: u8 = 0x27;
/// Sent by the host, unanswered: the binding changed - ask `OP_INFO`.
pub const NOTE_RADIO: u8 = 0x2F;
/// The transfer was done (or, for `OP_INFO`, a device is bound).
pub const ST_OK: u8 = 0;
/// No device is bound on this host.
pub const ST_NO_DEVICE: u8 = 1;
/// The host tried, and the device or the bus did not complete it.
pub const ST_FAILED: u8 = 2;
/// The request was malformed.
pub const ST_BAD_REQUEST: u8 = 3;
/// The most data one control transfer carries, either way.
pub const CONTROL_MAX: usize = 256;
/// The most one bulk IN transfer carries: seven 512-byte high-speed packets, beside the reply's two bytes.
pub const BULK_IN_MAX: usize = 3584;
/// The most one bulk OUT transfer carries: the rest of a message after the op and the endpoint byte.
pub const BULK_OUT_MAX: usize = crate::ipc::MAX_PAYLOAD - 2;
const _: () = assert!(BULK_IN_MAX + 2 <= crate::ipc::MAX_PAYLOAD);
const _: () = assert!(BULK_IN_MAX % 512 == 0);
