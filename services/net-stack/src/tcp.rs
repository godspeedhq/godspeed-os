//! TCP over IPv4, bounded and heap-free.
//!
//! `docs/tcp-design.md` is the argument; this is the implementation. The two properties that shape
//! every line here:
//!
//!   * **No heap** (§26.6.1). A fixed connection table, fixed per-connection arenas, and a fixed
//!     number of out-of-order segments held. The maximum footprint is readable off the constants
//!     below and cannot grow at runtime.
//!   * **The advertised receive window IS the free space in the receive arena.** TCP's own flow
//!     control therefore enforces §26.6 rather than fighting it, and the peer is told the exact truth
//!     about what we can hold. Telling a peer the truth about capacity is Commandment VIII at the
//!     protocol level.
//!
//! Read per §26.14: the PROTOCOL's requirements are borrowed from RFC 793/1122/6298 and from reading
//! FreeBSD as an executable datasheet. Their MODEL is refused - no mbuf chains, no sockets as file
//! descriptors, no callback timers, no global state. A connection here is a delegated resource
//! capability (§7.10), exactly as a file is.

use godspeed_sdk::service_context::ServiceContext;

// ── Bounds. Every one of these is a hard ceiling, not a hint. ───────────────────────────────────

/// Connections held at once. Four rather than a larger round number because each costs its two
/// arenas below, and this table lives in `service_main`'s frame - a service stack is 256 KiB and a
/// debug ARM build has already overflowed one (see `feedback_arm_release_build`).
pub const MAX_CONNS: usize = 4;
/// Per-connection send arena: data handed to us that the peer has not acknowledged yet. A segment
/// cannot be dropped from here until its ACK arrives, because retransmission is reading from it.
pub const SND_BUF: usize = 2048;
/// Per-connection receive arena. Its FREE SPACE is what we advertise as the window, so this constant
/// is the largest amount of un-read data a peer can ever push at this machine.
pub const RCV_BUF: usize = 2048;
/// Out-of-order segments held while waiting for the gap to fill. Beyond this we DROP and let the peer
/// retransmit, which is always-legal TCP and is what keeps reassembly bounded. FreeBSD holds far
/// more; that is a property of their design, not of the protocol.
pub const MAX_OOO: usize = 4;
/// Maximum segment size we announce and honour. 1460 = 1500 MTU - 20 IP - 20 TCP, the Ethernet case.
pub const MSS: usize = 1460;
/// Retransmission bounds (RFC 6298 §2.4-2.5). The floor is 200 ms rather than the RFC's 1 s because
/// this is a LAN-and-QEMU stack and a one-second floor makes every lost segment feel like a hang;
/// the ceiling is the thing that actually matters for boundedness.
pub const RTO_MIN_MS: u64 = 200;
pub const RTO_MAX_MS: u64 = 8_000;
/// Retransmissions of one segment before the connection is declared dead. Bounded on purpose: a
/// retry loop with no limit is an unbounded wait wearing a disguise (§26.6).
pub const MAX_RETX: u8 = 6;
/// Frames drained and connections advanced per poll step. One busy connection must not starve the
/// serve path, so the poll step's cost has a ceiling like everything else.
pub const POLL_FRAMES: usize = 8;

// ── Wire constants ─────────────────────────────────────────────────────────────────────────────

pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;

const IP_PROTO_TCP: u8 = 6;
const ETH_LEN: usize = 14;
const IP_LEN: usize = 20;
const TCP_LEN: usize = 20;
/// Ethernet + IPv4 + TCP headers. Payload starts here.
pub const HDR: usize = ETH_LEN + IP_LEN + TCP_LEN;

// ── Sequence arithmetic ────────────────────────────────────────────────────────────────────────
//
// TCP sequence numbers WRAP, so they are compared by signed difference and never by `<`. Writing
// `a < b` on raw u32 works for 4 GiB and then silently inverts, which is the kind of bug that
// survives every test short of a long-lived connection.

#[inline]
pub fn seq_lt(a: u32, b: u32) -> bool { (a.wrapping_sub(b) as i32) < 0 }
#[inline]
pub fn seq_le(a: u32, b: u32) -> bool { (a.wrapping_sub(b) as i32) <= 0 }
#[inline]
pub fn seq_gt(a: u32, b: u32) -> bool { (a.wrapping_sub(b) as i32) > 0 }

// ── States (RFC 793 figure 6) ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Closed,
    SynSent,
    Established,
    /// We sent FIN, waiting for its ACK and for the peer's FIN.
    FinWait1,
    /// Our FIN is acknowledged; waiting for the peer's FIN.
    FinWait2,
    /// Peer sent FIN first; the application may still send (half-close).
    CloseWait,
    /// We sent our FIN after the peer's; waiting for its ACK.
    LastAck,
    /// Both FINs exchanged, waiting out the quiet time so a delayed duplicate cannot be mistaken for
    /// a new connection's data.
    TimeWait,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Closed => "closed", State::SynSent => "syn-sent",
            State::Established => "established", State::FinWait1 => "fin-wait-1",
            State::FinWait2 => "fin-wait-2", State::CloseWait => "close-wait",
            State::LastAck => "last-ack", State::TimeWait => "time-wait",
        }
    }
}

/// How a connection ended, so a client learns WHY rather than only that it is gone (§26.7 - a
/// failure that is reported is worth more than one that is merely detected).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    None,
    /// The peer refused, or reset an established connection.
    Reset,
    /// `MAX_RETX` retransmissions of the same segment went unacknowledged.
    RetxExhausted,
    /// The handshake did not complete inside its budget.
    ConnectTimeout,
}

// ── A held out-of-order segment ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Ooo {
    seq: u32,
    len: u16,
    used: bool,
    data: [u8; MSS],
}

impl Ooo {
    const fn empty() -> Self { Ooo { seq: 0, len: 0, used: false, data: [0u8; MSS] } }
}

// ── A connection ───────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct Conn {
    pub state: State,
    pub fault: Fault,
    /// The resource id of the capability this connection was minted as, so a badged invocation finds
    /// it. 0 when the slot is free.
    pub rid: u64,

    pub local_port: u16,
    pub remote_ip: [u8; 4],
    pub remote_port: u16,

    // Send sequence space (RFC 793 §3.2)
    /// Oldest unacknowledged byte.
    pub snd_una: u32,
    /// Next byte we will send.
    pub snd_nxt: u32,
    /// The peer's advertised window: how much it will accept beyond `snd_una`.
    pub snd_wnd: u16,
    /// Our initial send sequence, kept so `bytes_sent` can be reported without exposing raw seqs.
    iss: u32,

    // Receive sequence space
    /// Next byte we expect from the peer. This is what we ACK.
    pub rcv_nxt: u32,

    /// Unacknowledged outbound data. `snd_buf[..snd_len]` corresponds to sequence numbers
    /// `snd_una .. snd_una + snd_len`.
    snd_buf: [u8; SND_BUF],
    snd_len: usize,

    /// Received, in-order, not yet read by the client.
    rcv_buf: [u8; RCV_BUF],
    rcv_len: usize,

    ooo: [Ooo; MAX_OOO],

    // Timers, all in milliseconds of monotonic service time.
    /// When the oldest unacknowledged segment must be resent. 0 = no timer armed.
    retx_at_ms: u64,
    pub retx_count: u8,
    /// Smoothed RTT and its variation (RFC 6298). 0 srtt = no sample taken yet.
    srtt_ms: u64,
    rttvar_ms: u64,
    pub rto_ms: u64,
    /// When the segment currently being timed was sent, and which sequence it ends at. Karn's
    /// algorithm: a RETRANSMITTED segment is never used for an RTT sample, because we cannot tell
    /// which copy the ACK is for.
    rtt_timed_seq: u32,
    rtt_timed_at_ms: u64,
    rtt_timing: bool,
    /// Deadline for a state that must not last forever (handshake, TIME_WAIT).
    state_deadline_ms: u64,
    /// Set when the client has asked to close and the send buffer must drain first.
    close_pending: bool,
}

impl Conn {
    pub const fn free() -> Self {
        Conn {
            state: State::Closed, fault: Fault::None, rid: 0,
            local_port: 0, remote_ip: [0; 4], remote_port: 0,
            snd_una: 0, snd_nxt: 0, snd_wnd: 0, iss: 0, rcv_nxt: 0,
            snd_buf: [0u8; SND_BUF], snd_len: 0,
            rcv_buf: [0u8; RCV_BUF], rcv_len: 0,
            ooo: [Ooo::empty(); MAX_OOO],
            retx_at_ms: 0, retx_count: 0,
            srtt_ms: 0, rttvar_ms: 0, rto_ms: RTO_MIN_MS,
            rtt_timed_seq: 0, rtt_timed_at_ms: 0, rtt_timing: false,
            state_deadline_ms: 0, close_pending: false,
        }
    }

    pub fn in_use(&self) -> bool { self.state != State::Closed || self.rid != 0 }

    /// Free space in the receive arena. THIS IS THE ADVERTISED WINDOW - see the module header.
    pub fn window(&self) -> u16 { (RCV_BUF - self.rcv_len) as u16 }

    /// Bytes the client has sent that the peer has not acknowledged.
    pub fn unacked(&self) -> usize { self.snd_len }
    /// Bytes received and waiting to be read.
    pub fn readable(&self) -> usize { self.rcv_len }
    /// Total payload bytes handed to the peer and acknowledged.
    pub fn bytes_acked(&self) -> u32 { self.snd_una.wrapping_sub(self.iss).saturating_sub(1) }

    /// RFC 6298 §2: fold one RTT sample into the smoothed estimate and recompute the RTO.
    fn rtt_sample(&mut self, r_ms: u64) {
        if self.srtt_ms == 0 {
            self.srtt_ms = r_ms;
            self.rttvar_ms = r_ms / 2;
        } else {
            let diff = if self.srtt_ms > r_ms { self.srtt_ms - r_ms } else { r_ms - self.srtt_ms };
            // rttvar = 3/4 rttvar + 1/4 |srtt - r|   ;   srtt = 7/8 srtt + 1/8 r
            self.rttvar_ms = (3 * self.rttvar_ms + diff) / 4;
            self.srtt_ms = (7 * self.srtt_ms + r_ms) / 8;
        }
        self.rto_ms = (self.srtt_ms + 4 * self.rttvar_ms).clamp(RTO_MIN_MS, RTO_MAX_MS);
    }
}

// ── A parsed segment ───────────────────────────────────────────────────────────────────────────

pub struct Seg<'a> {
    pub src_ip: [u8; 4],
    pub dst_ip: [u8; 4],
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub wnd: u16,
    pub payload: &'a [u8],
}

fn be16(b: &[u8]) -> u16 { ((b[0] as u16) << 8) | b[1] as u16 }
fn be32(b: &[u8]) -> u32 {
    ((b[0] as u32) << 24) | ((b[1] as u32) << 16) | ((b[2] as u32) << 8) | b[3] as u32
}

/// Parse an Ethernet frame as IPv4/TCP, or `None` if it is anything else.
///
/// Every length is checked against the frame actually received rather than against the header's own
/// claim, because the header is written by whoever is on the other end of the cable. A stack that
/// trusts a remote length field is one malformed packet away from reading someone else's memory.
pub fn parse(f: &[u8]) -> Option<Seg<'_>> {
    if f.len() < HDR { return None; }
    if f[12] != 0x08 || f[13] != 0x00 { return None; }          // not IPv4
    let ihl = ((f[ETH_LEN] & 0x0f) as usize) * 4;
    if ihl < IP_LEN || f.len() < ETH_LEN + ihl + TCP_LEN { return None; }
    if f[ETH_LEN + 9] != IP_PROTO_TCP { return None; }

    let ip_total = be16(&f[ETH_LEN + 2..ETH_LEN + 4]) as usize;
    // The IP total length may be SHORTER than the frame (Ethernet pads to 60 bytes), and a hostile
    // one may claim to be longer. Take the smaller, always.
    let ip_end = (ETH_LEN + ip_total).min(f.len());
    let t = ETH_LEN + ihl;
    if ip_end < t + TCP_LEN { return None; }

    let doff = ((f[t + 12] >> 4) as usize) * 4;
    if doff < TCP_LEN || t + doff > ip_end { return None; }

    let mut src_ip = [0u8; 4]; src_ip.copy_from_slice(&f[ETH_LEN + 12..ETH_LEN + 16]);
    let mut dst_ip = [0u8; 4]; dst_ip.copy_from_slice(&f[ETH_LEN + 16..ETH_LEN + 20]);

    Some(Seg {
        src_ip, dst_ip,
        src_port: be16(&f[t..t + 2]),
        dst_port: be16(&f[t + 2..t + 4]),
        seq: be32(&f[t + 4..t + 8]),
        ack: be32(&f[t + 8..t + 12]),
        flags: f[t + 13],
        wnd: be16(&f[t + 14..t + 16]),
        payload: &f[t + doff..ip_end],
    })
}

/// The TCP checksum: one's-complement sum over the pseudo-header, the TCP header and the payload
/// (RFC 793 §3.1). The pseudo-header is why this cannot reuse the plain IP `checksum` in `main.rs`:
/// it covers addresses that are not inside the segment, which is precisely what makes a TCP segment
/// undeliverable to the wrong host even if its own header survives intact.
fn tcp_checksum(src: &[u8; 4], dst: &[u8; 4], tcp: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut add16 = |v: u16| { sum = sum.wrapping_add(v as u32); };

    add16(be16(&src[0..2])); add16(be16(&src[2..4]));
    add16(be16(&dst[0..2])); add16(be16(&dst[2..4]));
    add16(IP_PROTO_TCP as u16);
    add16(tcp.len() as u16);

    let mut i = 0;
    while i + 1 < tcp.len() { add16(be16(&tcp[i..i + 2])); i += 2; }
    if i < tcp.len() { add16((tcp[i] as u16) << 8); }            // odd tail, padded with zero

    while (sum >> 16) != 0 { sum = (sum & 0xffff) + (sum >> 16); }
    !(sum as u16)
}

/// Build one Ethernet/IPv4/TCP frame into `out`, returning its length.
///
/// Returns 0 rather than panicking if the payload cannot fit, so a caller that miscounts loses a
/// segment instead of taking down the service (Commandment V: nothing above the kernel halts the
/// machine, including by panic).
#[allow(clippy::too_many_arguments)]
pub fn emit(out: &mut [u8], gw_mac: &[u8; 6], our_mac: &[u8; 6], our_ip: &[u8; 4],
            dst_ip: &[u8; 4], src_port: u16, dst_port: u16,
            seq: u32, ack: u32, flags: u8, wnd: u16, payload: &[u8]) -> usize {
    let total = HDR + payload.len();
    if total > out.len() || payload.len() > MSS { return 0; }
    for b in out[..total].iter_mut() { *b = 0; }

    out[0..6].copy_from_slice(gw_mac);
    out[6..12].copy_from_slice(our_mac);
    out[12] = 0x08; out[13] = 0x00;

    let ip = ETH_LEN;
    out[ip] = 0x45;                                              // IPv4, 20-byte header
    let ip_total = (IP_LEN + TCP_LEN + payload.len()) as u16;
    out[ip + 2] = (ip_total >> 8) as u8; out[ip + 3] = ip_total as u8;
    out[ip + 6] = 0x40;                                          // don't fragment
    out[ip + 8] = 64;                                            // TTL
    out[ip + 9] = IP_PROTO_TCP;
    out[ip + 12..ip + 16].copy_from_slice(our_ip);
    out[ip + 16..ip + 20].copy_from_slice(dst_ip);
    let ipck = super::checksum(&out[ip..ip + IP_LEN]);
    out[ip + 10] = (ipck >> 8) as u8; out[ip + 11] = ipck as u8;

    let t = ip + IP_LEN;
    out[t] = (src_port >> 8) as u8; out[t + 1] = src_port as u8;
    out[t + 2] = (dst_port >> 8) as u8; out[t + 3] = dst_port as u8;
    out[t + 4] = (seq >> 24) as u8; out[t + 5] = (seq >> 16) as u8;
    out[t + 6] = (seq >> 8) as u8;  out[t + 7] = seq as u8;
    out[t + 8] = (ack >> 24) as u8; out[t + 9] = (ack >> 16) as u8;
    out[t + 10] = (ack >> 8) as u8; out[t + 11] = ack as u8;
    out[t + 12] = 0x50;                                          // data offset 5 words, no options
    out[t + 13] = flags;
    out[t + 14] = (wnd >> 8) as u8; out[t + 15] = wnd as u8;
    out[t + 20..total].copy_from_slice(payload);

    let ck = tcp_checksum(our_ip, dst_ip, &out[t..total]);
    out[t + 16] = (ck >> 8) as u8; out[t + 17] = ck as u8;
    total
}

// ── The connection table and the state machine ─────────────────────────────────────────────────

/// Everything TCP owns. Passed by `&mut` from `service_main` rather than living in a static, because
/// a service may hold no unowned global mutable state (Commandment VI, and `VI-static-mut` enforces
/// it). That also makes the footprint honest: this struct IS the memory cost of TCP here.
pub struct Tcp {
    pub conns: [Conn; MAX_CONNS],
    /// Ephemeral port allocator. Starts high to stay clear of anything well known.
    next_port: u16,
    /// Cycles per millisecond, or 0 when the clock is not calibrated. `backlog/27`: a deadline built
    /// from an uncalibrated clock collapses to now, so this is read ONCE, explicitly, and its absence
    /// is reported rather than silently turned into a plausible number.
    cyc_per_ms: u64,
    /// Monotonic base, so `now_ms` counts from service start and cannot be confused with wall time.
    base_tsc: u64,
    /// Said once, when a clock is missing, so the operator learns why timers are inert.
    warned_no_clock: bool,
}

impl Tcp {
    pub fn new(tsc_hz: u64, now_tsc: u64) -> Self {
        Tcp {
            conns: [Conn::free(); MAX_CONNS],
            next_port: 49152,
            cyc_per_ms: tsc_hz / 1000,
            base_tsc: now_tsc,
            warned_no_clock: false,
        }
    }

    /// Milliseconds since this table was created, or `None` when there is no usable clock.
    ///
    /// `None` rather than 0: a zero here would make every deadline appear permanently due, which is
    /// the same silent-plausible-value defect `backlog/27` records one layer down.
    pub fn now_ms(&self, ctx: &ServiceContext) -> Option<u64> {
        if self.cyc_per_ms == 0 { return None; }
        Some(ctx.read_tsc().wrapping_sub(self.base_tsc) / self.cyc_per_ms)
    }

    pub fn have_clock(&self) -> bool { self.cyc_per_ms != 0 }

    /// Any connection not closed. The serve loop uses this to decide whether it may block in
    /// `recv()` (nothing to poll) or must use a bounded wait - so TCP costs nothing at all when it
    /// is not in use.
    pub fn active(&self) -> bool { self.conns.iter().any(|c| c.state != State::Closed) }

    pub fn by_rid(&mut self, rid: u64) -> Option<&mut Conn> {
        self.conns.iter_mut().find(|c| c.rid == rid && c.rid != 0)
    }

    fn free_slot(&mut self) -> Option<usize> {
        self.conns.iter().position(|c| !c.in_use())
    }

    fn find(&mut self, lport: u16, rip: &[u8; 4], rport: u16) -> Option<&mut Conn> {
        self.conns.iter_mut().find(|c| {
            c.state != State::Closed && c.local_port == lport
                && c.remote_port == rport && &c.remote_ip == rip
        })
    }

    /// Start an active open. Returns the slot index, or `None` when the table is full.
    ///
    /// The initial sequence number is derived from the clock rather than fixed. RFC 793 wants it
    /// unpredictable so a delayed segment from an old incarnation of the same connection cannot be
    /// accepted as current; a constant ISS makes that failure reachable on a machine that reboots
    /// fast, which this one does.
    pub fn connect(&mut self, ctx: &ServiceContext, rid: u64, dst: [u8; 4], dport: u16)
                   -> Option<usize> {
        let iss = (ctx.read_tsc() as u32) ^ 0x5a5a_0000;
        let port = self.next_port;
        self.next_port = if self.next_port >= 65000 { 49152 } else { self.next_port + 1 };
        let now = self.now_ms(ctx).unwrap_or(0);
        let i = self.free_slot()?;
        let c = &mut self.conns[i];
        *c = Conn::free();
        c.rid = rid;
        c.state = State::SynSent;
        c.local_port = port;
        c.remote_ip = dst;
        c.remote_port = dport;
        c.iss = iss;
        c.snd_una = iss;
        c.snd_nxt = iss.wrapping_add(1);          // SYN consumes one sequence number
        c.rto_ms = RTO_MIN_MS;
        c.retx_at_ms = now + RTO_MIN_MS;
        c.state_deadline_ms = now + 10_000;       // a handshake that never completes must end
        Some(i)
    }

    /// Queue application data on a connection. Returns how many bytes were accepted, which may be
    /// fewer than offered: the send arena is fixed, and short-writing is how a fixed arena tells the
    /// truth instead of dropping the tail silently.
    pub fn write(&mut self, rid: u64, data: &[u8]) -> usize {
        let c = match self.by_rid(rid) { Some(c) => c, None => return 0 };
        if !matches!(c.state, State::Established | State::CloseWait) { return 0; }
        let room = SND_BUF - c.snd_len;
        let n = data.len().min(room);
        c.snd_buf[c.snd_len..c.snd_len + n].copy_from_slice(&data[..n]);
        c.snd_len += n;
        n
    }

    /// Take up to `out.len()` bytes of received data. Returns how many were copied.
    pub fn read(&mut self, rid: u64, out: &mut [u8]) -> usize {
        let c = match self.by_rid(rid) { Some(c) => c, None => return 0 };
        let n = c.rcv_len.min(out.len());
        out[..n].copy_from_slice(&c.rcv_buf[..n]);
        c.rcv_buf.copy_within(n..c.rcv_len, 0);
        c.rcv_len -= n;
        n
    }

    /// Ask for an orderly close. The FIN is not sent until the send arena has drained, so data
    /// already accepted from the client is not discarded by the act of closing.
    pub fn close(&mut self, rid: u64) {
        if let Some(c) = self.by_rid(rid) {
            if c.state == State::Established || c.state == State::CloseWait {
                c.close_pending = true;
            }
        }
    }

    /// Release a slot outright: the capability was revoked or the client vanished.
    pub fn forget(&mut self, rid: u64) {
        if let Some(c) = self.by_rid(rid) { *c = Conn::free(); }
    }
}

// ── Receiving ──────────────────────────────────────────────────────────────────────────────────

/// Append in-order payload and then drain anything the gap was blocking.
fn deliver(c: &mut Conn, seq: u32, data: &[u8]) {
    // A retransmission may overlap what we already hold; take only the new tail.
    let skip = c.rcv_nxt.wrapping_sub(seq) as usize;
    if skip >= data.len() { return; }
    let fresh = &data[skip..];
    let room = RCV_BUF - c.rcv_len;
    let n = fresh.len().min(room);
    c.rcv_buf[c.rcv_len..c.rcv_len + n].copy_from_slice(&fresh[..n]);
    c.rcv_len += n;
    c.rcv_nxt = c.rcv_nxt.wrapping_add(n as u32);

    // Now that rcv_nxt moved, a held segment may have become the next one. Repeat until nothing
    // fits: a bounded loop, because MAX_OOO is fixed and each pass consumes at least one slot.
    let mut progress = true;
    while progress {
        progress = false;
        for k in 0..MAX_OOO {
            if !c.ooo[k].used { continue; }
            let s = c.ooo[k].seq;
            let l = c.ooo[k].len as usize;
            if seq_le(s, c.rcv_nxt) && seq_gt(s.wrapping_add(l as u32), c.rcv_nxt) {
                let off = c.rcv_nxt.wrapping_sub(s) as usize;
                let room = RCV_BUF - c.rcv_len;
                let n = (l - off).min(room);
                // Copy through a temporary: `c.ooo[k].data` and `c.rcv_buf` are both fields of `c`,
                // so a direct slice-to-slice copy would need two simultaneous borrows.
                let mut tmp = [0u8; MSS];
                tmp[..n].copy_from_slice(&c.ooo[k].data[off..off + n]);
                c.rcv_buf[c.rcv_len..c.rcv_len + n].copy_from_slice(&tmp[..n]);
                c.rcv_len += n;
                c.rcv_nxt = c.rcv_nxt.wrapping_add(n as u32);
                c.ooo[k].used = false;
                progress = true;
            } else if seq_le(s.wrapping_add(l as u32), c.rcv_nxt) {
                c.ooo[k].used = false;               // wholly stale now
                progress = true;
            }
        }
    }
}

/// Hold an out-of-order segment, or drop it if there is no room.
///
/// Dropping is not a failure here: the peer will retransmit, and refusing to grow is the whole
/// point (§26.6.1). What would be a failure is pretending to hold it.
fn hold_ooo(c: &mut Conn, seq: u32, data: &[u8]) {
    if data.is_empty() || data.len() > MSS { return; }
    for k in 0..MAX_OOO {
        if c.ooo[k].used && c.ooo[k].seq == seq { return; }   // already have this one
    }
    if let Some(k) = (0..MAX_OOO).find(|&k| !c.ooo[k].used) {
        c.ooo[k].seq = seq;
        c.ooo[k].len = data.len() as u16;
        c.ooo[k].data[..data.len()].copy_from_slice(data);
        c.ooo[k].used = true;
    }
}

// ── This machine's identity on the wire ────────────────────────────────────────────────────────

/// Where frames come from and go. Passed in rather than stored, because `net-stack` re-runs its
/// configuration dance on a cable event and a cached copy here would be a second truth that drifts
/// (Commandment III).
#[derive(Clone, Copy)]
pub struct Net {
    pub our_mac: [u8; 6],
    pub gw_mac: [u8; 6],
    pub our_ip: [u8; 4],
}

impl Tcp {
    /// Handle one inbound frame. Returns the length of a frame to transmit in `out`, or 0.
    ///
    /// Exactly one segment may be emitted per inbound segment, and it is always an ACK (or a RST).
    /// DATA is never sent from here - that is `poll_one`'s job - which keeps "react to the peer" and
    /// "make our own progress" separable, and means a flood of inbound segments cannot make this
    /// function do unbounded work.
    pub fn on_frame(&mut self, ctx: &ServiceContext, net: &Net, f: &[u8], out: &mut [u8]) -> usize {
        let seg = match parse(f) { Some(s) => s, None => return 0 };
        if seg.dst_ip != net.our_ip { return 0; }
        let now = self.now_ms(ctx).unwrap_or(0);

        let (lp, rip, rp) = (seg.dst_port, seg.src_ip, seg.src_port);
        let c = match self.find(lp, &rip, rp) { Some(c) => c, None => return 0 };

        // A RST ends the connection, and the reason is kept. Anything else about this segment is
        // moot once the peer has refused.
        if seg.flags & RST != 0 {
            c.fault = Fault::Reset;
            c.state = State::Closed;
            return 0;
        }

        let mut need_ack = false;

        if c.state == State::SynSent {
            if seg.flags & SYN != 0 && seg.flags & ACK != 0 {
                if seg.ack != c.snd_nxt {
                    // Acknowledging something we never sent. Refuse the connection rather than
                    // adopt its sequence space.
                    return emit(out, &net.gw_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                                c.local_port, c.remote_port, seg.ack, 0, RST, 0, &[]);
                }
                c.snd_una = seg.ack;
                c.rcv_nxt = seg.seq.wrapping_add(1);
                c.snd_wnd = seg.wnd;
                c.state = State::Established;
                c.retx_at_ms = 0;
                c.retx_count = 0;
                c.state_deadline_ms = 0;
                if c.rtt_timing && now >= c.rtt_timed_at_ms {
                    let r = now - c.rtt_timed_at_ms;
                    c.rtt_sample(r);
                    c.rtt_timing = false;
                }
                need_ack = true;
            }
            // A bare SYN here would be a simultaneous open. Not supported, and saying so is better
            // than half-handling it: the peer's retransmitted SYN will find us still in SynSent and
            // our own SYN retransmission continues, so the connection fails on its deadline rather
            // than entering a state this stack does not implement.
        } else if c.state != State::Closed {
            // ---- acknowledgement ----
            if seg.flags & ACK != 0 {
                if seq_gt(seg.ack, c.snd_una) && seq_le(seg.ack, c.snd_nxt) {
                    let acked = seg.ack.wrapping_sub(c.snd_una) as usize;
                    // `acked` may include the phantom byte of a SYN or FIN, which is not in the
                    // arena. Only real data is dropped from it.
                    let data_acked = acked.min(c.snd_len);
                    if data_acked > 0 {
                        c.snd_buf.copy_within(data_acked..c.snd_len, 0);
                        c.snd_len -= data_acked;
                    }
                    c.snd_una = seg.ack;
                    c.retx_count = 0;

                    // KARN'S ALGORITHM: a sample is only valid if the segment being timed was never
                    // retransmitted. `rtt_timing` is cleared on every retransmission, so reaching
                    // here with it still set means this ACK unambiguously belongs to one send.
                    if c.rtt_timing && seq_le(c.rtt_timed_seq, seg.ack) && now >= c.rtt_timed_at_ms {
                        let r = now - c.rtt_timed_at_ms;
                        c.rtt_sample(r);
                        c.rtt_timing = false;
                    }

                    c.retx_at_ms = if c.snd_una == c.snd_nxt { 0 } else { now + c.rto_ms };

                    if c.state == State::FinWait1 && c.snd_una == c.snd_nxt {
                        c.state = State::FinWait2;
                    } else if c.state == State::LastAck && c.snd_una == c.snd_nxt {
                        c.state = State::Closed;
                        return 0;
                    }
                }
                c.snd_wnd = seg.wnd;
            }

            // ---- data ----
            if !seg.payload.is_empty() && c.state != State::TimeWait {
                if seg.seq == c.rcv_nxt {
                    deliver(c, seg.seq, seg.payload);
                } else if seq_gt(seg.seq, c.rcv_nxt)
                    && seq_lt(seg.seq, c.rcv_nxt.wrapping_add(RCV_BUF as u32)) {
                    hold_ooo(c, seg.seq, seg.payload);
                }
                // A duplicate below rcv_nxt is dropped, but still acknowledged: the peer is
                // retransmitting because it did not hear us, and staying silent repeats that.
                need_ack = true;
            }

            // ---- FIN, only when it is the next thing in sequence ----
            let fin_seq = seg.seq.wrapping_add(seg.payload.len() as u32);
            if seg.flags & FIN != 0 && fin_seq == c.rcv_nxt {
                c.rcv_nxt = c.rcv_nxt.wrapping_add(1);
                c.state = match c.state {
                    State::Established => State::CloseWait,
                    // Simultaneous close. Treated as TIME_WAIT rather than adding a CLOSING state:
                    // our FIN is either already acknowledged or will be retransmitted, and the quiet
                    // period is the part that matters. Recorded as a deliberate narrowing (§26.14).
                    State::FinWait1 | State::FinWait2 => {
                        c.state_deadline_ms = now + 2_000;
                        State::TimeWait
                    }
                    s => s,
                };
                need_ack = true;
            }
        }

        if need_ack {
            let w = c.window();
            return emit(out, &net.gw_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                        c.local_port, c.remote_port, c.snd_nxt, c.rcv_nxt, ACK, w, &[]);
        }
        0
    }

    /// Advance connection `i` by one step: expire a deadline, retransmit if due, or send whatever the
    /// peer's window allows. Returns a frame length in `out`, or 0.
    ///
    /// ONE frame per call, deliberately. The caller loops a bounded number of times, so a connection
    /// with a full send arena cannot hold the poll step for an unbounded stretch.
    pub fn poll_one(&mut self, ctx: &ServiceContext, net: &Net, i: usize, out: &mut [u8]) -> usize {
        let now = match self.now_ms(ctx) { Some(n) => n, None => 0 };
        let has_clock = self.have_clock();
        let c = &mut self.conns[i];
        if c.state == State::Closed { return 0; }

        // ---- deadlines ----
        if has_clock && c.state_deadline_ms != 0 && now >= c.state_deadline_ms {
            match c.state {
                State::SynSent => { c.fault = Fault::ConnectTimeout; c.state = State::Closed; return 0; }
                State::TimeWait => { c.state = State::Closed; return 0; }
                _ => c.state_deadline_ms = 0,
            }
        }

        // ---- retransmission ----
        if has_clock && c.retx_at_ms != 0 && now >= c.retx_at_ms {
            c.retx_count += 1;
            if c.retx_count > MAX_RETX {
                c.fault = Fault::RetxExhausted;
                c.state = State::Closed;
                return 0;
            }
            // Exponential backoff, clamped. The clamp is what makes this bounded rather than
            // merely finite (§26.6).
            c.rto_ms = (c.rto_ms * 2).min(RTO_MAX_MS);
            c.retx_at_ms = now + c.rto_ms;
            c.rtt_timing = false;                         // Karn: this ACK cannot be timed

            if c.state == State::SynSent {
                return emit(out, &net.gw_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                            c.local_port, c.remote_port, c.iss, 0, SYN, RCV_BUF as u16, &[]);
            }
            if c.snd_len > 0 {
                let n = c.snd_len.min(MSS);
                let mut tmp = [0u8; MSS];
                tmp[..n].copy_from_slice(&c.snd_buf[..n]);
                let w = c.window();
                return emit(out, &net.gw_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                            c.local_port, c.remote_port, c.snd_una, c.rcv_nxt,
                            ACK | PSH, w, &tmp[..n]);
            }
            // Nothing buffered, so the unacknowledged thing is our FIN.
            if matches!(c.state, State::FinWait1 | State::LastAck) {
                let w = c.window();
                return emit(out, &net.gw_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                            c.local_port, c.remote_port, c.snd_nxt.wrapping_sub(1), c.rcv_nxt,
                            ACK | FIN, w, &[]);
            }
            return 0;
        }

        if !matches!(c.state, State::Established | State::CloseWait) { return 0; }

        // ---- new data, within the peer's window ----
        let sent_off = c.snd_nxt.wrapping_sub(c.snd_una) as usize;
        if sent_off < c.snd_len {
            let inflight = sent_off;
            let allowed = (c.snd_wnd as usize).saturating_sub(inflight);
            if allowed > 0 {
                let n = (c.snd_len - sent_off).min(MSS).min(allowed);
                if n > 0 {
                    let mut tmp = [0u8; MSS];
                    tmp[..n].copy_from_slice(&c.snd_buf[sent_off..sent_off + n]);
                    let seq = c.snd_nxt;
                    c.snd_nxt = c.snd_nxt.wrapping_add(n as u32);
                    if !c.rtt_timing {
                        c.rtt_timing = true;
                        c.rtt_timed_seq = c.snd_nxt;
                        c.rtt_timed_at_ms = now;
                    }
                    if c.retx_at_ms == 0 { c.retx_at_ms = now + c.rto_ms; }
                    let w = c.window();
                    return emit(out, &net.gw_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                                c.local_port, c.remote_port, seq, c.rcv_nxt, ACK | PSH, w, &tmp[..n]);
                }
            }
            // The peer's window is shut. A real stack probes it here so a lost window update cannot
            // deadlock the connection; this one does not yet, and the retransmit timer is what stops
            // it hanging forever. Recorded rather than implied (§26.7).
            return 0;
        }

        // ---- close, only once everything accepted from the client has been sent ----
        if c.close_pending && sent_off == c.snd_len {
            c.close_pending = false;
            let seq = c.snd_nxt;
            c.snd_nxt = c.snd_nxt.wrapping_add(1);
            c.state = if c.state == State::CloseWait { State::LastAck } else { State::FinWait1 };
            if c.retx_at_ms == 0 { c.retx_at_ms = now + c.rto_ms; }
            let w = c.window();
            return emit(out, &net.gw_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                        c.local_port, c.remote_port, seq, c.rcv_nxt, ACK | FIN, w, &[]);
        }
        0
    }

    /// The SYN that opens a connection. Separate from `poll_one` because the first transmission is
    /// not a retransmission and must start the RTT timer.
    pub fn syn_frame(&mut self, ctx: &ServiceContext, net: &Net, i: usize, out: &mut [u8]) -> usize {
        let now = self.now_ms(ctx).unwrap_or(0);
        let c = &mut self.conns[i];
        c.rtt_timing = true;
        c.rtt_timed_seq = c.snd_nxt;
        c.rtt_timed_at_ms = now;
        emit(out, &net.gw_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
             c.local_port, c.remote_port, c.iss, 0, SYN, RCV_BUF as u16, &[])
    }

    /// Say once, loudly, that timers are inert. `backlog/27`: without a calibrated clock a deadline
    /// collapses to now, so rather than pretend to a retransmission schedule this reports the
    /// limitation and keeps going with what still works (in-order delivery driven by the peer).
    pub fn warn_if_no_clock(&mut self, ctx: &ServiceContext) {
        if self.cyc_per_ms == 0 && !self.warned_no_clock {
            self.warned_no_clock = true;
            ctx.log("net-stack: tcp - NO CALIBRATED CLOCK, so retransmission and connect timeouts \
                     are INERT on this port (backlog/27). Connections still work while the peer is \
                     answering; a lost segment will not be resent.");
        }
    }
}
