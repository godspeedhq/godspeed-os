// SPDX-License-Identifier: Apache-2.0
//! `bus` - a device a driver reaches through the host of the bus it is on, not through registers of its own.
//!
//! A USB device has no register window and no DMA arena a driver could be granted: its host controller owns both,
//! and talks to every device on its bus. So a USB device's driver is the host's CLIENT. The host binds the device
//! (`xhci` and `dwc2` do, for one device each that they do not drive themselves), the supervisor starts the driver
//! the device's VID:PID names with that host as its peer, and the driver asks the host for the device's transfers:
//! who it is, control transfers on its default endpoint, and its bulk endpoints. That conversation is
//! `godspeed_sdk::usbfn`; this module is it in the one shape a driver should use.
//!
//! **A bus, not a class.** Nothing here knows what the device is - a radio, a serial adapter, a sensor - any more
//! than `registers` knows what a register does. It moves transfers, and the device's meaning is its driver's
//! (`docs/driver-library.md`, "What belongs here").
//!
//! What a host answers is narrow on purpose: the one device it bound for this driver, and nothing else on the bus.

use godspeed_sdk::{Message, ServiceContext};

pub use godspeed_sdk::usbfn as wire;

/// The hosts that bind a device for a driver of its own. A driver is given exactly one of them as its peer, so
/// trying each in turn finds the one it was given and no other.
const HOSTS: [&str; 2] = ["xhci", "dwc2"];
/// The bound on one request to the host. A control transfer takes milliseconds; a host retrying a device that does
/// not answer can use up this bound, so a request that is not answered in it means the device is not answering.
const HOST_SECS: i64 = 2;
/// How many of the host's notices may arrive in place of an answer before that is said rather than chased
/// (`wire::OP_SYNC`).
const SYNC_MAX: u32 = 8;

/// Why a transfer did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbError {
    /// The host has no device bound for this driver: unplugged, or not enumerated yet.
    NoDevice,
    /// The host tried, and the device or the bus did not complete it.
    Failed,
    /// The request was not one the host takes: too long, an endpoint the device does not have, or empty.
    BadRequest,
    /// The host itself did not answer, or answered something that was not this protocol.
    Host(crate::Error),
}

impl UsbError {
    /// A sentence for a log line.
    pub fn as_str(&self) -> &'static str {
        match self {
            UsbError::NoDevice => "the host has no device bound for this driver",
            UsbError::Failed => "the device or the bus did not complete the transfer",
            UsbError::BadRequest => "the host refused the request as malformed",
            UsbError::Host(e) => e.as_str(),
        }
    }
}

/// Who the bound device is, as its host enumerated it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bound {
    pub vid: u16,
    pub pid: u16,
    /// Which binding of it this is: the same device bound again (a replug) has a different one.
    pub gen: u32,
}

/// The device this driver was started for, reached through its host.
pub struct UsbDevice<'a> {
    ctx: &'a ServiceContext,
    host: &'static str,
}

impl<'a> UsbDevice<'a> {
    /// The host this driver was given as its peer, or `None` when it was given none - it was not started for a
    /// USB device.
    pub fn granted(ctx: &'a ServiceContext) -> Option<UsbDevice<'a>> {
        HOSTS.iter().copied().find(|h| crate::ipc::peer(ctx, h).is_some()).map(|host| UsbDevice { ctx, host })
    }

    /// The host's name, for a log line.
    pub fn host(&self) -> &'static str {
        self.host
    }

    /// Who the bound device is (`wire::OP_INFO`).
    pub fn info(&self) -> Result<Bound, UsbError> {
        let r = self.ask(&[wire::OP_INFO])?;
        let p = r.payload_bytes();
        status(p, wire::OP_INFO)?;
        if p.len() < wire::INFO_GEN_AT + 4 {
            return Err(UsbError::Host(crate::Error::Malformed));
        }
        Ok(Bound {
            vid: u16::from_le_bytes([p[2], p[3]]),
            pid: u16::from_le_bytes([p[4], p[5]]),
            gen: u32::from_le_bytes([p[6], p[7], p[8], p[9]]),
        })
    }

    /// One control transfer IN on the default endpoint: `setup` is the 8-byte setup packet as USB defines it, and
    /// what the device returned is copied into `out`. How many bytes it returned.
    pub fn control_in(&self, setup: [u8; 8], out: &mut [u8]) -> Result<usize, UsbError> {
        let mut req = [0u8; 9];
        req[0] = wire::OP_CONTROL;
        req[1..9].copy_from_slice(&setup);
        let r = self.ask(&req)?;
        let p = r.payload_bytes();
        status(p, wire::OP_CONTROL)?;
        let n = (p.len() - 2).min(out.len());
        out[..n].copy_from_slice(&p[2..2 + n]);
        Ok(n)
    }

    /// One control transfer OUT on the default endpoint, with `data` as its data stage (at most
    /// `wire::CONTROL_MAX` bytes; the setup packet's length field is the caller's to set to match).
    pub fn control_out(&self, setup: [u8; 8], data: &[u8]) -> Result<(), UsbError> {
        if data.len() > wire::CONTROL_MAX {
            return Err(UsbError::BadRequest);
        }
        let mut req = [0u8; 9 + wire::CONTROL_MAX];
        req[0] = wire::OP_CONTROL;
        req[1..9].copy_from_slice(&setup);
        req[9..9 + data.len()].copy_from_slice(data);
        let r = self.ask(&req[..9 + data.len()])?;
        status(r.payload_bytes(), wire::OP_CONTROL)
    }

    /// One bulk OUT transfer to the device's `out`th bulk OUT endpoint, counted in the order its configuration
    /// descriptor lists them, from 0. At most `wire::BULK_OUT_MAX` bytes.
    pub fn bulk_out(&self, out: u8, data: &[u8]) -> Result<(), UsbError> {
        if data.is_empty() || data.len() > wire::BULK_OUT_MAX {
            return Err(UsbError::BadRequest);
        }
        let mut req = [0u8; 2 + wire::BULK_OUT_MAX];
        req[0] = wire::OP_BULK_OUT;
        req[1] = out;
        req[2..2 + data.len()].copy_from_slice(data);
        let r = self.ask(&req[..2 + data.len()])?;
        status(r.payload_bytes(), wire::OP_BULK_OUT)
    }

    /// The bulk IN transfer the host holds from the device, copied into `out`: how many bytes, 0 when it holds
    /// none. The host keeps one IN armed and arms it again as each is collected, so the first call is what starts
    /// receiving; it sends `wire::NOTE_BULK_IN` when one is held, for a driver that waits rather than asks.
    pub fn bulk_in(&self, out: &mut [u8]) -> Result<usize, UsbError> {
        let r = self.ask(&[wire::OP_BULK_IN])?;
        let p = r.payload_bytes();
        status(p, wire::OP_BULK_IN)?;
        let n = (p.len() - 2).min(out.len());
        out[..n].copy_from_slice(&p[2..2 + n]);
        Ok(n)
    }

    /// One request, bounded. A notice the host sent in place of the answer is named back with `OP_SYNC`, so the
    /// answer still on its way is the one returned and nothing is left owed (`wire::OP_SYNC`).
    #[inline(never)]
    fn ask(&self, body: &[u8]) -> Result<Message, UsbError> {
        let ask = |m: &[u8]| crate::call::request_within(self.ctx, self.host, &Message::from_bytes(m), HOST_SECS)
            .map_err(UsbError::Host);
        let mut r = ask(body)?;
        let mut syncs = 0u32;
        while let Some(&note) = r.payload_bytes().first().filter(|&&b| b == wire::NOTE_BULK_IN || b == wire::NOTE_RADIO) {
            if syncs == SYNC_MAX {
                return Err(UsbError::Host(crate::Error::Malformed));
            }
            syncs += 1;
            r = ask(&[wire::OP_SYNC, note])?;
        }
        Ok(r)
    }
}

/// The host's status for `op`, as an error where it is not `ST_OK`.
fn status(p: &[u8], op: u8) -> Result<(), UsbError> {
    match (p.first().copied(), p.get(1).copied()) {
        (Some(o), Some(wire::ST_OK)) if o == op => Ok(()),
        (Some(o), Some(wire::ST_NO_DEVICE)) if o == op => Err(UsbError::NoDevice),
        (Some(o), Some(wire::ST_FAILED)) if o == op => Err(UsbError::Failed),
        (Some(o), Some(wire::ST_BAD_REQUEST)) if o == op => Err(UsbError::BadRequest),
        _ => Err(UsbError::Host(crate::Error::Malformed)),
    }
}
