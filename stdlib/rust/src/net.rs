// SPDX-License-Identifier: Apache-2.0
//! The network, without the packing.
//!
//! # What this wraps, and what it deliberately does not
//!
//! `net-stack` answers two distinct kinds of request, and only the first is here:
//!
//! 1. **Plain requests to the service**, addressed by a leading opcode byte - resolve a name, ping a
//!    host, run one TCP transaction, ask what our address is. That is this module.
//!
//! 2. **Invocations of a delegated resource capability** (§7.10) - a UDP socket or a TCP listener is
//!    a real kernel capability that `net-stack` mints and grants, and using one means invoking it
//!    with an operation and a right. **That is not here**, because the machinery it needs is shared
//!    with file capabilities and belongs in its own module rather than being half-built twice.
//!    `docs/stdlib-design.md` §8 has the argument.
//!
//! So: this module gets you a name resolved, a host pinged, and a TCP request answered. It does not
//! get you a listening socket. That boundary is deliberate and is stated rather than discovered.
//!
//! # A caution the rest of this library does not need
//!
//! `net-stack`'s request protocol has moved 34 times in three releases. `fs`'s has not. Every opcode
//! below is therefore pinned with the source line that defines it, and the surface is kept to the
//! operations with a demonstrated caller - because the cost of this module being wrong is not a
//! compile error, it is a machine that quietly talks to the wrong port.

use godspeed_sdk::ipc::Message;
use godspeed_sdk::service_context::ServiceContext;

use crate::call;
use crate::error::Error;

// The opcodes, as `services/net-stack` dispatches them (its `pl.first() == Some(&N)` arms).
const OP_RESOLVE: u8 = 1;
const OP_PING: u8 = 3;
const OP_ARP: u8 = 6;
const OP_RENEW: u8 = 8;
const OP_TCP: u8 = 21;
/// Any opcode `net-stack` does not recognise is answered with the status blob. Using an explicitly
/// reserved value rather than a real operation keeps that from being an accident: if `net-stack`
/// ever gives 0 a meaning, this asks for something specific and wrong rather than something vague
/// and wrong, and the tests below would notice.
const OP_STATUS: u8 = 0;

/// A resolve or a TCP transaction crosses the network, so the five-second default is too tight.
/// `net-stack` bounds a DNS resolve by the CLIENT's patience rather than by an attempt count (its
/// own words, and a fix on this branch), which means the number here IS the policy.
pub const NET_SECS: i64 = 10;

/// An IPv4 address.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Ipv4(pub [u8; 4]);

impl Ipv4 {
    /// Parse dotted-quad. Returns `None` rather than guessing at anything malformed.
    pub fn parse(s: &str) -> Option<Ipv4> {
        let mut out = [0u8; 4];
        let mut parts = 0usize;
        for field in s.split('.') {
            if parts == 4 || field.is_empty() || field.len() > 3 {
                return None;
            }
            let mut v: u16 = 0;
            for c in field.bytes() {
                if !c.is_ascii_digit() {
                    return None;
                }
                v = v * 10 + (c - b'0') as u16;
            }
            if v > 255 {
                return None;
            }
            out[parts] = v as u8;
            parts += 1;
        }
        if parts == 4 { Some(Ipv4(out)) } else { None }
    }
}

/// What the machine currently believes about the network.
///
/// A snapshot, not a subscription: `net-stack` re-reads the link when asked, so a cable pulled since
/// the last call shows up on the next one rather than being cached into a comfortable lie.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Status {
    /// Our address. All zeroes means unconfigured.
    pub ip: Ipv4,
    /// The default gateway.
    pub gateway: Ipv4,
    /// The gateway's MAC, valid only when [`gateway_resolved`](Status::gateway_resolved).
    pub gateway_mac: [u8; 6],
    /// The DNS server, from the lease.
    pub dns: Ipv4,
    /// ARP has resolved the gateway, so traffic can leave.
    pub gateway_resolved: bool,
    /// A ping has succeeded at least once.
    pub ping_ok: bool,
    /// **DHCP granted this address**, as opposed to it being a fallback guess. This is the
    /// difference between configured and merely reachable, and it is the bit worth asserting on: a
    /// receive path that has stopped working shows up here as a stack that never got a lease.
    pub leased: bool,
}

impl Status {
    /// The short answer to "is the network usable": we have a leased address and a resolved gateway.
    pub fn usable(&self) -> bool {
        self.leased && self.gateway_resolved
    }
}

/// A handle to the network service.
///
/// # Authority
///
/// Carries none of its own. Every call rides the caller's existing `ipc_send = ["net-stack"]` grant;
/// a task whose contract never asked for it gets [`Error::Unreachable`], exactly as for any peer it
/// cannot reach. **There is no ambient network** (§3.1, and `docs/networking.md` says the same).
pub struct Net<'a> {
    ctx: &'a ServiceContext,
}

impl<'a> Net<'a> {
    /// Take a network handle. Cheap, allocates nothing, grants nothing.
    pub fn new(ctx: &'a ServiceContext) -> Net<'a> {
        Net { ctx }
    }

    fn call(&self, payload: &[u8], secs: i64) -> Result<Message, Error> {
        call::request_within(self.ctx, "net-stack", &Message::from_bytes(payload), secs)
    }

    /// What the machine currently believes about the network.
    ///
    /// **Blocks** briefly. **Read-only**, so every no-answer failure is safe to retry.
    pub fn status(&self) -> Result<Status, Error> {
        let r = self.call(&[OP_STATUS], call::DEFAULT_SECS)?;
        let b = r.payload_bytes();
        if b.len() < 19 {
            return Err(Error::Malformed);
        }
        Ok(Status {
            ip: Ipv4([b[0], b[1], b[2], b[3]]),
            gateway: Ipv4([b[4], b[5], b[6], b[7]]),
            gateway_mac: [b[8], b[9], b[10], b[11], b[12], b[13]],
            gateway_resolved: b[14] & 1 != 0,
            ping_ok: b[14] & 2 != 0,
            leased: b[14] & 4 != 0,
            dns: Ipv4([b[15], b[16], b[17], b[18]]),
        })
    }

    /// Resolve a hostname to an address.
    ///
    /// **Blocks** up to [`NET_SECS`]: this leaves the machine and a name server may be slow or
    /// absent. **Read-only**, so a retry is always safe.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the name does not resolve. [`Error::OutcomeUnknown`] if no answer
    /// arrives in time - harmless here, since resolving twice costs only time.
    pub fn resolve(&self, host: &str) -> Result<Ipv4, Error> {
        let mut req = [0u8; 256];
        let n = host.len().min(req.len() - 1);
        if n != host.len() {
            return Err(Error::InvalidInput);
        }
        req[0] = OP_RESOLVE;
        req[1..1 + n].copy_from_slice(host.as_bytes());
        let r = self.call(&req[..1 + n], NET_SECS)?;
        let b = r.payload_bytes();
        // A resolve that found nothing answers short or zeroed rather than with an error status.
        if b.len() < 4 || b[..4] == [0, 0, 0, 0] {
            return Err(Error::NotFound);
        }
        Ok(Ipv4([b[0], b[1], b[2], b[3]]))
    }

    /// Send one ICMP echo and wait for the reply.
    ///
    /// **Blocks** up to [`NET_SECS`]. Returns `true` if the host answered.
    ///
    /// A host that does not answer is `Ok(false)`, not an error: silence is a legitimate result from
    /// a reachable network, and conflating it with "the request failed" is how a diagnostic tool
    /// starts lying about which half is broken.
    pub fn ping(&self, ip: Ipv4) -> Result<bool, Error> {
        let req = [OP_PING, ip.0[0], ip.0[1], ip.0[2], ip.0[3]];
        let r = self.call(&req, NET_SECS)?;
        Ok(r.payload_bytes().first() == Some(&1))
    }

    /// Resolve one host's MAC address on the local link.
    ///
    /// **Blocks.** `Ok(None)` means nothing answered, which on a local link usually means the host
    /// is not there - again a result rather than a failure.
    pub fn arp(&self, ip: Ipv4) -> Result<Option<[u8; 6]>, Error> {
        let req = [OP_ARP, ip.0[0], ip.0[1], ip.0[2], ip.0[3]];
        let r = self.call(&req, NET_SECS)?;
        let b = r.payload_bytes();
        if b.len() < 7 || b[0] != 1 {
            return Ok(None);
        }
        Ok(Some([b[1], b[2], b[3], b[4], b[5], b[6]]))
    }

    /// One whole TCP transaction: connect, send `request`, read the reply, close.
    ///
    /// Writes the reply into `buf` and returns how many bytes landed there.
    ///
    /// **Blocks** up to [`NET_SECS`]. **This one changes state on the far side**: it is a request to
    /// somebody else's server, and that server may act on it. On [`Error::OutcomeUnknown`] the
    /// request may have been delivered and acted upon, and re-sending it is a SECOND transaction -
    /// which for anything that is not a plain fetch is exactly the double-submit problem.
    /// [`Error::retry_is_safe`] answers `false` there for that reason.
    ///
    /// # Errors
    /// [`Error::Failed`] if the connection could not be made or was closed with nothing returned.
    pub fn tcp(&self, ip: Ipv4, port: u16, request: &[u8], buf: &mut [u8]) -> Result<usize, Error> {
        let mut req = [0u8; 512];
        let head = 7;
        if head + request.len() > req.len() {
            return Err(Error::InvalidInput);
        }
        req[0] = OP_TCP;
        req[1..5].copy_from_slice(&ip.0);
        req[5] = (port >> 8) as u8;
        req[6] = port as u8;
        req[head..head + request.len()].copy_from_slice(request);
        let r = self.call(&req[..head + request.len()], NET_SECS)?;
        let b = r.payload_bytes();
        // `net-stack` answers an unreachable peer with nothing at all, and the shell's own `tcp`
        // command reads that as "connected to nothing". Reported as a failure rather than as an
        // empty success, because an empty buffer is what a caller would otherwise act on.
        if b.is_empty() {
            return Err(Error::Failed);
        }
        // Checked BEFORE copying, so `BufferTooSmall` leaves the buffer untouched - the same
        // contract `fs::read_into` gives. A partially-filled buffer plus an error is the worst of
        // both: the caller cannot tell how much of it is real.
        if b.len() > buf.len() {
            return Err(Error::BufferTooSmall);
        }
        buf[..b.len()].copy_from_slice(b);
        Ok(b.len())
    }

    /// Re-run the address dance in place, for a link that came up after boot.
    ///
    /// **Blocks. Changes state**: it may replace the machine's address. On
    /// [`Error::OutcomeUnknown`] call [`status`](Net::status) to find out what happened rather than
    /// renewing again.
    pub fn renew(&self) -> Result<(), Error> {
        self.call(&[OP_RENEW], NET_SECS)?;
        Ok(())
    }
}
