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

/// Connections held at once. TWO, not a larger round number, because the transaction model uses
/// exactly one at a time and a table sized for concurrency that does not exist yet is the
/// speculative abstraction §26.2 forbids. Each connection costs its two arenas plus its held
/// out-of-order segments, and this table lives in `service_main`'s frame: on the Pi 2 that stack is
/// 256 KiB, a debug ARM build has already overflowed one, and four connections put this service's
/// entry frame at 98 KiB. Raise it when concurrent connections exist to need it.
pub const MAX_CONNS: usize = 2;
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

/// What a peer that sent us NO maximum-segment-size option is assumed to accept (RFC 1122 §4.2.2.6).
///
/// Deliberately the conservative standard value rather than something convenient. A stack that
/// assumes 1460 from a peer that never said so is guessing about a path it cannot see, and the cost
/// of being wrong is silent: segments too large for some link in the middle are dropped, and the
/// connection stalls on retransmissions that will never succeed. Every peer worth talking to sends
/// the option, so this is the floor for the ones that do not.
pub const MSS_DEFAULT: u16 = 536;

/// The MSS option itself: kind 2, length 4, then the value. Four bytes, which is exactly one 32-bit
/// word, so it needs no padding and moves the data offset from 5 to 6.
const OPT_MSS_LEN: usize = 4;

/// Congestion control, RFC 5681.
///
/// Before this, sending was bounded only by the PEER'S advertised window - which says what the peer
/// can buffer, and nothing at all about what the path between us can carry. A stack with only that
/// bound answers its first window by putting the whole thing on the wire at once, and on a link that
/// cannot take it the result is loss, then a retransmission burst of the same size. The window is the
/// receiver's limit; the congestion window is the network's, and a correct stack respects both.
///
/// Slow start's initial window, RFC 5681 §3.1 equation 1: `min(4*SMSS, max(2*SMSS, 4380))`.
const IW_CEIL: u32 = 4380;
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
    /// A peer's SYN arrived for a port we are listening on; our SYN-ACK is owed or in flight, and we
    /// are waiting for the acknowledgement that completes the handshake.
    ///
    /// The passive half of the open. Until this existed the machine could dial out and never answer,
    /// which is the difference between a client and a host.
    SynReceived,
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
            State::SynReceived => "syn-received",
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

    /// This connection arrived from a LISTENER rather than from `connect`, so it has no owner until
    /// a client accepts it. Distinguishes an unclaimed inbound connection from a free slot, both of
    /// which have `rid == 0`.
    pub accepted: bool,

    /// Where to address this connection's frames: the peer's own MAC when it is on-link, the
    /// gateway's when it is not.
    ///
    /// **Per CONNECTION, not per service, and that distinction cost a day of hardware debugging.**
    /// It was a single field on `Net` meaning "the gateway", which is wrong for a host on our own
    /// subnet: our half of the flow goes through the router while the peer answers us directly, the
    /// router sees one side and drops the rest, and the symptom is a handshake that reaches
    /// Established and then nothing while `ping` to the same host works. Resolving it per connection
    /// fixed that - but it was still being carried on `Net`, which is per CALL. The moment a
    /// connection outlives the request that opened it, a background poll would emit with whatever
    /// `Net` the caller happened to build, which is the gateway again. Storing it here makes that
    /// impossible rather than merely unlikely.
    pub peer_mac: [u8; 6],

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
    /// An acknowledgement is owed to the peer, to be sent by `poll_one` on its next pass.
    ///
    /// `on_frame` USED TO SEND IT ITSELF, and that was the last hardware bug on this branch. Doing so
    /// means calling `nic_req` from inside the handling of another `nic_req` reply, and net-stack has
    /// ONE endpoint with ONE receive slot - so the nested request/reply desyncs against the outer
    /// one, exactly as `docs/net-tags-design.md` describes. The symptom on a Pi 2: 3 SYN-ACKs
    /// matched, 8 frames handed over, 4 on the wire, and an orphaned
    /// "request had no reply cap - dropping" left behind. The ACKs were built and never transmitted,
    /// so the peer retransmitted its SYN-ACK eight times and the handshake never completed.
    ///
    /// Reacting to the peer and making our own progress are now separable, which is what
    /// `docs/tcp-design.md` said the architecture had to be before any of this was written.
    ack_due: bool,
    /// The window most recently ADVERTISED to the peer.
    ///
    /// Without this there is no way to notice that the window has reopened. Reading data out of the
    /// arena frees space, and a peer that was told 608 bytes stays limited to 608 until it is told
    /// otherwise - so a reply larger than the arena stalls partway and the connection then closes
    /// with the rest unread. Measured, not theorised: a 2888-byte reply arrived as 1440 bytes.
    last_adv: u16,

    // ── Congestion control (RFC 5681) ──────────────────────────────────────────────────────────
    /// What the PEER will accept in one segment, from its SYN's option or the RFC 1122 default.
    /// Distinct from `MSS`, which is what WE will accept: the two are independent and a correct
    /// stack sends by the smaller.
    pub snd_mss: u16,
    /// The congestion window, in bytes. The NETWORK's limit on what may be outstanding, as opposed
    /// to `snd_wnd`, which is the RECEIVER's. Sending is bounded by the smaller of the two.
    pub cwnd: u32,
    /// Slow start ends and congestion avoidance begins when `cwnd` reaches this. Starts effectively
    /// infinite: nothing is known about the path until something is lost, and guessing a limit
    /// before the first loss would be exactly the invented number 26.4 objects to.
    pub ssthresh: u32,
    /// Consecutive acknowledgements that acknowledged nothing new. Three is the signal to
    /// retransmit without waiting for the timer (RFC 5681 3.2) - the peer is telling us, through
    /// the only channel it has, that it is receiving segments with a hole in front of them.
    dup_acks: u8,
    /// NewReno: the highest sequence sent when fast recovery began. Recovery ends when this is
    /// acknowledged, not when the first new acknowledgement arrives - otherwise a second loss in the
    /// same window halves the window twice for one event.
    recover: u32,
    in_recovery: bool,
    /// Set by fast retransmit, cleared when `poll_one` acts on it.
    fast_retx: bool,
    /// Persist timer: when to probe a window the peer has closed. 0 = not armed.
    ///
    /// A window update is a bare acknowledgement, and a bare acknowledgement is not retransmitted by
    /// anybody. So if the one that reopens a shut window is lost, the sender waits for a message
    /// that will never come and the receiver waits for data that will never be sent - a deadlock out
    /// of two correct implementations. The code here said this was missing and left the retransmit
    /// timer to cover it; the retransmit timer does not cover it, because with everything
    /// acknowledged and the window at zero there is nothing armed to retransmit.
    probe_at_ms: u64,
    probe_backoff_ms: u64,
}

impl Conn {
    pub const fn free() -> Self {
        Conn {
            state: State::Closed, fault: Fault::None, rid: 0, accepted: false, peer_mac: [0; 6],
            local_port: 0, remote_ip: [0; 4], remote_port: 0,
            snd_una: 0, snd_nxt: 0, snd_wnd: 0, iss: 0, rcv_nxt: 0,
            snd_buf: [0u8; SND_BUF], snd_len: 0,
            rcv_buf: [0u8; RCV_BUF], rcv_len: 0,
            ooo: [Ooo::empty(); MAX_OOO],
            snd_mss: MSS_DEFAULT,
            cwnd: 0, ssthresh: u32::MAX / 2,
            dup_acks: 0, recover: 0, in_recovery: false, fast_retx: false,
            probe_at_ms: 0, probe_backoff_ms: 0,
            retx_at_ms: 0, retx_count: 0,
            srtt_ms: 0, rttvar_ms: 0, rto_ms: RTO_MIN_MS,
            rtt_timed_seq: 0, rtt_timed_at_ms: 0, rtt_timing: false,
            state_deadline_ms: 0, close_pending: false, ack_due: false, last_adv: 0,
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
    /// The peer's maximum segment size, when this is a SYN that carried the option. `None` on every
    /// other segment, and on a SYN that offered nothing - which RFC 1122 says to read as 536.
    pub mss: Option<u16>,
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
    // The peer's maximum segment size, if it offered one. Only ever present on a SYN, so only ever
    // looked for there - options on an ordinary segment are somebody else's extension and are
    // skipped by `doff` above, which is the whole reason that field exists.
    let mss = if f[t + 13] & SYN != 0 && doff > TCP_LEN {
        parse_mss(&f[t + TCP_LEN..t + doff])
    } else {
        None
    };

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
        mss,
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

/// Read a maximum-segment-size option out of a SYN's option field, if one is there.
///
/// Walks the list properly rather than looking at the first four bytes, because the option that
/// matters is not guaranteed to be first and rarely is: Linux leads with MSS, Windows does too, but
/// a peer is entitled to put a No-Operation or a window-scale option ahead of it. A malformed list
/// stops the walk instead of being interpreted - a length byte of 0 or 1 would otherwise loop
/// forever on a frame a hostile peer controls entirely (Commandment V).
fn parse_mss(opts: &[u8]) -> Option<u16> {
    let mut i = 0;
    while i < opts.len() {
        match opts[i] {
            0 => return None,                       // End of Option List
            1 => i += 1,                            // No-Operation, one byte, no length
            kind => {
                if i + 1 >= opts.len() { return None; }
                let len = opts[i + 1] as usize;
                if len < 2 || i + len > opts.len() { return None; }
                if kind == 2 && len == 4 {
                    return Some(((opts[i + 2] as u16) << 8) | opts[i + 3] as u16);
                }
                i += len;
            }
        }
    }
    None
}

/// Build one Ethernet/IPv4/TCP frame into `out`, returning its length.
///
/// Returns 0 rather than panicking if the payload cannot fit, so a caller that miscounts loses a
/// segment instead of taking down the service (Commandment V: nothing above the kernel halts the
/// machine, including by panic).
#[allow(clippy::too_many_arguments)]
pub fn emit(out: &mut [u8], peer_mac: &[u8; 6], our_mac: &[u8; 6], our_ip: &[u8; 4],
            dst_ip: &[u8; 4], src_port: u16, dst_port: u16,
            seq: u32, ack: u32, flags: u8, wnd: u16, payload: &[u8]) -> usize {
    // A SYN, and only a SYN, carries our maximum segment size. Announcing it is not a nicety: a
    // peer that receives no option must assume 536 (RFC 1122 §4.2.2.6), so a stack that stays silent
    // is asking every peer on the internet to talk to it in 536-byte pieces. We were silent.
    let opt_len = if flags & SYN != 0 { OPT_MSS_LEN } else { 0 };
    let total = HDR + opt_len + payload.len();
    if total > out.len() || payload.len() > MSS { return 0; }
    for b in out[..total].iter_mut() { *b = 0; }

    out[0..6].copy_from_slice(peer_mac);
    out[6..12].copy_from_slice(our_mac);
    out[12] = 0x08; out[13] = 0x00;

    let ip = ETH_LEN;
    out[ip] = 0x45;                                              // IPv4, 20-byte header
    let ip_total = (IP_LEN + TCP_LEN + opt_len + payload.len()) as u16;
    out[ip + 2] = (ip_total >> 8) as u8; out[ip + 3] = ip_total as u8;
    // IP IDENTIFICATION AND FLAGS, MATCHED TO THE FRAME THAT DEMONSTRABLY WORKS ON A REAL LAN.
    //
    // This used to send identification 0 with the Don't Fragment bit set. Both are legal - RFC 6864
    // explicitly allows a zero id when DF is set - and QEMU never cared. On a Raspberry Pi 2 behind a
    // consumer router, a SYN built that way never reached a host on the same subnet, while `ping` to
    // that same host worked: and `ping` sends identification 1 with DF CLEAR, through the same
    // gateway MAC, from the same source, over the same driver.
    //
    // Verified from the capture rather than assumed: our TCP checksums, IP header checksums and runt
    // padding are all correct, so the header fields were the only difference left. Matching the
    // working frame is an evidence-led change, not a cargo-culted one - but it is a HYPOTHESIS about
    // that router's behaviour, and if it turns out not to be the cause this comment should say so
    // rather than be quietly deleted.
    // Derived, not threaded: the sequence number already varies per segment, so mixing it with the
    // source port gives a non-zero, changing identification without a counter to carry through ten
    // call sites. The field only has to differ between packets that could be fragments of each other.
    let ip_id = (seq as u16) ^ src_port ^ 0x1000;
    out[ip + 4] = (ip_id >> 8) as u8; out[ip + 5] = ip_id as u8;
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
    // Data offset in 32-BIT WORDS, so the option's four bytes are one word: 5 without, 6 with.
    out[t + 12] = (((TCP_LEN + opt_len) / 4) as u8) << 4;
    out[t + 13] = flags;
    out[t + 14] = (wnd >> 8) as u8; out[t + 15] = wnd as u8;
    if opt_len == OPT_MSS_LEN {
        out[t + 20] = 2;                                         // kind: maximum segment size
        out[t + 21] = 4;                                         // length, including these two bytes
        out[t + 22] = (MSS >> 8) as u8; out[t + 23] = MSS as u8;
    }
    out[t + TCP_LEN + opt_len..total].copy_from_slice(payload);

    let ck = tcp_checksum(our_ip, dst_ip, &out[t..total]);
    out[t + 16] = (ck >> 8) as u8; out[t + 17] = ck as u8;
    total
}

// ── The connection table and the state machine ─────────────────────────────────────────────────

/// Everything TCP owns. Passed by `&mut` from `service_main` rather than living in a static, because
/// a service may hold no unowned global mutable state (Commandment VI, and `VI-static-mut` enforces
/// it). That also makes the footprint honest: this struct IS the memory cost of TCP here.
/// A port this machine answers on.
///
/// **Deliberately not a `Conn`.** BSD gives a listener a full socket and so does most of the
/// literature, but a `Conn` here carries a 2 KiB send arena, a 2 KiB receive arena and four
/// out-of-order slots - about ten kilobytes that a listener never touches, on a service whose entry
/// frame is already a third of its stack. A listener needs a port and an owner, so that is what it
/// is: ten bytes, and `MAX_CONNS` stays available for actual connections.
#[derive(Clone, Copy)]
pub struct Listener {
    /// The port being answered on. 0 = this slot is free.
    pub port: u16,
    /// The capability this listener was minted as, so its owner can be found and so closing it is
    /// an ordinary revoke.
    pub rid: u64,
}

impl Listener {
    pub const fn free() -> Self { Listener { port: 0, rid: 0 } }
}

/// How many ports can be listened on at once. Two, matching `MAX_CONNS`: a machine that can hold two
/// connections has no use for more listening ports than that, and each is checked on every inbound
/// segment that matches no connection.
pub const MAX_LISTEN: usize = 2;

pub struct Tcp {
    pub conns: [Conn; MAX_CONNS],
    /// Ports this machine answers on. See `Listener`.
    pub listeners: [Listener; MAX_LISTEN],
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
    /// How far the LAST transaction got, kept after its slot is released. A connection that never
    /// left `SynSent` and one that reached `Established` and was answered with nothing are the same
    /// empty reply to a client and completely different faults to diagnose.
    pub last_state: State,
    pub last_retx: u8,
    /// Frames handed to `on_frame` during the last transaction, and how many of those it RECOGNISED
    /// as a segment for one of our connections. The gap between the two is the whole diagnosis when a
    /// connection stalls: frames arriving but none matching means the peer is talking to a
    /// four-tuple we do not have.
    pub stat_seen: u16,
    pub stat_matched: u16,
    /// Frames this table asked to be TRANSMITTED. If a connection reaches Established and this is
    /// still only the SYN count, the acknowledgement was never built - which is a different fault
    /// from one that was built and lost.
    pub stat_sent: u16,
    /// One character per frame this transaction handed to the driver, in order, so the log can show
    /// WHICH frames were built rather than only how many.
    ///
    ///   S/A/F/D  a SYN, a bare acknowledgement, a FIN, or a segment carrying data
    ///   lower case   the same frame, where `nic_req` did NOT come back with a reply
    ///
    /// The distinction is the whole point: a stack that builds an acknowledgement and a stack whose
    /// acknowledgement never leaves look identical in a count, and they are different bugs.
    pub tx_log: [u8; 24],
    pub tx_n: usize,
}

impl Tcp {
    pub fn new(tsc_hz: u64, now_tsc: u64) -> Self {
        Tcp {
            conns: [Conn::free(); MAX_CONNS],
            listeners: [Listener::free(); MAX_LISTEN],
            next_port: 49152,
            cyc_per_ms: tsc_hz / 1000,
            base_tsc: now_tsc,
            warned_no_clock: false,
            last_state: State::Closed,
            last_retx: 0,
            stat_seen: 0, stat_matched: 0, stat_sent: 0,
            tx_log: [0u8; 24], tx_n: 0,
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

    /// Record one transmitted frame by its TCP flags, and whether the driver answered.
    ///
    /// Reads the flags out of the frame that is actually going out, not from what the caller
    /// believes it built - the two have differed on this branch already.
    pub fn note_tx(&mut self, frame: &[u8], delivered: bool) {
        if self.tx_n >= self.tx_log.len() || frame.len() < HDR { return; }
        let fl = frame[ETH_LEN + IP_LEN + 13];
        let mut ch = if fl & SYN != 0 { b'S' }
                     else if fl & FIN != 0 { b'F' }
                     else if frame.len() > HDR { b'D' }
                     else { b'A' };
        if !delivered { ch = ch.to_ascii_lowercase(); }
        self.tx_log[self.tx_n] = ch;
        self.tx_n += 1;
    }

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

    /// Answer on `port` from now on. Returns false if every listener slot is taken, or if that port
    /// is already being listened on - a second listener on one port would make which one receives a
    /// connection a matter of array order, which is exactly the kind of implicit behaviour §26.5
    /// refuses.
    pub fn listen(&mut self, port: u16, rid: u64) -> bool {
        if port == 0 { return false; }
        if self.listeners.iter().any(|l| l.port == port) { return false; }
        match self.listeners.iter_mut().find(|l| l.port == 0) {
            Some(l) => { l.port = port; l.rid = rid; true }
            None => false,
        }
    }

    /// Stop answering on the port this listener owns.
    ///
    /// Connections already ACCEPTED from it are untouched and keep running, which is the same
    /// separation `fs` makes between a directory and the files opened from it: closing the listener
    /// closes the door, not the conversations already inside.
    pub fn unlisten(&mut self, rid: u64) -> bool {
        match self.listeners.iter_mut().find(|l| l.rid == rid && l.port != 0) {
            Some(l) => { *l = Listener::free(); true }
            None => false,
        }
    }

    /// Is this port being listened on?
    fn listening_on(&self, port: u16) -> bool {
        self.listeners.iter().any(|l| l.port == port)
    }

    /// A connection that has completed its handshake and has not been handed to a client yet.
    ///
    /// `rid == 0` is what marks it unclaimed: a connection opened by `connect` is minted with its
    /// client's resource id from the start, while an accepted one has no owner until somebody takes
    /// it.
    pub fn pending_accept(&self) -> Option<usize> {
        (0..MAX_CONNS).find(|&i| {
            let c = &self.conns[i];
            c.rid == 0 && c.accepted && matches!(c.state,
                State::Established | State::CloseWait | State::FinWait1 | State::FinWait2)
        })
    }

    /// Start an active open. Returns the slot index, or `None` when the table is full.
    ///
    /// The initial sequence number is derived from the clock rather than fixed. RFC 793 wants it
    /// unpredictable so a delayed segment from an old incarnation of the same connection cannot be
    /// accepted as current; a constant ISS makes that failure reachable on a machine that reboots
    /// fast, which this one does.
    pub fn connect(&mut self, ctx: &ServiceContext, rid: u64, dst: [u8; 4], dport: u16,
                   peer_mac: [u8; 6]) -> Option<usize> {
        let iss = (ctx.read_tsc() as u32) ^ 0x5a5a_0000;
        let port = self.next_port;
        self.next_port = if self.next_port >= 65000 { 49152 } else { self.next_port + 1 };
        let now = self.now_ms(ctx).unwrap_or(0);
        let i = self.free_slot()?;
        let c = &mut self.conns[i];
        *c = Conn::free();
        c.rid = rid;
        c.peer_mac = peer_mac;
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
// ── Proving the parts QEMU cannot reach ────────────────────────────────────────────────────────

/// Drive the state machine against frames built in memory, and report what held.
///
/// **This exists because the interesting new behaviour cannot be reached from a test that uses the
/// network.** Congestion control reacts to LOSS, fast retransmit to three duplicate acknowledgements,
/// the persist timer to a window the peer has closed - and the QEMU user-mode backend the branch is
/// tested against drops nothing, reorders nothing and never shuts its window. A guard that has never
/// been observed firing is not evidence that it works; it is evidence that nothing has asked.
///
/// So the peer is synthesised. Every frame handed to `on_frame` here is built by `emit`, the same
/// function that builds the ones that go on the wire, and read back by `parse`, the same one that
/// reads the ones that arrive - which makes this a round trip through the real encoders rather than
/// a test of a mock. No NIC is involved and nothing is transmitted.
///
/// Returns `(passed, failed)`. Each failure is logged with what it expected, because a self-test that
/// reports only a count tells you something is wrong and nothing about what.
#[inline(never)]
pub fn selftest(ctx: &ServiceContext) -> (u32, u32) {
    let mut pass = 0u32;
    let mut fail = 0u32;
    let mut check = |ok: bool, what: &str, p: &mut u32, f: &mut u32| {
        if ok { *p += 1; } else { *f += 1; ctx.log_fmt(format_args!("tcp selftest: FAIL - {}", what)); }
    };

    // ---- the option encoder and decoder agree ----
    //
    // Round trip, not a fixed byte string: a fixed string would keep passing if both sides moved
    // together, and both sides moving together is exactly what a data-offset mistake looks like.
    let net = Net { our_mac: [2, 0, 0, 0, 0, 1], peer_mac: [2, 0, 0, 0, 0, 2], our_ip: [10, 0, 0, 1] };
    let mut buf = [0u8; 1600];
    let n = emit(&mut buf, &net.peer_mac, &net.our_mac, &net.our_ip, &[10, 0, 0, 2],
                 1234, 80, 100, 0, SYN, 2048, &[]);
    check(n == HDR + 4, "a SYN is four bytes longer than a bare header (the MSS option)", &mut pass, &mut fail);
    match parse(&buf[..n]) {
        Some(sg) => {
            check(sg.mss == Some(MSS as u16), "the MSS we emit is the MSS we parse", &mut pass, &mut fail);
            check(sg.payload.is_empty(), "a SYN's options are not mistaken for payload", &mut pass, &mut fail);
        }
        None => check(false, "a SYN carrying options parses at all", &mut pass, &mut fail),
    }
    // A segment that is NOT a SYN must carry no option and stay 20 bytes of header.
    let n2 = emit(&mut buf, &net.peer_mac, &net.our_mac, &net.our_ip, &[10, 0, 0, 2],
                  1234, 80, 100, 1, ACK, 2048, b"hi");
    check(n2 == HDR + 2, "an ordinary segment carries no options", &mut pass, &mut fail);

    // ---- the option WALKER handles what a real peer sends, and what a hostile one does ----
    check(parse_mss(&[1, 1, 2, 4, 0x05, 0xb4]) == Some(1460),
          "an MSS option behind two No-Operations is still found", &mut pass, &mut fail);
    check(parse_mss(&[3, 3, 7, 2, 4, 0x02, 0x18]) == Some(536),
          "an MSS option behind a window-scale option is still found", &mut pass, &mut fail);
    check(parse_mss(&[0, 2, 4, 0x05, 0xb4]).is_none(),
          "nothing is read past an End of Option List", &mut pass, &mut fail);
    // The two that matter for not hanging on a frame the peer controls entirely (Commandment V).
    check(parse_mss(&[2, 0, 0, 0]).is_none(), "a zero option length terminates the walk", &mut pass, &mut fail);
    check(parse_mss(&[2, 40, 0, 0]).is_none(), "an option length past the end terminates the walk", &mut pass, &mut fail);

    // ---- the congestion arithmetic ----
    let mut c = Conn::free();
    c.snd_mss = MSS as u16;
    c.cc_open();
    check(c.cwnd == IW_CEIL, "the initial window is RFC 5681's min(4*SMSS, max(2*SMSS, 4380))", &mut pass, &mut fail);
    // Slow start: one segment's worth per acknowledged segment.
    let before = c.cwnd;
    c.cc_acked(MSS as u32);
    check(c.cwnd == before + MSS as u32, "slow start grows by a segment per acknowledgement", &mut pass, &mut fail);
    // Congestion avoidance: far slower, but never zero. The `.max(1)` is the thing being pinned -
    // integer division gives zero once cwnd passes SMSS squared, and a window that cannot grow is a
    // stall that looks like a slow network.
    c.ssthresh = 1;
    c.cwnd = 4_000_000;
    let before = c.cwnd;
    c.cc_acked(MSS as u32);
    check(c.cwnd > before, "congestion avoidance still grows when cwnd exceeds SMSS squared", &mut pass, &mut fail);
    check(c.cwnd - before < MSS as u32, "congestion avoidance grows far slower than slow start", &mut pass, &mut fail);
    // Loss halves, with a two-segment floor.
    c.cc_lost(20_000);
    check(c.ssthresh == 10_000, "loss halves what the path is believed to carry", &mut pass, &mut fail);
    c.cc_lost(100);
    check(c.ssthresh == 2 * MSS as u32, "the floor after loss is two segments, not the halved value", &mut pass, &mut fail);

    // ---- fast retransmit, driven through the real state machine ----
    let mut t = Tcp::new(0, 0);          // no clock: timers are inert, which is what this wants
    let peer = [10, 0, 0, 2];
    let Some(i) = t.connect(ctx, 7, peer, 80, net.peer_mac) else {
        check(false, "a connection slot is available for the self-test", &mut pass, &mut fail);
        return (pass, fail);
    };
    let (lport, iss_plus1) = { let c = &t.conns[i]; (c.local_port, c.snd_nxt) };
    // The peer's SYN-ACK, built by the same encoder. Its own sequence is arbitrary.
    let pseq: u32 = 0x1000_0000;
    let n = emit(&mut buf, &net.our_mac, &net.peer_mac, &peer, &net.our_ip,
                 80, lport, pseq, iss_plus1, SYN | ACK, 8000, &[]);
    let mut sink = [0u8; 1600];
    t.on_frame(ctx, &net, &buf[..n], &mut sink);
    check(t.conns[i].state == State::Established, "the synthetic SYN-ACK establishes the connection", &mut pass, &mut fail);
    check(t.conns[i].snd_mss == MSS as u16, "the peer's advertised MSS is adopted", &mut pass, &mut fail);
    check(t.conns[i].cwnd == IW_CEIL, "the congestion window opens on establishment", &mut pass, &mut fail);

    // Queue data and put a segment on the wire, so there is something to acknowledge.
    let payload = [0x41u8; 200];
    let wrote = t.write(7, &payload);
    check(wrote == payload.len(), "the send arena accepted the self-test's data", &mut pass, &mut fail);
    // POLL UNTIL THERE IS DATA, not once. `poll_one` pays the acknowledgement owed from the SYN-ACK
    // before it sends anything of its own - deliberately, because the peer is waiting on that to
    // finish its handshake - so the first pass carries no payload. Asserting on one pass is how this
    // self-test failed the first time it ran, which is a fair demonstration that it is looking.
    let mut sent = 0usize;
    for _ in 0..4 {
        let n = t.poll_one(ctx, &net, i, &mut sink);
        if n == 0 { break; }
        if parse(&sink[..n]).map(|sg| !sg.payload.is_empty()).unwrap_or(false) { sent = n; break; }
    }
    check(sent > HDR, "a data segment reaches the wire once the owed acknowledgement is paid", &mut pass, &mut fail);
    let una = t.conns[i].snd_una;

    // Three bare, window-unchanged, nothing-new acknowledgements: the peer reporting a hole.
    let ss_before = t.conns[i].ssthresh;
    for k in 0..3 {
        let n = emit(&mut buf, &net.our_mac, &net.peer_mac, &peer, &net.our_ip,
                     80, lport, pseq.wrapping_add(1), una, ACK, 8000, &[]);
        t.on_frame(ctx, &net, &buf[..n], &mut sink);
        if k < 2 {
            check(!t.conns[i].in_recovery,
                  "one or two duplicates are NOT treated as loss (reordering is likelier)", &mut pass, &mut fail);
        }
    }
    check(t.conns[i].in_recovery, "three duplicate acknowledgements enter fast recovery", &mut pass, &mut fail);
    check(t.conns[i].ssthresh < ss_before, "entering recovery lowers the slow-start threshold", &mut pass, &mut fail);
    check(t.conns[i].retx_count == 0, "a fast retransmit is NOT counted as a timeout", &mut pass, &mut fail);
    // And it actually resends, from the hole, on the next poll.
    let again = t.poll_one(ctx, &net, i, &mut sink);
    check(again > HDR, "fast retransmit puts the missing segment back on the wire", &mut pass, &mut fail);
    if let Some(sg) = parse(&sink[..again]) {
        check(sg.seq == una, "the retransmission starts at the hole, not at snd_nxt", &mut pass, &mut fail);
    }

    // A peer that offers NO option must leave us at the RFC 1122 default rather than our own 1460.
    t = Tcp::new(0, 0);
    {
        let t2 = &mut t;
        if let Some(j) = t2.connect(ctx, 8, peer, 80, net.peer_mac) {
            let (lp2, ack2) = { let c = &t2.conns[j]; (c.local_port, c.snd_nxt) };
            // Hand-built so the SYN flag is set but no option follows - `emit` always adds one.
            let m = emit(&mut buf, &net.our_mac, &net.peer_mac, &peer, &net.our_ip,
                         80, lp2, pseq, ack2, SYN | ACK, 8000, &[]);
            buf[ETH_LEN + IP_LEN + 12] = 0x50;                       // data offset back to 5 words
            let ip_total = (IP_LEN + TCP_LEN) as u16;
            buf[ETH_LEN + 2] = (ip_total >> 8) as u8; buf[ETH_LEN + 3] = ip_total as u8;
            buf[ETH_LEN + 10] = 0; buf[ETH_LEN + 11] = 0;
            let ck = super::checksum(&buf[ETH_LEN..ETH_LEN + IP_LEN]);
            buf[ETH_LEN + 10] = (ck >> 8) as u8; buf[ETH_LEN + 11] = ck as u8;
            let end = ETH_LEN + IP_LEN + TCP_LEN;
            buf[ETH_LEN + IP_LEN + 16] = 0; buf[ETH_LEN + IP_LEN + 17] = 0;
            let tck = tcp_checksum(&peer, &net.our_ip, &buf[ETH_LEN + IP_LEN..end]);
            buf[ETH_LEN + IP_LEN + 16] = (tck >> 8) as u8; buf[ETH_LEN + IP_LEN + 17] = tck as u8;
            let _ = m;
            t2.on_frame(ctx, &net, &buf[..end], &mut sink);
            check(t2.conns[j].state == State::Established, "a SYN-ACK with no options still establishes", &mut pass, &mut fail);
            check(t2.conns[j].snd_mss == MSS_DEFAULT,
                  "a peer that offers no MSS is assumed to accept 536, not our own 1460", &mut pass, &mut fail);
        }
    }

    // ---- PASSIVE OPEN: the machine answers a connection it did not start ----
    //
    // Driven entirely through the real entry points - `listen`, then `on_frame` with a SYN built by
    // `emit`, then `poll_one` - so this exercises the same code an inbound connection takes and not
    // a paraphrase of it.
    t = Tcp::new(0, 0);
    {
        let lst = &mut t;
        check(lst.listen(8080, 42), "a port can be listened on", &mut pass, &mut fail);
        check(!lst.listen(8080, 43), "the same port cannot be listened on twice", &mut pass, &mut fail);
        check(lst.listen(9090, 44), "a second, different port can be listened on", &mut pass, &mut fail);
        check(!lst.listen(7070, 45), "a third listener is refused - the bound is MAX_LISTEN", &mut pass, &mut fail);
        check(lst.pending_accept().is_none(), "nothing is waiting to be accepted before any SYN", &mut pass, &mut fail);

        // A peer dials in. Its MAC is one we never resolved - the point being that the reply is
        // addressed from the FRAME, so no ARP is needed for an inbound connection.
        let their_mac = [0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f];
        let their_ip = [10, 0, 0, 9];
        let their_seq: u32 = 0x2000_0000;
        let n = emit(&mut buf, &net.our_mac, &their_mac, &their_ip, &net.our_ip,
                     55000, 8080, their_seq, 0, SYN, 4096, &[]);
        lst.on_frame(ctx, &net, &buf[..n], &mut sink);

        let idx = (0..MAX_CONNS).find(|&k| lst.conns[k].state == State::SynReceived);
        check(idx.is_some(), "a SYN to a listening port opens a connection", &mut pass, &mut fail);
        let Some(k) = idx else { return (pass, fail) };
        check(lst.conns[k].accepted, "the connection is marked as one we did not start", &mut pass, &mut fail);
        check(lst.conns[k].rid == 0, "and it has no owner until somebody accepts it", &mut pass, &mut fail);
        check(lst.conns[k].peer_mac == their_mac,
              "its peer MAC is taken from the frame, so no ARP is needed", &mut pass, &mut fail);
        check(lst.conns[k].rcv_nxt == their_seq.wrapping_add(1),
              "the peer's SYN consumed one sequence number", &mut pass, &mut fail);
        check(lst.pending_accept().is_none(),
              "a half-open connection is NOT offered for accept", &mut pass, &mut fail);

        // The SYN-ACK is owed, not sent, and `poll_one` is what sends it.
        let n = lst.poll_one(ctx, &net, k, &mut sink);
        check(n > 0, "poll_one sends the SYN-ACK the handshake owes", &mut pass, &mut fail);
        let mut our_iss = 0u32;
        if let Some(sg) = parse(&sink[..n]) {
            check(sg.flags & SYN != 0 && sg.flags & ACK != 0,
                  "and it is a SYN-ACK, not a bare acknowledgement", &mut pass, &mut fail);
            check(sg.ack == their_seq.wrapping_add(1),
                  "acknowledging exactly the peer's SYN", &mut pass, &mut fail);
            check(sg.mss == Some(MSS as u16),
                  "carrying our maximum segment size, as every SYN must", &mut pass, &mut fail);
            our_iss = sg.seq;
        }

        // The peer completes the handshake, with data riding on the same segment - the common case.
        let n = emit(&mut buf, &net.our_mac, &their_mac, &their_ip, &net.our_ip,
                     55000, 8080, their_seq.wrapping_add(1), our_iss.wrapping_add(1),
                     ACK, 4096, b"GET /");
        lst.on_frame(ctx, &net, &buf[..n], &mut sink);
        check(lst.conns[k].state == State::Established,
              "the peer's acknowledgement completes the passive open", &mut pass, &mut fail);
        check(lst.conns[k].readable() == 5,
              "data arriving WITH that acknowledgement is delivered, not dropped", &mut pass, &mut fail);
        check(lst.pending_accept() == Some(k),
              "an established inbound connection is offered for accept", &mut pass, &mut fail);

        // A wrong acknowledgement must not complete a handshake. Fresh connection, same listener.
        let n = emit(&mut buf, &net.our_mac, &their_mac, &their_ip, &net.our_ip,
                     55001, 9090, their_seq, 0, SYN, 4096, &[]);
        lst.on_frame(ctx, &net, &buf[..n], &mut sink);
        if let Some(k2) = (0..MAX_CONNS).find(|&x| lst.conns[x].state == State::SynReceived) {
            let n = emit(&mut buf, &net.our_mac, &their_mac, &their_ip, &net.our_ip,
                         55001, 9090, their_seq.wrapping_add(1), 0xdead_beef, ACK, 4096, &[]);
            lst.on_frame(ctx, &net, &buf[..n], &mut sink);
            check(lst.conns[k2].state == State::SynReceived,
                  "an acknowledgement of something we never sent does NOT complete the handshake",
                  &mut pass, &mut fail);
        }

        // Closing the listener stops new connections without disturbing established ones.
        check(lst.unlisten(42), "a listener can be closed by its owner", &mut pass, &mut fail);
        check(!lst.unlisten(42), "and closing it twice is refused", &mut pass, &mut fail);
        check(lst.conns[k].state == State::Established,
              "closing the listener leaves connections already accepted from it running",
              &mut pass, &mut fail);
    }

    // ---- the persist timer arms when the peer shuts its window ----
    t = Tcp::new(0, 0);
    let t3 = &mut t;
    if let Some(j) = t3.connect(ctx, 9, peer, 80, net.peer_mac) {
        let (lp3, ack3) = { let c = &t3.conns[j]; (c.local_port, c.snd_nxt) };
        let n = emit(&mut buf, &net.our_mac, &net.peer_mac, &peer, &net.our_ip,
                     80, lp3, pseq, ack3, SYN | ACK, 8000, &[]);
        t3.on_frame(ctx, &net, &buf[..n], &mut sink);
        t3.write(9, &payload);
        let _ = t3.poll_one(ctx, &net, j, &mut sink);
        let una3 = t3.conns[j].snd_una;
        // An acknowledgement of nothing new that SHUTS the window. Deliberately not a duplicate for
        // congestion purposes - the window changed, which RFC 5681 says disqualifies it - so this
        // also pins that the two paths do not collide.
        let n = emit(&mut buf, &net.our_mac, &net.peer_mac, &peer, &net.our_ip,
                     80, lp3, pseq.wrapping_add(1), una3, ACK, 0, &[]);
        t3.on_frame(ctx, &net, &buf[..n], &mut sink);
        check(t3.conns[j].snd_wnd == 0, "a zero-window acknowledgement is recorded", &mut pass, &mut fail);
        check(t3.conns[j].dup_acks == 0, "a window change disqualifies an acknowledgement as a duplicate", &mut pass, &mut fail);
    }

    (pass, fail)
}


// ── Congestion control (RFC 5681) ──────────────────────────────────────────────────────────────

impl Conn {
    /// The segment size to SEND with: the smaller of what we can build and what the peer will take.
    fn smss(&self) -> usize { (self.snd_mss as usize).min(MSS).max(1) }

    /// Open the congestion window for a new connection. RFC 5681 3.1, equation 1.
    fn cc_open(&mut self) {
        let s = self.smss() as u32;
        self.cwnd = (4 * s).min((2 * s).max(IW_CEIL));
        self.ssthresh = u32::MAX / 2;
        self.dup_acks = 0;
        self.in_recovery = false;
        self.fast_retx = false;
    }

    /// An acknowledgement of new data: grow the window.
    ///
    /// Slow start doubles it per round trip; congestion avoidance adds roughly one segment per round
    /// trip, which is the `SMSS*SMSS/cwnd` per acknowledgement of RFC 5681 3.1. The `.max(1)` on the
    /// increment is not cosmetic - integer division gives zero once `cwnd` exceeds `SMSS` squared,
    /// and a window that can never grow again is a stall that looks like a slow network.
    fn cc_acked(&mut self, acked: u32) {
        let s = self.smss() as u32;
        if self.cwnd < self.ssthresh {
            self.cwnd = self.cwnd.saturating_add(acked.min(s));
        } else {
            let inc = (s.saturating_mul(s) / self.cwnd.max(1)).max(1);
            self.cwnd = self.cwnd.saturating_add(inc);
        }
    }

    /// Loss: halve what we believe the path carries, with a floor of two segments (RFC 5681 3.1).
    fn cc_lost(&mut self, inflight: u32) {
        let s = self.smss() as u32;
        self.ssthresh = (inflight / 2).max(2 * s);
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
    /// The MAC to address frames to for THIS connection - the peer's own if it is on-link, the
    /// gateway's otherwise.
    ///
    /// It was `gw_mac` and was always the gateway, which is wrong for a host on our own subnet and
    /// cost a day of hardware debugging. Routing to a neighbour makes the path asymmetric: our half
    /// goes through the router while the peer answers us directly, so the router sees only one side
    /// of the flow and drops everything after the first packet. The symptom was a handshake that
    /// reached Established and then silence - the SYN through, every later segment swallowed - while
    /// `ping` to the same host worked, because ICMP is stateless and gets forwarded regardless.
    pub peer_mac: [u8; 6],
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
        self.stat_seen = self.stat_seen.saturating_add(1);
        let seg = match parse(f) { Some(s) => s, None => return 0 };
        if seg.dst_ip != net.our_ip { return 0; }
        let now = self.now_ms(ctx).unwrap_or(0);

        let (lp, rip, rp) = (seg.dst_port, seg.src_ip, seg.src_port);
        // Counted BEFORE the borrow: `find` takes `&mut self`, so the counter cannot be touched
        // while `c` is alive. Asking first and incrementing on the answer keeps both.
        let matched = self.find(lp, &rip, rp).is_some();
        if matched { self.stat_matched = self.stat_matched.saturating_add(1); }

        // ---- PASSIVE OPEN: a SYN for a port we answer on ----
        //
        // No connection matches, so before dropping the segment, ask whether we are LISTENING on the
        // port it is addressed to. This is the whole of accept as far as the protocol is concerned:
        // a slot is taken, the peer's sequence space is adopted, and a SYN-ACK is owed.
        //
        // A SYN with ACK set is not an opening SYN - it is an answer to a connection we never
        // started, which RFC 793 says to reject rather than adopt. Dropping it is the narrower
        // response and costs the peer only its own timeout.
        if !matched && seg.flags & SYN != 0 && seg.flags & ACK == 0 && self.listening_on(lp) {
            // THE PEER'S MAC COMES FROM THE FRAME, not from ARP. The segment arrived from that
            // address, so it is by construction the right one to answer - no resolution, no
            // gateway-versus-on-link question, and none of the day this cost when `connect` had to
            // work it out. The source MAC of a frame that reached us is the one fact we never have
            // to ask for.
            let mut pmac = [0u8; 6];
            pmac.copy_from_slice(&f[6..12]);
            let iss = (ctx.read_tsc() as u32) ^ 0x7a7a_0000;
            let mss = seg.mss;
            let wnd = seg.wnd;
            let sseq = seg.seq;
            let i = match self.free_slot() {
                Some(i) => i,
                // The table is full. Dropping is correct and deliberate: the peer retries, and by
                // then a slot may have freed. A RST would be ruder and tells it nothing useful.
                //
                // SAID OUT LOUD, though: from the peer's side a refused connection and a machine
                // that is not there look identical, and from this side it is the difference between
                // "nobody called" and "somebody called and we had no room" (§26.7).
                None => {
                    ctx.log_fmt(format_args!(
                        "net-stack: refused a connection from {}.{}.{}.{}:{} on port {} - the                          connection table is full",
                        rip[0], rip[1], rip[2], rip[3], rp, lp));
                    return 0;
                }
            };
            let c = &mut self.conns[i];
            *c = Conn::free();
            c.accepted = true;
            c.peer_mac = pmac;
            c.local_port = lp;
            c.remote_ip = rip;
            c.remote_port = rp;
            c.iss = iss;
            c.snd_una = iss;
            c.snd_nxt = iss.wrapping_add(1);       // our SYN takes one sequence number
            c.rcv_nxt = sseq.wrapping_add(1);      // ...and so does theirs
            c.snd_wnd = wnd;
            if let Some(m) = mss { if m >= 88 { c.snd_mss = m; } }
            c.cc_open();
            c.state = State::SynReceived;
            c.rto_ms = RTO_MIN_MS;
            c.retx_at_ms = now + RTO_MIN_MS;
            c.state_deadline_ms = now + 10_000;    // a handshake that never completes must end
            // OWED, NOT SENT - `on_frame` never transmits. `poll_one` sends the SYN-ACK on its next
            // pass, which is the separation that cost a day of hardware debugging to find.
            c.ack_due = true;
            self.stat_matched = self.stat_matched.saturating_add(1);
            // ANNOUNCE THE ATTEMPT, not just the success. A SYN that arrives and a handshake that
            // completes are different events, and the gap between them is where a passive open
            // fails - so both are logged. Without this, "the SYN never arrived" and "the SYN
            // arrived and we never answered" are the same silence.
            ctx.log_fmt(format_args!(
                "net-stack: inbound connection from {}.{}.{}.{}:{} on port {} - answering",
                rip[0], rip[1], rip[2], rip[3], rp, lp));
            return 0;
        }

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
                    // Acknowledging something we never sent. Drop the connection rather than adopt
                    // its sequence space. A RST would be politer and is deliberately not sent: this
                    // function no longer transmits at all, and a peer that acknowledged phantom data
                    // will time out on its own. Recorded as a narrowing (§26.14).
                    c.fault = Fault::Reset;
                    c.state = State::Closed;
                    return 0;
                }
                c.snd_una = seg.ack;
                c.rcv_nxt = seg.seq.wrapping_add(1);
                c.snd_wnd = seg.wnd;
                // The peer's segment size is settled HERE and nowhere else: the SYN-ACK is the only
                // segment that carries it, and everything sent afterwards is sized by it. A peer
                // that offered nothing keeps the RFC 1122 default of 536 rather than our own 1460,
                // which would be a guess about a path we cannot see.
                if let Some(m) = seg.mss { if m >= 88 { c.snd_mss = m; } }
                // Slow start begins now, not at `connect`: the initial window is a multiple of the
                // segment size, and until this moment the segment size was unknown.
                c.cc_open();
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
        } else if c.state == State::SynReceived {
            // ---- the third leg of a PASSIVE open ----
            //
            // Their acknowledgement of our SYN-ACK. It must acknowledge exactly the sequence number
            // our SYN consumed; anything else is for a connection we do not have.
            if seg.flags & ACK != 0 && seg.ack == c.snd_nxt {
                c.snd_una = seg.ack;
                c.snd_wnd = seg.wnd;
                c.state = State::Established;
                c.retx_at_ms = 0;
                c.retx_count = 0;
                c.state_deadline_ms = 0;
                ctx.log_fmt(format_args!(
                    "net-stack: connection from {}.{}.{}.{}:{} is established - waiting to be accepted",
                    rip[0], rip[1], rip[2], rip[3], rp));
                if c.rtt_timing && now >= c.rtt_timed_at_ms {
                    let r = now - c.rtt_timed_at_ms;
                    c.rtt_sample(r);
                    c.rtt_timing = false;
                }
                // The same segment may carry data, and a client that sends immediately after
                // connecting is the common case rather than an exotic one. Falling through to the
                // data path below would mean re-entering this match arm, so it is delivered here.
                if !seg.payload.is_empty() && seg.seq == c.rcv_nxt {
                    deliver(c, seg.seq, seg.payload);
                    c.ack_due = true;
                }
                if seg.flags & FIN != 0
                    && seg.seq.wrapping_add(seg.payload.len() as u32) == c.rcv_nxt {
                    c.rcv_nxt = c.rcv_nxt.wrapping_add(1);
                    c.state = State::CloseWait;
                    c.ack_due = true;
                }
            }
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
                    // The window has moved, so a shut-window probe is no longer owed.
                    c.probe_at_ms = 0;
                    c.probe_backoff_ms = 0;

                    // ---- congestion control: an acknowledgement of new data ----
                    //
                    // In fast recovery the rule is NewReno's, and the distinction it draws is the
                    // whole reason it exists: recovery ends when everything outstanding when the
                    // loss was detected has been acknowledged, NOT at the first new acknowledgement.
                    // Ending early on a partial acknowledgement treats a second loss in the same
                    // window as a second congestion event and halves the window twice for one.
                    if c.in_recovery {
                        if seq_le(c.recover, seg.ack) {
                            c.in_recovery = false;
                            c.cwnd = c.ssthresh;
                        } else {
                            // A partial acknowledgement: the next hole is now at `snd_una`, so
                            // retransmit from there immediately rather than waiting out the timer.
                            c.fast_retx = true;
                        }
                    } else {
                        c.cc_acked(acked as u32);
                    }
                    c.dup_acks = 0;

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
                } else if seg.ack == c.snd_una
                    && seg.payload.is_empty()
                    && seg.flags & (SYN | FIN) == 0
                    && seg.wnd == c.snd_wnd
                    && c.snd_nxt != c.snd_una
                {
                    // ---- a DUPLICATE acknowledgement (RFC 5681 3.2) ----
                    //
                    // All five conditions matter, and RFC 5681 lists them for a reason: an
                    // acknowledgement that carries data, or opens the window, or acknowledges
                    // something new, is doing a job of its own and is not evidence of loss. Only a
                    // bare repeat, while data is outstanding, means the peer is receiving segments
                    // with a hole in front of them.
                    c.dup_acks = c.dup_acks.saturating_add(1);
                    if c.in_recovery {
                        // Each further duplicate is one segment that has LEFT the network, so the
                        // window may open by one to keep data flowing during recovery.
                        let s = c.smss() as u32;
                        c.cwnd = c.cwnd.saturating_add(s);
                    } else if c.dup_acks == 3 {
                        // FAST RETRANSMIT. Three duplicates is the point at which reordering stops
                        // being the likelier explanation, and waiting for the retransmission timer
                        // costs at least RTO_MIN_MS for something the peer has already told us
                        // about. This is the single largest practical win in this whole change on a
                        // link that drops anything.
                        let inflight = c.snd_nxt.wrapping_sub(c.snd_una);
                        c.cc_lost(inflight);
                        c.recover = c.snd_nxt;
                        c.in_recovery = true;
                        c.cwnd = c.ssthresh.saturating_add(3 * c.smss() as u32);
                        c.fast_retx = true;
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

        // OWED, NOT SENT. See `ack_due`: transmitting from here means a nested request/reply on a
        // single-slot endpoint, and the acknowledgement silently never leaves.
        if need_ack { c.ack_due = true; }
        let _ = out;
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

        // ---- fast retransmit ----
        //
        // Before the timer below, deliberately: the whole value of fast retransmit is that it does
        // not wait for it. Three duplicate acknowledgements are the peer saying it has a hole, and
        // the hole is always at `snd_una` - so that is what goes back out, once, without touching
        // the retransmission counter. This is not a timeout and must not be counted as one, or a
        // link that reorders would close a healthy connection on `MAX_RETX`.
        if c.fast_retx {
            c.fast_retx = false;
            if c.snd_len > 0 {
                let n = c.snd_len.min(c.smss());
                let mut tmp = [0u8; MSS];
                tmp[..n].copy_from_slice(&c.snd_buf[..n]);
                let w = c.window();
                c.last_adv = w;
                c.rtt_timing = false;              // Karn: a resent segment cannot be timed
                if c.retx_at_ms == 0 { c.retx_at_ms = now + c.rto_ms; }
                return emit(out, &c.peer_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                            c.local_port, c.remote_port, c.snd_una, c.rcv_nxt,
                            ACK | PSH, w, &tmp[..n]);
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

            // RFC 5681 3.1: a timeout is the strongest evidence of congestion there is, so the
            // window collapses to ONE segment and slow start begins again. Fast recovery, if it was
            // running, is abandoned - the duplicate acknowledgements it was built on have stopped
            // arriving, which is what the timeout means.
            let inflight = c.snd_nxt.wrapping_sub(c.snd_una);
            c.cc_lost(inflight);
            c.cwnd = c.smss() as u32;
            c.dup_acks = 0;
            c.in_recovery = false;
            c.fast_retx = false;

            if c.state == State::SynSent {
                return emit(out, &c.peer_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                            c.local_port, c.remote_port, c.iss, 0, SYN, RCV_BUF as u16, &[]);
            }
            if c.snd_len > 0 {
                let n = c.snd_len.min(c.smss());
                let mut tmp = [0u8; MSS];
                tmp[..n].copy_from_slice(&c.snd_buf[..n]);
                let w = c.window();
                return emit(out, &c.peer_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                            c.local_port, c.remote_port, c.snd_una, c.rcv_nxt,
                            ACK | PSH, w, &tmp[..n]);
            }
            // Nothing buffered, so the unacknowledged thing is our FIN.
            if matches!(c.state, State::FinWait1 | State::LastAck) {
                let w = c.window();
                return emit(out, &c.peer_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                            c.local_port, c.remote_port, c.snd_nxt.wrapping_sub(1), c.rcv_nxt,
                            ACK | FIN, w, &[]);
            }
            return 0;
        }

        // ---- the SYN-ACK a passive open owes, and its retransmission ----
        //
        // Before the ordinary acknowledgement path, because in `SynReceived` there is no ordinary
        // acknowledgement to send: the peer is waiting on a SYN-ACK and nothing else will do.
        if c.state == State::SynReceived {
            let due = c.ack_due || (has_clock && c.retx_at_ms != 0 && now >= c.retx_at_ms);
            if !due { return 0; }
            if has_clock && c.retx_at_ms != 0 && now >= c.retx_at_ms {
                c.retx_count += 1;
                if c.retx_count > MAX_RETX {
                    c.fault = Fault::RetxExhausted;
                    c.state = State::Closed;
                    return 0;
                }
                c.rto_ms = (c.rto_ms * 2).min(RTO_MAX_MS);
                c.retx_at_ms = now + c.rto_ms;
            }
            c.ack_due = false;
            let w = c.window();
            c.last_adv = w;
            return emit(out, &c.peer_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                        c.local_port, c.remote_port, c.iss, c.rcv_nxt, SYN | ACK, w, &[]);
        }

        // ---- the acknowledgement owed from the last inbound segment ----
        //
        // FIRST, before retransmission or new data: the peer is waiting on this to complete its
        // handshake or to release its window, and anything else we might send is less urgent than
        // the thing it is blocked on.
        if c.ack_due {
            c.ack_due = false;
            let w = c.window();
            c.last_adv = w;
            return emit(out, &c.peer_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                        c.local_port, c.remote_port, c.snd_nxt, c.rcv_nxt, ACK, w, &[]);
        }

        // ---- window update: tell the peer the arena has drained ----
        //
        // RFC 1122 4.2.2.17, and the reason it is not optional here: the advertised window IS the
        // free space in a FIXED arena, so a reply larger than the arena necessarily shuts the window
        // partway through. Once the client reads, the space is back - and a peer that is not told
        // stays throttled at the old figure until something else makes it ask. Sent when at least a
        // segment's worth has opened up, so a byte-at-a-time reader cannot turn this into a storm of
        // ACKs (the silly-window problem, from the other side).
        if matches!(c.state, State::Established | State::CloseWait | State::FinWait1 | State::FinWait2) {
            // THE THRESHOLD IS min(MSS, RCV_BUF/2), not MSS (RFC 1122 4.2.2.17). With MSS 1460 and
            // a 2048-byte arena the window can NEVER reopen by a full MSS after a single segment -
            // 2048 - 1440 = 608, then 1440 - which is less than 1460. So an MSS-only threshold makes
            // this branch unreachable on exactly the buffer sizes this stack uses, and the first
            // version of it was: the fix changed nothing and the reply still arrived truncated at
            // 1440 of 2888 bytes.
            const WND_STEP: u16 = if MSS < RCV_BUF / 2 { MSS as u16 } else { (RCV_BUF / 2) as u16 };
            let w = c.window();
            if w.saturating_sub(c.last_adv) >= WND_STEP {
                c.last_adv = w;
                return emit(out, &c.peer_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                            c.local_port, c.remote_port, c.snd_nxt, c.rcv_nxt, ACK, w, &[]);
            }
        }

        if !matches!(c.state, State::Established | State::CloseWait) { return 0; }

        // ---- new data, within the peer's window ----
        let sent_off = c.snd_nxt.wrapping_sub(c.snd_una) as usize;
        if sent_off < c.snd_len {
            let inflight = sent_off;
            // BOTH windows. `snd_wnd` is what the receiver can hold; `cwnd` is what the path has
            // been shown to carry. Sending by the receiver's alone is what this stack did, and it
            // means answering the first window by putting all of it on the wire at once.
            let usable = (c.snd_wnd as usize).min(c.cwnd as usize);
            let allowed = usable.saturating_sub(inflight);
            if allowed > 0 {
                c.probe_at_ms = 0;
                let n = (c.snd_len - sent_off).min(c.smss()).min(allowed);
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
                    c.last_adv = w;
                    return emit(out, &c.peer_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                                c.local_port, c.remote_port, seq, c.rcv_nxt, ACK | PSH, w, &tmp[..n]);
                }
            }
            // ---- the peer's window is shut: PROBE it (RFC 1122 §4.2.2.17) ----
            //
            // This used to return 0 and say a real stack probes here. It does, and this one now
            // does, because the gap was real rather than theoretical: a window update is a bare
            // acknowledgement, nobody retransmits a bare acknowledgement, and with everything we
            // sent already acknowledged there is no retransmission timer armed to cover its loss.
            // Both sides then wait forever, each correctly.
            //
            // Only when it is the RECEIVER holding us, not the congestion window - a shut congestion
            // window is our own doing and its timer is the retransmission timer.
            if has_clock && c.snd_wnd == 0 {
                if c.probe_at_ms == 0 {
                    c.probe_backoff_ms = c.rto_ms;
                    c.probe_at_ms = now + c.probe_backoff_ms;
                } else if now >= c.probe_at_ms {
                    // Backoff, clamped, so an unresponsive peer costs a probe now and then rather
                    // than a stream of them (§26.6).
                    c.probe_backoff_ms = (c.probe_backoff_ms * 2).max(RTO_MIN_MS).min(RTO_MAX_MS);
                    c.probe_at_ms = now + c.probe_backoff_ms;
                    // ONE byte, deliberately beyond the window the peer advertised. That is what
                    // makes it a probe: the peer must answer it, either by accepting the byte (the
                    // window had reopened and the update was lost) or by repeating the zero window
                    // (it really is still full). Either answer un-sticks us.
                    //
                    // `snd_nxt` advances, because this is real data and not a ghost - an
                    // acknowledgement for it must pass the `seq_le(seg.ack, c.snd_nxt)` test above
                    // or it would be discarded as acknowledging something never sent. Advancing it
                    // also arms the retransmission timer, which is what bounds this against a peer
                    // that has silently gone away.
                    let b = [c.snd_buf[sent_off]];
                    let seq = c.snd_nxt;
                    c.snd_nxt = c.snd_nxt.wrapping_add(1);
                    if c.retx_at_ms == 0 { c.retx_at_ms = now + c.rto_ms; }
                    let w = c.window();
                    c.last_adv = w;
                    return emit(out, &c.peer_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
                                c.local_port, c.remote_port, seq, c.rcv_nxt, ACK, w, &b);
                }
            }
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
            return emit(out, &c.peer_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
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
        emit(out, &c.peer_mac, &net.our_mac, &net.our_ip, &c.remote_ip,
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
