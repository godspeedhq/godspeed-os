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
pub use crate::addr::Ipv4;

// The opcodes, as `services/net-stack` dispatches them (its `pl.first() == Some(&N)` arms).
const OP_RESOLVE: u8 = 1;
const OP_PING: u8 = 3;
const OP_ARP: u8 = 6;
const OP_RENEW: u8 = 8;
const OP_TCP: u8 = 21;
/// Any opcode `net-stack` does not recognise is answered with the status blob. Using an explicitly
/// reserved value rather than a real operation keeps that from being an accident. It is not a
/// guess: `net-stack` lists 0 alongside the real read-only ops in its gateway-retry trigger, so the
/// service already treats it as a request a client legitimately makes.
const OP_STATUS: u8 = 0;

/// The first byte of every request, echoed at byte 0 of the reply so a late answer to an earlier
/// question is recognised rather than read as this one's. A SECOND counter from the filesystem's:
/// the two channels are independent and a tag only has to be unique against its own.
const TAG_BASE: u8 = 0x80;

/// Everything after the echoed tag.
fn body(m: &Message) -> &[u8] {
    let b = m.payload_bytes();
    if b.len() > 1 { &b[1..] } else { &[] }
}

/// A resolve or a TCP transaction crosses the network, so the five-second default is too tight.
/// `net-stack` bounds a DNS resolve by the CLIENT's patience rather than by an attempt count (its
/// own words, and a fix on this branch), which means the number here IS the policy.
pub const NET_SECS: i64 = 10;

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
    tag: u8,
    /// Fired once when a request lingers. See [`Net::with_notice`].
    notice: Option<&'a dyn Fn()>,
}

impl<'a> Net<'a> {
    /// Take a network handle. Cheap, allocates nothing, grants nothing.
    pub fn new(ctx: &'a ServiceContext) -> Net<'a> {
        Net { ctx, tag: TAG_BASE, notice: None }
    }

    /// A network handle that tells you when a call is taking a while.
    ///
    /// `notice` is fired once, after a couple of seconds, if a request has not been answered. It
    /// takes nothing and returns nothing: it means only "this is lasting". An interactive program
    /// prints `[q] quit` and starts watching the keyboard; a daemon might log it; most programs do
    /// not need it at all.
    ///
    /// This exists because `services/shell` could not otherwise move onto this library without
    /// dropping that affordance on exactly the calls where a person needs it - a `tcp` to an
    /// unreachable host waits the full deadline in silence. Losing that would have been a
    /// regression dressed up as a migration.
    pub fn with_notice(ctx: &'a ServiceContext, notice: &'a dyn Fn()) -> Net<'a> {
        Net { ctx, tag: TAG_BASE, notice: Some(notice) }
    }

    /// Send `[tag, patience, body..]` and return the reply with the tag checked and stripped.
    ///
    /// **The two header bytes are not optional.** `net-stack` strips them from any request of two
    /// bytes or more and echoes the tag back at byte 0, so a request built without them has its
    /// OPCODE eaten as a tag and is dispatched on whatever byte follows. The first draft of this
    /// module got that wrong, and only `status()` worked - because a one-byte request falls below
    /// the strip threshold and reaches the default arm, which answers status anyway.
    ///
    /// The patience byte is this call's deadline, told to the service so it can stop holding a
    /// request its client has already given up on. It is the client's number, which is why it
    /// travels with the request rather than living in the service.
    fn call(&mut self, body: &[u8], secs: i64) -> Result<Message, Error> {
        let mut req = [0u8; 512];
        if 2 + body.len() > req.len() {
            return Err(Error::InvalidInput);
        }
        self.tag = TAG_BASE.wrapping_add(self.tag.wrapping_sub(TAG_BASE).wrapping_add(1) & 0x3F);
        let tag = self.tag;
        req[0] = tag;
        req[1] = secs.clamp(0, 255) as u8;
        req[2..2 + body.len()].copy_from_slice(body);
        let r = call::request_within_notice(
            self.ctx, "net-stack", &Message::from_bytes(&req[..2 + body.len()]), secs, self.notice)?;
        // A reply whose tag does not match is the answer to a request we already gave up on. Reading
        // it as this one's is how a channel goes "out of step" and every later exchange answers the
        // question before.
        if r.payload_bytes().first() != Some(&tag) {
            return Err(Error::Malformed);
        }
        Ok(r)
    }

    /// What the machine currently believes about the network.
    ///
    /// **Blocks** briefly. **Read-only**, so every no-answer failure is safe to retry.
    pub fn status(&mut self) -> Result<Status, Error> {
        let r = self.call(&[OP_STATUS], call::DEFAULT_SECS)?;
        let b = body(&r);
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
    pub fn resolve(&mut self, host: &str) -> Result<Ipv4, Error> {
        let mut req = [0u8; 256];
        let n = host.len().min(req.len() - 1);
        if n != host.len() {
            return Err(Error::InvalidInput);
        }
        req[0] = OP_RESOLVE;
        req[1..1 + n].copy_from_slice(host.as_bytes());
        let r = self.call(&req[..1 + n], NET_SECS)?;
        let b = body(&r);
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
    pub fn ping(&mut self, ip: Ipv4) -> Result<bool, Error> {
        let req = [OP_PING, ip.0[0], ip.0[1], ip.0[2], ip.0[3]];
        let r = self.call(&req, NET_SECS)?;
        Ok(body(&r).first() == Some(&1))
    }

    /// Resolve one host's MAC address on the local link.
    ///
    /// **Blocks.** `Ok(None)` means nothing answered, which on a local link usually means the host
    /// is not there - again a result rather than a failure.
    pub fn arp(&mut self, ip: Ipv4) -> Result<Option<[u8; 6]>, Error> {
        let req = [OP_ARP, ip.0[0], ip.0[1], ip.0[2], ip.0[3]];
        let r = self.call(&req, NET_SECS)?;
        let b = body(&r);
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
    pub fn tcp(&mut self, ip: Ipv4, port: u16, request: &[u8], buf: &mut [u8]) -> Result<usize, Error> {
        let mut req = [0u8; 500];
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
        let b = body(&r);
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
    pub fn renew(&mut self) -> Result<(), Error> {
        self.call(&[OP_RENEW], NET_SECS)?;
        Ok(())
    }
}
