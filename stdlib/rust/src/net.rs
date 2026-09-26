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

use godspeed_sdk::capability::{CapHandle, RIGHT_READ, RIGHT_WRITE};
use godspeed_sdk::ipc::Message;
use godspeed_sdk::service_context::ServiceContext;

use crate::call;
use crate::error::Error;
use crate::resource::{self, Held};
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

/// Open a UDP socket, as `net-stack` numbers its operations.
const OP_OPEN_SOCKET: u8 = 2;
/// Listen on a TCP port: `[22, port_hi, port_lo]`.
const OP_LISTEN: u8 = 22;

// Operations on a LISTENER capability.
const LOP_ACCEPT: u8 = 0;
const LOP_CLOSE: u8 = 1;

// Operations on a CONNECTION capability.
const COP_RECV: u8 = 0;
const COP_SEND: u8 = 1;
const COP_CLOSE: u8 = 2;

/// Everything after the echoed tag.
fn body(m: &Message) -> &[u8] {
    let b = m.payload_bytes();
    if b.len() > 1 { &b[1..] } else { &[] }
}

/// A resolve or a TCP transaction crosses the network, so the five-second default is too tight.
/// `net-stack` bounds a DNS resolve by the CLIENT's patience rather than by an attempt count (its
/// own words, and a fix on this branch), which means the number here IS the policy.
pub const NET_SECS: i64 = 10;

/// How long to wait for a datagram sent through a [`Socket`].
///
/// **Longer than `net-stack`'s own worst case for the operation, and that is the whole point.** It
/// retries a datagram `DANCE_TRIES` (6) times at `DANCE_SECS` (2) apiece before answering - so a
/// client waiting ten gives up while the service is still working and reports
/// [`Error::OutcomeUnknown`] about a request that was about to succeed.
///
/// **Why 30 and not 12.** The retry budget reads as 6 x 2 = 12 seconds. It is not: each of those
/// waits bounds itself with
///
/// ```ignore
/// let t0 = self.epoch_secs_monotonic();
/// if self.epoch_secs_monotonic() - t0 >= max_secs { return Timeout }
/// ```
///
/// and `epoch_secs_monotonic` returns WHOLE SECONDS. A "2 second" deadline therefore elapses when
/// the second COUNTER advances by two - anywhere between just over 1s and just under 3s of real
/// time, depending where in the second `t0` fell. The worst case per try is `max_secs + 1`:
///
/// ```text
/// 6 tries x (2 + 1) = up to 18 seconds
/// ```
///
/// which is why 10 and 15 both produced intermittent [`Error::OutcomeUnknown`] and 30 is stable.
///
/// **The general rule, which matters more than this constant: a deadline built from whole-second
/// differences carries up to +1s of slop, so N chained waits of S seconds bound at N x (S + 1).**
/// A budget computed as N x S is short, and being short turns a slow SUCCESS into an unknown
/// outcome - the one error that forbids the retry which would have fixed it.
///
/// That is the worst available false report: `OutcomeUnknown` tells a caller the operation MAY have
/// happened and must not be retried. A deadline shorter than the service's own bound does not bound
/// anything useful - it converts a slow success into an unknown outcome, which is strictly worse
/// than waiting.
pub const SOCKET_SECS: i64 = 30;

/// How long to wait for a TCP transaction through [`Net::tcp`].
///
/// A connect, a request and a response to a host that may be far away and may be slow. `services/
/// shell` used twenty seconds for exactly this before it moved onto this library, and the move to
/// [`NET_SECS`] halved it by accident - which nothing in QEMU is slow enough to notice, and a real
/// site on real hardware certainly is. Restored, and named so it cannot drift again.
pub const TCP_SECS: i64 = 20;

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
    /// Open a UDP socket and receive a CAPABILITY to it (7.10).
    ///
    /// The returned [`Socket`] is the authority to send through one source port. It is minted by
    /// `net-stack`, validated by the kernel on every use, and revoked when `net-stack` restarts -
    /// at which point operations return [`Error::Revoked`] and you open another.
    ///
    /// **Blocks** up to [`NET_SECS`]. **Authority:** the caller's existing `net-stack` capability;
    /// opening a socket grants nothing the contract did not already grant.
    ///
    /// # Errors
    /// - [`Error::Unavailable`] - `net-stack` would not open one. Usually no usable NIC.
    /// - [`Error::Failed`] - it answered without a capability.
    /// - Opening is idempotent from the caller's side, so a no-answer error is safe to retry -
    ///   though it may leave a socket behind in `net-stack`.
    pub fn socket<'n>(&'n mut self) -> Result<Socket<'n, 'a>, Error> {
        let ctx = self.ctx;
        let r = self.call(&[OP_OPEN_SOCKET], NET_SECS)?;
        if body(&r).first() != Some(&1) {
            return Err(Error::Unavailable);
        }
        // The capability rode the reply as an EMBEDDED cap; the kernel placed it in our table on
        // receipt and it is ours to claim or leak.
        let cap = ctx.take_pending_cap().ok_or(Error::Failed)?;
        Ok(Socket { net: self, ctx, cap, held: Held::new() })
    }

    /// The next tag, for the socket capability - which speaks to the SAME service on the SAME
    /// endpoint and must therefore draw from THIS counter rather than start a second one.
    pub(crate) fn next_tag_pub(&mut self) -> u8 {
        self.tag = TAG_BASE.wrapping_add(self.tag.wrapping_sub(TAG_BASE).wrapping_add(1) & 0x3F);
        self.tag
    }

    /// Listen on a TCP port and receive a CAPABILITY to the listener.
    ///
    /// The returned [`Listener`] is the authority to accept connections on one port, and nothing
    /// else. `net-stack` refuses a port already listened on, so two programs cannot silently share
    /// one.
    ///
    /// **Blocks** up to [`NET_SECS`]. **Authority:** the caller's existing `net-stack` capability.
    ///
    /// # Errors
    /// - [`Error::Unavailable`] - `net-stack` would not listen: the port is taken, the listener
    ///   table is full, or there is no usable NIC. Its log says which.
    /// - [`Error::Failed`] - it agreed and sent no capability.
    pub fn listen<'n>(&'n mut self, port: u16) -> Result<Listener<'n, 'a>, Error> {
        let ctx = self.ctx;
        let r = self.call(&[OP_LISTEN, (port >> 8) as u8, port as u8], NET_SECS)?;
        if body(&r).first() != Some(&1) {
            return Err(Error::Unavailable);
        }
        let cap = ctx.take_pending_cap().ok_or(Error::Failed)?;
        Ok(Listener { net: self, ctx, cap, held: Held::new(), closed: false })
    }

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
        let r = self.call(&req[..head + request.len()], TCP_SECS)?;
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

/// A UDP socket, held as a capability.
///
/// Minted by `net-stack` and validated by the kernel on every use, exactly as a file capability is
/// (7.10). Holding one is the authority to send through it and nothing else: it names one source
/// port, it cannot be widened, and it stops working the moment `net-stack` revokes it or dies.
///
/// # Why it borrows the [`Net`] handle
///
/// The same reason [`File`](crate::file::File) borrows its `Fs`: both speak to one service over one
/// endpoint and must share ONE correlation-tag counter. Two counters can mint the same tag for two
/// exchanges in flight, and the result is not a loud rejection but a stale reply silently accepted
/// as the current answer. The borrow makes that unspellable.
pub struct Socket<'n, 'a: 'n> {
    net: &'n mut Net<'a>,
    ctx: &'a ServiceContext,
    cap: CapHandle,
    held: Held,
}

impl<'n, 'a: 'n> Socket<'n, 'a> {
    /// Send a datagram and return what came back, in `buf`.
    ///
    /// Returns how many bytes of response landed in `buf`. **Zero means nothing answered** - which
    /// is an ordinary outcome for UDP, not an error, and is why this is `Ok(0)` rather than an
    /// `Err`. Nothing is retried on your behalf.
    ///
    /// **Blocks** up to [`SOCKET_SECS`], because the round trip happens inside `net-stack` - it
    /// sends, retries, and waits for the response before replying to us at all.
    ///
    /// **Authority:** this capability's `WRITE` right, checked by the KERNEL before `net-stack` is
    /// reached.
    ///
    /// # The ambiguity, stated
    ///
    /// `net-stack` answers "nothing came back" with a single zero byte, which is byte-identical to a
    /// genuine one-byte response of `0x00`. This reports both as `Ok(0)`. The protocol makes them the
    /// same bytes, so no reading of it can tell them apart; recording that is better than choosing
    /// one and being quietly wrong for the other. (`services/shell` reports the sentinel as one byte
    /// of DATA, which is how `sock` says "received 1 bytes back" about a query nobody answered.)
    ///
    /// # Errors
    /// - [`Error::PermissionDenied`] - this capability does not carry `WRITE`.
    /// - [`Error::Revoked`] - `net-stack` revoked it, or died and was replaced. Open a new one.
    /// - [`Error::BufferTooSmall`] - the response does not fit. **Nothing is retried**; the datagram
    ///   was already sent, so call again only if the peer tolerates a repeat.
    /// - **A send MUTATES the world.** On [`Error::OutcomeUnknown`] the datagram may have left; see
    ///   [`Error::retry_is_safe`], which says no.
    pub fn send_to(&mut self, ip: Ipv4, port: u16, data: &[u8], buf: &mut [u8]) -> Result<usize, Error> {
        let mut body = [0u8; 6 + 1024];
        if 6 + data.len() > body.len() {
            return Err(Error::InvalidInput);
        }
        body[..4].copy_from_slice(&ip.0);
        body[4] = (port >> 8) as u8;
        body[5] = port as u8;
        body[6..6 + data.len()].copy_from_slice(data);

        let tag = self.net.next_tag_pub();
        // The patience byte is THIS call's deadline, so the two cannot disagree - see `Net::call`,
        // which says the same thing about a named request.
        let m = resource::invoke(self.ctx, self.cap, RIGHT_WRITE, tag, Some(SOCKET_SECS as u8),
                                 &body[..6 + data.len()], SOCKET_SECS, &mut self.held)?;
        // `[tag, response..]` - the tag is verified and left in place, so the body starts at 1.
        let b = m.payload_bytes();
        let resp = if b.len() > 1 { &b[1..] } else { &[][..] };
        // The sentinel. See the ambiguity note above.
        if resp.len() <= 1 && resp.first().copied().unwrap_or(0) == 0 {
            return Ok(0);
        }
        if resp.len() > buf.len() {
            return Err(Error::BufferTooSmall);
        }
        buf[..resp.len()].copy_from_slice(resp);
        Ok(resp.len())
    }

    /// Take one message that arrived during an operation and was NOT the reply.
    ///
    /// Drain this in a loop after every operation if your task serves clients - they are real
    /// requests, held rather than dropped, and only you can answer them. A program that serves
    /// nobody always gets `None`.
    pub fn take_held(&mut self) -> Option<Message> {
        self.held.take()
    }
}

impl<'n, 'a: 'n> Drop for Socket<'n, 'a> {
    /// Drops our handle to the capability.
    ///
    /// **There is no close operation for a UDP socket** - `net-stack` exposes send and nothing else
    /// on this capability - so this releases the local cap-table slot and cannot tell the service
    /// anything. The socket is reclaimed when `net-stack` revokes it or restarts. Recorded rather
    /// than hidden: a reader comparing this with [`File`](crate::file::File), which does close, would
    /// otherwise assume an omission.
    fn drop(&mut self) {
        self.ctx.remove_cap(self.cap);
    }
}

/// A TCP listener, held as a capability.
///
/// Accepting hands over authority - each accepted connection is its own capability - so accepting
/// requires the listener's WRITE right. A read-only listener capability can be held and inspected
/// and cannot take connections.
pub struct Listener<'n, 'a: 'n> {
    net: &'n mut Net<'a>,
    ctx: &'a ServiceContext,
    cap: CapHandle,
    held: Held,
    closed: bool,
}

impl<'n, 'a: 'n> Listener<'n, 'a> {
    /// One invocation on a capability this listener owns, drawing from the one tag counter.
    ///
    /// The patience byte is this call's deadline. `accept` is the reason it has to be there: a server
    /// polling for a caller is a LONG wait on a held capability, and `net-stack` used to put such a
    /// request aside for a fixed 1500 ms and then throw it away while the client waited twenty
    /// seconds for an answer that no longer existed.
    fn call(&mut self, cap: CapHandle, right: u8, body_bytes: &[u8]) -> Result<Message, Error> {
        let tag = self.net.next_tag_pub();
        resource::invoke(self.ctx, cap, right, tag, Some(NET_SECS as u8), body_bytes, NET_SECS,
                         &mut self.held)
    }

    /// Take the next connection, if one is waiting.
    ///
    /// **`Ok(None)` means nobody has connected yet.** That is an ordinary poll result, not an error,
    /// so a server loops on it - and loops rather than blocking so it can still notice its own
    /// deadline, its operator, or a request on its endpoint.
    ///
    /// **Blocks** up to [`NET_SECS`] per call. **Authority:** this listener's `WRITE` right, because
    /// accepting hands over authority.
    ///
    /// # Errors
    /// [`Error::PermissionDenied`] if the capability lacks `WRITE`; [`Error::Revoked`] if
    /// `net-stack` restarted, in which case listen again - the port went with it.
    pub fn accept(&mut self) -> Result<Option<Conn<'_, 'n, 'a>>, Error> {
        let cap = self.cap;
        let r = self.call(cap, RIGHT_WRITE, &[LOP_ACCEPT])?;
        // `[tag, 1]` and an embedded capability, or `[tag, 0]` for nobody yet.
        if body(&r).first() != Some(&1) {
            return Ok(None);
        }
        let conn = self.ctx.take_pending_cap().ok_or(Error::Failed)?;
        Ok(Some(Conn { lis: self, cap: conn, closed: false }))
    }

    /// Stop listening and release the port.
    ///
    /// Consumes the listener. Dropping one also releases it, but a `Drop` cannot report a failure -
    /// and a port that was not released refuses the next `listen` on it, which is a confusing way to
    /// find out. Close explicitly where it matters.
    pub fn close(mut self) -> Result<(), Error> {
        self.close_inner()
    }

    fn close_inner(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let cap = self.cap;
        let r = self.call(cap, RIGHT_WRITE, &[LOP_CLOSE]).map(|_| ());
        self.ctx.remove_cap(cap);
        r
    }

    /// Take one message that arrived during an operation and was NOT the reply. See
    /// [`Socket::take_held`].
    pub fn take_held(&mut self) -> Option<Message> {
        self.held.take()
    }
}

impl<'n, 'a: 'n> Drop for Listener<'n, 'a> {
    fn drop(&mut self) {
        let _ = self.close_inner();
    }
}

/// One accepted TCP connection, held as a capability.
///
/// Borrows its [`Listener`], so one connection is open at a time and the tag counter has a single
/// owner from `Net` down. That is `serve`'s real model - accept one, answer it, close, accept the
/// next - now enforced rather than remembered.
pub struct Conn<'c, 'n: 'c, 'a: 'n> {
    lis: &'c mut Listener<'n, 'a>,
    cap: CapHandle,
    closed: bool,
}

impl<'c, 'n: 'c, 'a: 'n> Conn<'c, 'n, 'a> {
    /// Read whatever has arrived, into `buf`. Returns how many bytes.
    ///
    /// **Zero means nothing has arrived YET**, not end of stream - TCP delivers when it delivers, so
    /// a caller that wants more loops. Use [`is_closed`](Conn::is_closed) to tell "nothing yet" from
    /// "the peer has gone".
    ///
    /// **Authority:** this connection's `READ` right.
    pub fn recv(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let cap = self.cap;
        let r = self.lis.call(cap, RIGHT_READ, &[COP_RECV])?;
        let b = r.payload_bytes();
        let data = if b.len() > 1 { &b[1..] } else { &[][..] };
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    /// Send bytes. Returns how many were ACCEPTED, which may be fewer than offered.
    ///
    /// **A short send is reported, not hidden.** `net-stack`'s send arena is fixed, so a caller
    /// offering more than fits must know how much was taken or it loses the tail without being told
    /// (26.7). Send the remainder on a later call.
    ///
    /// **Authority:** this connection's `WRITE` right.
    pub fn send(&mut self, data: &[u8]) -> Result<usize, Error> {
        let mut req = [0u8; 1 + 1024];
        if 1 + data.len() > req.len() {
            return Err(Error::InvalidInput);
        }
        req[0] = COP_SEND;
        req[1..1 + data.len()].copy_from_slice(data);
        let cap = self.cap;
        let r = self.lis.call(cap, RIGHT_WRITE, &req[..1 + data.len()])?;
        let b = body(&r);
        if b.len() < 2 {
            return Err(Error::Malformed);
        }
        Ok(((b[1] as usize) << 8 | b[0] as usize).min(data.len()))
    }

    /// Whether [`close`](Conn::close) has already been called on this handle.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Close the connection.
    ///
    /// Consumes the handle. Dropping also closes, and cannot report the outcome.
    pub fn close(mut self) -> Result<(), Error> {
        self.close_inner()
    }

    fn close_inner(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let cap = self.cap;
        let r = self.lis.call(cap, RIGHT_WRITE, &[COP_CLOSE]).map(|_| ());
        self.lis.ctx.remove_cap(cap);
        r
    }
}

impl<'c, 'n: 'c, 'a: 'n> Drop for Conn<'c, 'n, 'a> {
    fn drop(&mut self) {
        let _ = self.close_inner();
    }
}
