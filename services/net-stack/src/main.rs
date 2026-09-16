// SPDX-License-Identifier: GPL-2.0-only
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
//! net-stack - the model-AGNOSTIC half of networking (docs/networking.md, Phase 2).
//!
//! nic-driver knows one NIC and speaks raw Ethernet frames; net-stack knows no hardware and speaks
//! ARP/IPv4/ICMP/UDP over those frames. The seam between them is the **frame interface**: a
//! request/reply (§8.2) where the request payload IS a frame to transmit and the reply payload IS the
//! frame that came back. So the protocols live HERE, in net-stack, over raw frames - not in the
//! driver. This is Commandment X: the driver is mechanism (put bytes on the wire), the protocol is
//! policy (what the bytes mean), and they live in different services.
//!
//! Phase 2 progress:
//!  - step 1: ARP - resolve the QEMU user-net gateway (10.0.2.2) to its hardware address.
//!  - step 2 (this commit): ICMP - PING the gateway. Build an ICMP echo request inside an IPv4 packet
//!    inside an Ethernet frame (to the MAC ARP just resolved), send it THROUGH nic-driver, and read
//!    back the echo REPLY. That is the networking analogue of v1's ping/pong milestone: a request
//!    goes out on the wire and a real reply comes back - three protocol layers, all in net-stack, all
//!    over the capability-mediated frame interface. UDP + the socket capability build on this next.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message, DeadlineOutcome, CapHandle};

// Our MAC is LEARNED from the NIC, never hardcoded (audit U9 / Commandment III). The controller's
// burned-in MAC is the one source of truth for our hardware identity; nic-driver reads it (RTL8168
// IDR0-5 / e1000 RAL0-RAH0) and returns it in the `[3]` status reply, and `learn_our_mac` threads it
// through the frame builders as `our_mac`. Previously a hardcoded QEMU default (52:54:00:12:34:56) rode
// on the driver's promiscuous RX forgiving a spoofed source; advertising the real MAC is what every
// real stack does and drops the second source of truth. A zero MAC (no NIC) => stay unconfigured.
// QEMU user-net: the guest is 10.0.2.15, the virtual gateway (which answers ARP + ICMP) is 10.0.2.2.
const FALLBACK_IP: [u8; 4] = [10, 0, 2, 15]; // used ONLY if DHCP returns no offer (no NIC)
const GATEWAY_IP:  [u8; 4] = [10, 0, 2, 2];

/// The 16-bit one's-complement checksum used by IPv4 and ICMP (RFC 1071): sum the 16-bit big-endian
/// words, fold the carries, invert. The field being covered must be zero when this is computed.
/// TCP lives in its own module: `docs/tcp-design.md` explains why, and it keeps the protocol
/// separable from the request/response services around it.
mod tcp;

fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += ((data[i] as u32) << 8) | (data[i + 1] as u32);
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Bounded dance (§26.7): a frame round-trip is a synchronous call that blocks until nic-driver
/// replies. A driver with no working TX/RX (Stage A) may never answer a *frame* (even while it answers
/// other requests), so the dance uses a wall-clock deadline + a finite retry - a silent driver DEGRADES
/// the dance instead of wedging the whole service before it can serve (the T630 hang). The call returns
/// the instant a reply arrives (QEMU is unaffected); the deadline only bounds the no-reply case.
const DANCE_SECS:  i64 = 2;
/// Short deadline for INTERACTIVE nic-driver queries - the link check and the ICMP echo wait. A healthy
/// nic-driver answers in ms and a live 8.8.8.8 reply is ~15 ms, so 1 s is generous; the point is that when
/// the cable is unplugged (the nic-driver goes slow on RDU-recovery) `ping` gives up in ~1 s per query and
/// shows "no link" fast, instead of each query stalling at the 2 s DANCE budget. The boot DANCE keeps 2 s.
const LINK_SECS:   i64 = 1;
/// A ping reply may be delivered a ping or two behind the request that produced it (residual RX
/// delivery lag), so a reply matches the current seq OR one within this small BACKWARD window - the set
/// of still-outstanding echoes. Small enough that a stale reply from a dead link still cannot match.
const SEQ_MATCH_WINDOW: u16 = 4;
// A few tries per step: on a LIVE network the first frame back can be a background broadcast, so a step
// retries past stray frames (each retry is fast - a frame is already waiting) to find its real reply.
const DANCE_TRIES: u32 = 6;
// DNS collects frames after ONE query TX (the [4] RX-only path): up to this many frames pulled without
// re-transmitting, so a reply behind stray broadcasts is caught (a re-TX would drain+discard it).
const DNS_RX_TRIES: u32 = 12;
// (PING_RX_TRIES removed: the "look past a stray broadcast" retry loop is replaced by nic-driver's [9]
// BATCH RX drain - one bounded round-trip that returns several frames for `ping` to scan, instead of N
// slow per-frame re-queries. See `ping` and the BATCH_MAX doc in services/nic-driver.)
/// Max ICMP echo DATA bytes `ping` will send (the Windows default is 32). Bounds the frame buffer.
const PING_MAX_PAYLOAD: usize = 1024;

/// Send a request to nic-driver and await the reply, RECOVERING from a nic-driver restart (audit M3).
/// If the cached send cap has gone stale - nic-driver was killed and respawned (a real event:
/// `chaos max-carnage nic-driver`), so its endpoint generation bumped - the first send fails and the
/// deadline wait returns `None`. We then reacquire the driver by name from the kernel directory
/// (§14.3, the same recovery `dhcp_discover`/`udp_roundtrip` already do in their loops) and retry once.
/// Returns `None` only if the driver is genuinely absent or silent past the deadline. Use this for the
/// FIRST request of each interactive path (`link_is_up`/`ping`/`dns`/`arp`); the poll-loop requests
/// that follow reuse the now-reacquired cached cap. Without it a configured stack never self-heals
/// after a driver restart on the ping/net/dns/arp surface - it needs a manual `net renew`. Because the
/// reply is `request_with_reply` under the hood, a driver that dies mid-request wakes us with
/// `ReplyDead` (never a hang), and the reacquire fixes a *stale* cap the fast-fail send exposes.
/// A STATUS query (op 3: MAC + link), with the receive channel cleared first.
///
/// Only status queries do this, and the distinction matters. When a `nic_req` times out its reply is
/// not cancelled - the driver still answers, and that answer arrives later, unread, so the NEXT request
/// takes it as its own and every request after that is one question out of step. On hardware that
/// showed as `learn_our_mac` reading a link-status reply, finding zeros where a MAC should be, and
/// net-stack reporting "no NIC MAC yet (driver absent/not ready)" while the driver was up and had
/// logged the MAC at boot.
///
/// **Why not on every request.** A driver reply to op 9 CARRIES RECEIVED FRAMES. Discarding one to
/// clear the channel throws away packets - which is what happened when this drain sat in the shared
/// `nic_req`: DHCP completed (OFFER, then ACK, the lease is ours) and the very next step, ARP for the
/// gateway, got no reply, because the batch holding it had been drained as "stale". The desync is worth
/// clearing; the frames are not worth losing. So the clear is confined to the query whose answer is
/// pure state and whose loss costs nothing.
///
/// A client request carries a reply cap; a driver reply does not. That is what separates them here -
/// a distinction the protocol already makes. The STALE DRIVER REPLY is what this clear exists to
/// discard, so it is discarded. A CLIENT REQUEST is dropped with its cap reclaimed and counted, which
/// is `docs/net-tags-design.md` phase-2 behaviour: it times out and retries, which is defined and
/// recoverable, where consuming it corrupts both sides silently.
fn nic_status_req(ctx: &ServiceContext, pending: &mut Displaced, msg: &Message, secs: i64) -> Option<Message> {
    while let Some(m) = ctx.try_recv() {
        // Read the badge for EVERY message, not only the ones with a reply cap. It reads and CLEARS,
        // so a badge left unread here would still be sitting there when the next message arrives and
        // would be attributed to it - a socket invocation misread onto an unrelated request.
        let badge = ctx.last_recv_badge();
        if let Some(cap) = ctx.take_pending_cap() {
            pending.note(ctx, &m, badge, cap);
        }
    }
    nic_req(ctx, pending, msg, secs)
}

// (The NIC_BUSY_MS / NIC_BUSY_TRIES pacing that lived here is GONE. It existed because a burst of
// DHCP REQUESTs filled nic-driver's 16-deep queue faster than the driver was scheduled to drain it,
// and a full queue meant a DROPPED frame the caller had to re-offer. `nic_req` now goes through the
// kernel's `CallDeadline`, which BLOCKS the send until the peer has room rather than dropping it, so
// there is nothing left to pace: congestion is handled by the primitive instead of by a retry loop
// that could give up and report a frame as refused.)

/// A busy nic-driver is congestion, not absence: back off briefly and re-ask, up to a bound.
const NIC_BUSY_MS: u64 = 2;
const NIC_BUSY_TRIES: u32 = 8;

fn nic_req(ctx: &ServiceContext, pending: &mut Displaced, msg: &Message, secs: i64) -> Option<Message> {
    // `request_with_reply_deadline_outcome`, NOT the `Call` primitive. Switching this to `Call`
    // during the x86 work is what stopped Pi 2 networking, and it was isolated by elimination on
    // hardware: with `Call`, nic-driver answers with an EMPTY status - no MAC, no link - so
    // net-stack never configures, DHCP never runs and every ping dies. Measured on the same board
    // and cable, with the drain loops and the serving otherwise identical: `Call` gives 0 frames
    // addressed to us, this gives 70 (2 ARP, 68 IPv4), a lease, and replies.
    //
    // WHY it fails is NOT yet established, and this comment will not pretend otherwise. `Call` is
    // the more correct primitive in principle - it dequeues the reply matched to its reply cap,
    // instead of taking whatever is next and possibly eating a client's request (CLAUDE.md §8.2).
    // Something about it does not hold on this path, and finding out is worth doing properly rather
    // than guessing at, which has already cost a day here.
    //
    // The x86 reason for the `Call` switch was that a plain recv could consume an unrelated client
    // request. That hazard is real, so this is a KNOWN debt, not a clean revert: recorded here
    // rather than closed (§26.7).
    for _ in 0..NIC_BUSY_TRIES {
        match sifted_req(ctx, pending, msg, secs) {
            DeadlineOutcome::Reply(r) => return Some(r),
            DeadlineOutcome::QueueFull => {
                ctx.sleep(ctx.duration_cycles(NIC_BUSY_MS));
                continue;
            }
            DeadlineOutcome::SendFailed if ctx.reacquire_by_name("nic-driver") =>
                return match sifted_req(ctx, pending, msg, secs) {
                    DeadlineOutcome::Reply(r) => Some(r),
                    _ => None,
                },
            // A TIMEOUT ALSO DESERVES A REACQUIRE, and this is where nine net-stack restarts went
            // wrong. Only SendFailed reacquired, so once nic-driver restarted and this cap died in a
            // way that presents as silence rather than a send error, every later query returned None
            // - `link_is_up` folds that into "no link", net-stack logged "no link at boot ... staying
            // unconfigured" seven times, and it then waited for a link-up EDGE that had already
            // happened while the cable never moved. One DHCP ACK across nine restarts.
            //
            // §14.3 puts reacquisition on the client, and the shell needed exactly this for its own
            // net-stack and nic-driver queries. Once per call, and only after the deadline has
            // already passed, so a healthy path is untouched: this is recovery, not a retry loop.
            _ => {
                if ctx.reacquire_by_name("nic-driver") {
                    return match sifted_req(ctx, pending, msg, secs) {
                        DeadlineOutcome::Reply(r) => Some(r),
                        _ => None,
                    };
                }
                return None;
            }
        }
    }
    None
}

/// One request to `nic-driver`, waited for WITHOUT believing the first message that lands.
///
/// This is the whole of phase 3 in eight lines, and the rest of the change is threading a `&mut` to
/// where they can run. The SDK's ordinary bounded request returns whatever arrives next, so a client
/// that spoke during the wait was taken as the driver's answer; the sifting variant asks this closure
/// about each message first.
///
/// **The discriminator is one the protocol already makes**: a CLIENT request carries a reply
/// capability, because the client wants an answer. A DRIVER reply does not, because it IS one. No
/// wire change, no tag byte, no shift of every existing field by one - which matters, because
/// `docs/net-tags-design.md` sizes the tag at forty edit points whose failure mode is "a
/// plausible-looking wrong value, not a crash". The tag is still owed for the case this cannot see
/// (two outstanding driver requests, which nothing currently makes); it is not owed for this one.
fn sifted_req(ctx: &ServiceContext, pending: &mut Displaced, msg: &Message, secs: i64) -> DeadlineOutcome {
    ctx.request_with_reply_deadline_sifted("nic-driver", msg, secs, |m| {
        // Read and CLEAR the badge for every message, so a socket invocation's badge cannot survive
        // to be attributed to whatever arrives next.
        let badge = ctx.last_recv_badge();
        match ctx.take_pending_cap() {
            // A CLIENT. Drop it, with its capability reclaimed so the table slot does not leak
            // (§8.5), and tell the wait this is not what it asked for.
            //
            // Dropped rather than kept, and that is a MEASURED decision rather than a shortcut.
            // Keeping it - phase 3 of `docs/net-tags-design.md` - was built first and taken out
            // again, because a request answered LATE is worse than one never answered: a client that
            // gives up RE-SENDS, so the late answer is a second reply to a question already asked
            // again, and the client reads it as the answer to its NEXT request. Every exchange
            // afterwards is permanently one behind. The shell log showed exactly that - a DNS lookup
            // displaced twice, answered twice, and a `net` status two commands later answered with a
            // hostname.
            //
            // A hold bound does not close it either. The hold can be short and the SERVE still long:
            // take a displaced lookup after 400 ms, spend three seconds resolving it, and the reply
            // lands after the client's deadline anyway. What actually closes it is correlation on the
            // CLIENT hop - a tag the client can use to recognise an answer to a question it is no
            // longer asking, which is what `fs` carries and what this hop does not. That is the real
            // prerequisite for phase 3, and it is recorded rather than half-built (§26.7).
            //
            // So the behaviour here is phase 2, which the design note explicitly sanctions ("ship it
            // here if phase 3 has to wait"): the client times out and retries, exactly one request is
            // ever outstanding, and the outcome is defined, loud and recoverable. What is NEW is that
            // it now happens on EVERY driver conversation instead of one of them - before this, a
            // client met during an ordinary `nic_req` was not dropped but CONSUMED, parsed as a link
            // status or a frame batch, and silently mis-served.
            Some(cap) => { pending.note(ctx, m, badge, cap); false }
            // No reply cap: the driver's answer. **THIS IS THE ONE SILENT WAY A CLIENT REQUEST CAN
            // BE LOST, AND IT IS THE LAST ONE LEFT UNINSTRUMENTED.** A driver reply carries no reply
            // cap - and neither does a client request whose cap has already been consumed, so the two
            // are indistinguishable here. Taking the second for the first hands a client's request to
            // `nic_req` as if it were a frame batch, which parses it, discards it, and says nothing:
            // every other loss path in this service (stash full, stash expired, no reply cap at
            // dispatch) reports itself, and this one does not.
            //
            // A client op is a poor proxy for "this is a client" in general, but 21 (tcp transact) and
            // 22 (tcp listen) are decisive ENOUGH to name the case: a frame batch or a link status
            // that happens to begin with either is possible, and it is far likelier that this is the
            // request a shell is currently blocked on. Said once, like its siblings.
            //
            // Hunting a Wyse `tcp ... big` that net-stack never logged while it kept answering ping
            // (backlog/29). Every other candidate was eliminated by reading; this one cannot be, so it
            // is being measured rather than assumed.
            None => {
                if matches!(m.payload_bytes().first(), Some(&21) | Some(&22))
                    && !pending.ate_client_said
                {
                    pending.ate_client_said = true;
                    ctx.log_fmt(format_args!(
                        "net-stack: took a capless message beginning {} as the driver's answer - if a                          client is blocked right now, THIS is where its request went (said once)",
                        m.payload_bytes().first().copied().unwrap_or(0)));
                }
                true
            }
        }
    })
}

/// A client's reply capability, bound together with the correlation tag its request carried.
///
/// **A separate TYPE rather than a second variable, and that is the whole safety argument.** Thirteen
/// places in the serve loop answer a client. Adding a tag byte by hand at each of them is the failure
/// `docs/net-tags-design.md` warns about in capitals - "a plausible-looking wrong value, not a crash"
/// - because a site that is missed still compiles, still sends, and is simply one byte out for the
/// rest of time. Changing the TYPE of what a reply is sent through makes every one of those sites a
/// build error until it is converted, so the compiler enumerates them instead of me.
///
/// The tag is `None` for the paths that are deliberately untagged, which is the same split `fs` makes:
/// a BADGED invocation is a capability calling its own owner and the badge already says which
/// resource, so there is nothing to correlate. `fs`'s note for the identical case reads "badged
/// file-cap invocations take the other path and are untagged".
#[derive(Clone, Copy)]
struct Reply {
    cap: CapHandle,
    tag: Option<u8>,
}

impl Reply {
    /// Answer the client, putting the tag back at byte 0 where it came from.
    ///
    /// `#[inline(never)]`: this carries a 4 KiB buffer, and inlining it into the serve loop would add
    /// that to `service_main`'s frame at every one of the thirteen call sites. The SDK has already
    /// paid for that mistake once - see `await_slice`, where a one-line wrapper returning a `Message`
    /// by value cost `fs` a stack frame per request and took it over its limit on hardware.
    /// Answering with NOTHING is not possible, so it is not allowed to be attempted.
    ///
    /// The kernel's `validate_user_ptr` rejects a zero-length buffer, so a zero-length send fails
    /// and the reply never leaves - the caller then waits out its entire deadline for a message that
    /// could not have been sent. On a Pi 4 that was five seconds on every `serve` close, while the
    /// close itself had worked (net-stack reaped the connection 400 ms later).
    ///
    /// Debug-asserted rather than silently padded: a caller that means "nothing" should say so with
    /// a byte that means it, because the receiver has to distinguish "no data" from "no answer"
    /// anyway. Padding here would hide the decision instead of forcing it.
    #[inline(never)]
    fn send(&self, ctx: &ServiceContext, body: &[u8]) {
        debug_assert!(!body.is_empty(),
            "a zero-length reply cannot be sent - the kernel refuses it and the caller hangs");
        match self.tag {
            None => { let _ = ctx.try_send_by_handle(self.cap, &Message::from_bytes(body)); }
            Some(t) => {
                let mut out = [0u8; 4096];
                let n = body.len().min(out.len() - 1);
                out[0] = t;
                out[1..1 + n].copy_from_slice(&body[..n]);
                let _ = ctx.try_send_by_handle(self.cap, &Message::from_bytes(&out[..1 + n]));
            }
        }
    }

    /// Answer the client with a capability embedded (the socket `open` path). `true` if it was sent.
    #[inline(never)]
    fn send_with_cap(&self, ctx: &ServiceContext, granted: CapHandle, body: &[u8]) -> bool {
        match self.tag {
            None => ctx.send_with_cap_by_handle(self.cap, granted, &Message::from_bytes(body)).is_ok(),
            Some(t) => {
                let mut out = [0u8; 64];
                let n = body.len().min(out.len() - 1);
                out[0] = t;
                out[1..1 + n].copy_from_slice(&body[..n]);
                ctx.send_with_cap_by_handle(self.cap, granted, &Message::from_bytes(&out[..1 + n])).is_ok()
            }
        }
    }

    /// Reclaim the capability once the request is answered (§8.5 - an unreclaimed slot leaks).
    fn done(&self, ctx: &ServiceContext) { ctx.remove_cap(self.cap); }
}

/// How many client requests were displaced by a conversation with `nic-driver`, and dropped.
///
/// A counter and a once-only report, not a queue. It exists because the drop is the one cost of
/// serving on the same endpoint replies arrive on, and a cost nobody can measure is indistinguishable
/// from one that is not happening: this is what would tell an operator that the correlation work in
/// `docs/net-tags-design.md` had become worth doing.
///
/// Threaded by `&mut` through everything that talks to the driver rather than kept in a static,
/// because a service holds no unowned global mutable state (Commandment VI). That threading is most
/// of this change, and it is the same `&mut` phase 3 will need when it arrives.
pub struct Displaced {
    /// Every client request this service has thrown away, for any reason. Drops are reported by RATE
    /// off this count rather than by a said-once latch - see `take`.
    n: u32,
    /// Said-once latch for the one SILENT way a client request can be lost - see the `None` arm of
    /// `sifted_req`'s closure.
    ate_client_said: bool,

    /// Client requests displaced by a conversation with `nic-driver`, kept in arrival order.
    ///
    /// **This is `docs/net-tags-design.md` phase 3, and it is safe now for one reason: the CLIENT HOP
    /// CARRIES A CORRELATION TAG.** It was built without one, measured, and withdrawn the same day
    /// (§7.2 there, with the log). The hazard was not the holding - it was answering LATE: a client
    /// that gives up RE-SENDS, so a late answer arrived as a second reply to a question already asked
    /// again, was read as the answer to the NEXT question, and every exchange afterwards was
    /// permanently one behind. A DNS lookup displaced twice, served twice, and a `net` status two
    /// commands later answered with a hostname.
    ///
    /// A tag removes that entirely. The re-send carries a FRESH tag, so when the held copy is finally
    /// answered the client sees a tag it is not waiting for, discards it, and keeps waiting for its
    /// own. The two replies stop being interchangeable, which is the whole property that was missing.
    ///
    /// So the gate that scoped holding to net-stack's own unsolicited work is GONE, and holding is
    /// now unconditional. The distinction it drew was real while a late answer could mislead; once it
    /// cannot, it only costs work that would otherwise be lost.
    held: [Option<Held>; STASH_N],
    /// Index of the oldest live entry. A ring, so serving in ARRIVAL ORDER costs nothing - clients
    /// are answered in the order they asked, which is the only order that cannot surprise them.
    head: usize,
    live: usize,
}

/// A held request, with everything needed to answer it later.
///
/// All three are captured at the moment of arrival and none can be recovered afterwards: the badge
/// and the pending capability are per-task kernel state describing THE MESSAGE JUST RECEIVED
/// (`last_recv_badge` reads and clears it, `take_pending_cap` pops a FIFO), so both are overwritten
/// by whatever lands next.
pub struct Held {
    len: usize,
    badge: Option<(u64, u8)>,
    reply: CapHandle,
    /// The cycle counter when it was displaced. See `HOLD_MS`.
    at: u64,
    /// How long THIS request's client said it will wait. See `HOLD_MS`.
    hold_ms: u64,
    body: [u8; HELD_BYTES],
}

/// How many displaced requests are kept.
///
/// Four. The endpoint queue behind this is 16 deep (CLAUDE.md §8.5), and a stash as deep as the queue
/// would just be the queue again in this service's stack - four slots is 4 KiB of `service_main`'s
/// frame and covers the realistic case, which is one client re-sending into one busy moment.
const STASH_N: usize = 4;

/// The largest request body that can be held. A status query is two bytes, a DNS lookup a hostname, a
/// TCP transact a shell command line. A socket send may legitimately be larger, and one that does not
/// fit is dropped rather than truncated - a request half-kept would be served as a DIFFERENT request.
const HELD_BYTES: usize = 1024;

/// How long a held request may wait before it is dropped instead of answered.
///
/// **The bound is set by LATENCY, not by correctness, and getting that backwards is a measured
/// mistake rather than a hypothetical one.** Before the client hop carried a tag, answering late
/// corrupted the channel and this was the only thing preventing a permanent desync. The tag removed
/// that: a late reply now carries a tag the client is not waiting for and is discarded.
///
/// So the first version of this constant after the tag was widened to 3 s - the shortest client
/// deadline - on the reasoning that anything inside it was safe. It IS safe, and it is slower. A
/// request held for 2.9 s is still served, by which time the client has given up at 3.0 s and
/// re-sent; net-stack then does the work TWICE and the duplicate delays the copy that is actually
/// wanted. The shell suite went from zero `net-stack unavailable` to one, with the same 174/0 either
/// way - a regression only the before/after comparison showed.
///
/// The bound must therefore be well UNDER the shortest client deadline, not equal to it, so that a
/// held request is either served promptly (which is the whole point) or abandoned early enough that
/// only the re-send is served. Half a second against a three-second deadline leaves the client five
/// times the hold to still be waiting.
///
/// On a board whose cycle counter is not calibrated, `duration_cycles` floors to one quantum
/// (`backlog/27`), so the budget collapses and every held request expires at once - the stack then
/// behaves as it did before the stash existed, which is the right way for it to fail.
const HOLD_MS: u64 = 1_500;

impl Displaced {
    fn new() -> Self {
        Displaced {
            n: 0,
            ate_client_said: false,
            held: [const { None }; STASH_N],
            head: 0,
            live: 0,
        }
    }

    /// A client request met during a driver conversation: keep it, so the work is not lost.
    fn note(&mut self, ctx: &ServiceContext, m: &Message, badge: Option<(u64, u8)>, cap: CapHandle) {
        let pl = m.payload_bytes();
        if pl.len() > HELD_BYTES {
            // REFUSED, not truncated. See HELD_BYTES.
            self.drop_one(ctx, cap, "it is larger than a stash slot");
            return;
        }
        if self.live == STASH_N {
            // Full. Evict the OLDEST - the client that has been waiting longest, and therefore the
            // one likeliest to have given up already. Its capability goes back first (§8.5).
            let old = self.head;
            if let Some(h) = self.held[old].take() {
                self.drop_one(ctx, h.reply, "the stash was full");
            }
            self.head = (self.head + 1) % STASH_N;
            self.live -= 1;
        }
        let slot = (self.head + self.live) % STASH_N;
        // THE CLIENT SAID HOW LONG IT WILL WAIT (byte 1 of a tagged request). Hold it for that,
        // rather than for one global guess - see `HOLD_MS`. A badged invocation carries no header, so
        // it gets the default. Holding longer than the client will wait is pure waste; holding for
        // less is the bug this fixes.
        let hold_ms = match badge {
            None => pl.get(1).map(|p| (*p as u64).saturating_mul(1_000)).unwrap_or(HOLD_MS),
            Some(_) => HOLD_MS,
        };
        let mut h = Held {
            len: pl.len(), badge, reply: cap, at: ctx.read_tsc(), hold_ms,
            body: [0u8; HELD_BYTES],
        };
        h.body[..pl.len()].copy_from_slice(pl);
        self.held[slot] = Some(h);
        self.live += 1;
    }

    /// Is a displaced client request waiting to be served?
    ///
    /// The wait loop asks this so it does not SLEEP on work it already has - see its call site.
    fn has_work(&self) -> bool { self.live > 0 }

    /// Take the oldest request still worth answering, for the serve loop.
    ///
    /// Entries are expired from the FRONT only, which is sound because they were kept in arrival
    /// order: once the head is young enough, so is everything behind it.
    fn take(&mut self, ctx: &ServiceContext, out: &mut [u8; HELD_BYTES])
            -> Option<(usize, Option<(u64, u8)>, CapHandle)> {
        let now = ctx.read_tsc();
        while self.live > 0 {
            let h = match self.held[self.head].take() { Some(h) => h, None => return None };
            self.head = (self.head + 1) % STASH_N;
            self.live -= 1;
            // Per request, not one budget for the whole stash - entries hold for different lengths
            // now, so each is checked against its own client's word.
            let budget = ctx.duration_cycles(h.hold_ms);
            // wrapping_sub, so a counter that wraps while something is held reads as a small elapsed
            // rather than an enormous one that expires a request which just arrived.
            if now.wrapping_sub(h.at) >= budget {
                ctx.remove_cap(h.reply);
                self.n = self.n.saturating_add(1);
                // REPORT EVERY ONE, not just the first. A "said once" latch is exactly what hid this
                // for three sessions: the first drop was reported at boot and every later one - each
                // costing a client its whole deadline - was silent, so the board looked healthy while
                // commands took twenty seconds. Bounded by RATE, not by a latch (§26.7).
                if self.n <= 8 || self.n % 8 == 0 {
                    ctx.log_fmt(format_args!(
                        // NOT "the client has re-sent" - that asserted something this service
                        // cannot know, and on the Pi 2 it was false: the client was still waiting,
                        // and this line was the only trace of why its request vanished.
                        "net-stack: dropped a held client request (op {}) after its client's own {} ms                          of patience - it is still waiting and will now time out (drop #{})",
                        h.body.get(2).copied().unwrap_or(0), h.hold_ms, self.n));
                }
                continue;
            }
            out[..h.len].copy_from_slice(&h.body[..h.len]);
            return Some((h.len, h.badge, h.reply));
        }
        None
    }

    /// Reclaim a capability for a request that will not be answered, and report the first one.
    fn drop_one(&mut self, ctx: &ServiceContext, cap: CapHandle, why: &str) {
        ctx.remove_cap(cap);
        self.n = self.n.saturating_add(1);
        if self.n <= 8 || self.n % 8 == 0 {
            ctx.log_fmt(format_args!(
                "net-stack: a client request met mid-question to nic-driver was dropped                  because {} - it times out and retries (drop #{})", why, self.n));
        }
    }
}

/// Phase 3: a DHCP DISCOVER over UDP - ask QEMU slirp's built-in DHCP server for our IP and read the
/// OFFER. This proves the UDP transport (the layer the socket capability sits on) over the frame
/// interface. Returns the offered IP, or None (no NIC / nothing answered). A real net-stack would use
/// this to LEARN its own IP instead of hardcoding it; here it demonstrates the round-trip.
/// Drain RX-ring batches ([9]) and call `on_frame` for each frame until it returns true (matched) or the
/// deadline elapses. On a busy LAN the reply arrives amid a FLOOD of broadcast, so every path that waits
/// for a specific reply must SCAN every frame, not take the one coupled frame back - the shared receive.
/// Like `drain_scan`, but RETURNS whether the closure matched.
///
/// The captured-flag form (`let mut hit = false; drain_scan(.., |f| { hit = true; true }); if hit`)
/// compiled away entirely for the DHCP ACK check: the branch and its log never reached the binary, so
/// on hardware neither the success nor the failure line ever printed and the REQUEST looked as though
/// it had never run. Returning the answer instead of writing it through a capture leaves nothing for
/// that to happen to. Verified by grepping the built ELF for the log strings.
fn serve_while_dancing(ctx: &ServiceContext, _pending: &mut Displaced, serve_status: Option<&[u8; 19]>) {
    // `None` means this wait is NOT a dance - it is the ping or DNS path, reached while already
    // handling a client request. Serving there would be re-entrant, so it does not.
    let Some(status) = serve_status else { return };
    // 16 = the per-endpoint queue depth (§8.5); draining at most that many bounds this pass.
    for _ in 0..16 {
        let Some(_req) = ctx.try_recv() else { return };
        let Some(reply) = ctx.take_pending_cap() else { continue };
        let _ = ctx.try_send_by_handle(reply, &Message::from_bytes(status));
        ctx.remove_cap(reply);
    }
}
fn drain_scan_hit(ctx: &ServiceContext, pending: &mut Displaced, secs: i64, serve_status: Option<&[u8; 19]>,
                  mut on_frame: impl FnMut(&[u8], &mut Displaced) -> bool) -> bool {
    let t0 = ctx.epoch_secs_monotonic();
    let mut empty_polls: u32 = 0;
    loop {
        let mut got_frames = false;
        if let Some(b) = nic_drain(ctx, pending) {
            let p = b.payload_bytes();
            let n = if p.is_empty() { 0 } else { p[0] as usize };
            got_frames = n > 0;
            let mut pos = 1usize;
            for _ in 0..n {
                if pos + 2 > p.len() { break; }
                let fl = u16::from_le_bytes([p[pos], p[pos + 1]]) as usize;
                pos += 2;
                if pos + fl > p.len() { break; }
                if on_frame(&p[pos..pos + fl], pending) { return true; }
                pos += fl;
            }
        }
        if ctx.epoch_secs_monotonic() - t0 >= secs { return false; }
        // SERVE AND PACE ONLY WHEN THE WIRE CAME BACK EMPTY.
        //
        // Both of these used to run on EVERY pass, including immediately after a burst arrived, and
        // that is a receive window given away mid-flow. It costs nothing on a NIC that DMAs frames
        // into a ring by itself - the frames are still there afterwards - and it is fatal on one
        // where the HOST must keep a bulk-IN outstanding, because that device's small FIFO drops
        // whatever lands in the gap. Measured on a Pi 2 (LAN9514): 3 frames collected with the FIFO
        // holding bytes, no DHCP offer ever seen, no lease, every ping dead. With this: 70 frames to
        // us, 2 ARP, 68 IPv4, lease, replies.
        //
        // An empty poll is the honest moment to do other work: there is nothing in flight to miss.
        // A non-empty one means the device is mid-burst, so go straight back for the next batch.
        //
        // Device-agnostic on purpose (§26.14). The x86 responsiveness this serving exists for is
        // preserved - an idle wire is exactly when a client is waiting - without asking net-stack to
        // know which kind of NIC it is talking to.
        if got_frames {
            empty_polls = 0;
        } else {
            serve_while_dancing(ctx, pending, serve_status);
            empty_polls += 1;
            // YIELD FIRST, SLEEP ONLY ONCE THE WIRE IS GENUINELY IDLE.
            //
            // `sleep` asks the scheduler for a MINIMUM, not a maximum. dwc2's own main loop records
            // measuring a 10 ms sleep take 3.8 SECONDS under `selfcheck`, because the service shares
            // a core with everything driving it - and nothing reads the device while we are gone. A
            // DHCP OFFER that arrives in that window is dropped by a FIFO that holds about one
            // burst, which is why the lease missed its 20 s budget and pings vanished.
            //
            // A reply usually follows the frame that prompted it, so the moments just after traffic
            // are the worst possible time to go blind. Yield for the first few empty polls - giving
            // up the core, but resuming at the next opportunity instead of adding a floor to it -
            // then fall back to the pace once the wire really has nothing.
            if empty_polls <= EMPTY_YIELDS {
                ctx.yield_cpu();
            } else {
                ctx.sleep(ctx.duration_cycles(RX_POLL_PACE_MS));
            }
        }
    }
}

/// How long to wait between RX polls of the NIC while waiting for a reply.
///
/// `drain_scan` used to re-ask with no pacing at all: `loop { nic_req(..) }` for up to two seconds,
/// which is thousands of requests a second at `nic-driver` and, behind it, at the USB driver. The
/// service waiting for a DHCP offer was saturating the two services that had to fetch it.
///
/// The SDK's wait now blocks rather than spins, which fixes the inner half. This fixes the outer half:
/// a poll is a QUESTION, and asking it ten thousand times a second does not make the answer arrive
/// sooner - it makes it arrive later, because the machinery that would produce it is busy answering.
/// 10 ms is one scheduler quantum: fast enough that a frame is picked up promptly, slow enough that
/// the driver is left alone to receive it.
const RX_POLL_PACE_MS: u64 = 10;
/// Empty polls to YIELD through before falling back to the pace sleep. See the drain loops: a reply
/// tends to follow the frame that prompted it, and a sleep here is a blackout of unpredictable
/// length, not a 10 ms one.
const EMPTY_YIELDS: u32 = 12;

fn drain_scan(ctx: &ServiceContext, pending: &mut Displaced, secs: i64, serve_status: Option<&[u8; 19]>,
              mut on_frame: impl FnMut(&[u8], &mut Displaced) -> bool) {
    let t0 = ctx.epoch_secs_monotonic();
    let mut empty_polls: u32 = 0;
    loop {
        let mut got_frames = false;
        if let Some(b) = nic_drain(ctx, pending) {
            let p = b.payload_bytes();
            let n = if p.is_empty() { 0 } else { p[0] as usize };
            got_frames = n > 0;
            let mut pos = 1usize;
            for _ in 0..n {
                if pos + 2 > p.len() { break; }
                let fl = u16::from_le_bytes([p[pos], p[pos + 1]]) as usize;
                pos += 2;
                if pos + fl > p.len() { break; }
                if on_frame(&p[pos..pos + fl], pending) { return; }
                pos += fl;
            }
        }
        if ctx.epoch_secs_monotonic() - t0 >= secs { return; }
        // Serve and pace only on an EMPTY poll - see the twin of this loop in `drain_scan_hit` for
        // why giving a receive window away mid-burst kills a host-polled NIC. On an empty poll,
        // `sleep` parks the task so the core is free for the driver trying to hand us a frame.
        if got_frames {
            empty_polls = 0;
        } else {
            serve_while_dancing(ctx, pending, serve_status);
            empty_polls += 1;
            // YIELD FIRST, SLEEP ONLY ONCE THE WIRE IS GENUINELY IDLE.
            //
            // `sleep` asks the scheduler for a MINIMUM, not a maximum. dwc2's own main loop records
            // measuring a 10 ms sleep take 3.8 SECONDS under `selfcheck`, because the service shares
            // a core with everything driving it - and nothing reads the device while we are gone. A
            // DHCP OFFER that arrives in that window is dropped by a FIFO that holds about one
            // burst, which is why the lease missed its 20 s budget and pings vanished.
            //
            // A reply usually follows the frame that prompted it, so the moments just after traffic
            // are the worst possible time to go blind. Yield for the first few empty polls - giving
            // up the core, but resuming at the next opportunity instead of adding a floor to it -
            // then fall back to the pace once the wire really has nothing.
            if empty_polls <= EMPTY_YIELDS {
                ctx.yield_cpu();
            } else {
                ctx.sleep(ctx.duration_cycles(RX_POLL_PACE_MS));
            }
        }
    }
}

/// DHCPREQUEST + DHCPACK - the half of the exchange that actually claims the address.
///
/// RFC 2131 §3.1: DISCOVER and OFFER only propose an address. The client must broadcast a REQUEST
/// naming both the address (option 50) and the server whose offer it accepts (option 54), and the
/// server must reply DHCPACK (option 53 = 5). Until that ACK, the address belongs to nobody.
///
/// Broadcast, not unicast to the server, and deliberately: at this point we still do not own the
/// address, so we cannot yet source packets from it, and the other DHCP servers on the segment need
/// to see that their offers were declined.
fn dhcp_request(ctx: &ServiceContext, pending: &mut Displaced, our_mac: &[u8; 6], ip: &[u8; 4], srv: &[u8; 4], bcast: bool,
                serve_status: Option<&[u8; 19]>) -> bool {
    // SIZED FOR ITS OWN OPTIONS. The DISCOVER's frame is 286 bytes because its option block is four
    // bytes (type + end). A REQUEST carries three options - message type (3), requested address (6),
    // server identifier (6) - plus the end byte: sixteen. Reusing 286 here wrote past the array on the
    // FIRST call, every call.
    //
    // That panic is the whole reason this never worked, and it hid behind two symptoms I misread. The
    // service died and was restarted six times a boot, so no REQUEST ever left the host - and because
    // the compiler can PROVE the index is out of bounds, it correctly deleted everything after it: the
    // ACK branch, its log, and the caller's failure log. I spent an hour treating that as an
    // inscrutable optimiser quirk. It was the compiler telling me the code was wrong.
    //
    // 42 header bytes + 236 BOOTP + 4 magic cookie + 16 options = 298.
    const REQ_LEN: usize = 298;
    const DHCP_LEN: usize = REQ_LEN - 42;          // BOOTP + cookie + options, as UDP carries it
    let mut frame = [0u8; REQ_LEN];
    for b in frame[0..6].iter_mut() { *b = 0xff; }
    frame[6..12].copy_from_slice(our_mac);
    frame[12] = 0x08; frame[13] = 0x00;
    frame[14] = 0x45; frame[15] = 0x00;
    // Lengths follow the ACTUAL frame, not the DISCOVER's constants. A REQUEST is longer, and a header
    // that understates its payload is a packet a router is entitled to drop without comment.
    let total: u16 = (20 + 8 + DHCP_LEN) as u16;
    frame[16] = (total >> 8) as u8; frame[17] = total as u8;
    frame[22] = 64;
    frame[23] = 17;
    for b in frame[30..34].iter_mut() { *b = 0xff; }
    let ip_ck = checksum(&frame[14..34]);
    frame[24] = (ip_ck >> 8) as u8; frame[25] = ip_ck as u8;
    frame[34] = 0; frame[35] = 68;
    frame[36] = 0; frame[37] = 67;
    let udp_len: u16 = (8 + DHCP_LEN) as u16;
    frame[38] = (udp_len >> 8) as u8; frame[39] = udp_len as u8;
    frame[42] = 1; frame[43] = 1; frame[44] = 6;
    // The SAME xid as the DISCOVER: a REQUEST continues that transaction, and a server matches it by
    // this field. A fresh xid here reads as an unrelated client and is ignored.
    frame[46] = 0x39; frame[47] = 0x03; frame[48] = 0xf3; frame[49] = 0x26;
    frame[52] = if bcast { 0x80 } else { 0x00 };         // see `dhcp_lease`
    frame[70..76].copy_from_slice(our_mac);
    frame[278] = 0x63; frame[279] = 0x82; frame[280] = 0x53; frame[281] = 0x63;
    let mut o = 282usize;
    frame[o] = 53; frame[o + 1] = 1; frame[o + 2] = 3; o += 3;            // message type = REQUEST
    frame[o] = 50; frame[o + 1] = 4;                                      // requested IP address
    frame[o + 2..o + 6].copy_from_slice(ip); o += 6;
    frame[o] = 54; frame[o + 1] = 4;                                      // server identifier
    frame[o + 2..o + 6].copy_from_slice(srv); o += 6;
    frame[o] = 255;                                                       // end

    let req = Message::from_bytes(&frame);
    let mut send_fail = 0u32;
    for _ in 0..DANCE_TRIES {
        // A REQUEST that never left is not a server that did not ACK - see `dhcp_discover`.
        if nic_req(ctx, pending, &req, LINK_SECS).is_none() { send_fail += 1; }
        // SAY WHY A REPLY WAS REJECTED. The server ACKs - the wire capture shows seven - and this scan
        // finds none, so one of the field tests is refusing a frame that really is there. Counting
        // what arrives and reporting the first BOOTREPLY's type and yiaddr names the wrong test
        // instead of inviting a fifth guess at it. Owned locals captured by the closure, no statics.
        let mut seen = 0u32;
        let mut replies = 0u32;
        let mut first_reply: Option<(u8, [u8; 4])> = None;
        let acked = drain_scan_hit(ctx, pending, DANCE_SECS, serve_status, |f, _| {
            seen += 1;
            // A BOOTREPLY carrying option 53 = 5 (DHCPACK) for the address we asked for. A NAK (6) is
            // a definite refusal and is treated as "not acknowledged" by simply not matching - the
            // caller re-DISCOVERs, which is what RFC 2131 asks of a NAKed client anyway.
            let is_bootp_reply = f.len() >= 62 && f[12] == 0x08 && f[13] == 0x00 && f[14] == 0x45
                && f[23] == 17 && f[42] == 2;
            if !is_bootp_reply { return false; }
            replies += 1;
            let mut mtype = 0u8;
            let mut o = 282usize;
            while o + 1 < f.len() {
                let opt = f[o];
                if opt == 255 { break; }
                if opt == 0 { o += 1; continue; }
                let len = f[o + 1] as usize;
                if opt == 53 && len >= 1 && o + 2 < f.len() { mtype = f[o + 2]; }
                o += 2 + len;
            }
            let yi = [f[58], f[59], f[60], f[61]];
            if first_reply.is_none() { first_reply = Some((mtype, yi)); }
            mtype == 5 && yi == *ip
        });
        if acked {
            ctx.log_fmt(format_args!(
                "net-stack: DHCP - ACK, {}.{}.{}.{} is ours (server {}.{}.{}.{})",
                ip[0], ip[1], ip[2], ip[3], srv[0], srv[1], srv[2], srv[3]));
            return true;
        }
        if !acked {
            ctx.log_fmt(format_args!(
                "net-stack: DHCP - no ACK matched: {} frames, {} BOOTP replies, first type {} \
                 yiaddr {}.{}.{}.{} (wanted type 5, yiaddr {}.{}.{}.{})",
                seen, replies,
                first_reply.map_or(0, |(t, _)| t),
                first_reply.map_or(0, |(_, y)| y[0]), first_reply.map_or(0, |(_, y)| y[1]),
                first_reply.map_or(0, |(_, y)| y[2]), first_reply.map_or(0, |(_, y)| y[3]),
                ip[0], ip[1], ip[2], ip[3]));
        }
    }
    if send_fail > 0 {
        ctx.log_fmt(format_args!(
            "net-stack: DHCP - no ACK, and {} of {} REQUESTs never left the host - the driver refused them, so this is not a silent server",
            send_fail, DANCE_TRIES));
    }
    false
}

/// Get a lease, asking for a UNICAST reply first and falling back to broadcast only if that fails.
///
/// The BOOTP flags word has one meaningful bit: set it and the server must answer by broadcast, clear it
/// and the server answers by unicast to our MAC. This client set it unconditionally, on both the DISCOVER
/// and the REQUEST, and that turned out to be hiding a fault rather than avoiding one.
///
/// RFC 2131 4.4.1 says to set the bit only when the client cannot receive a unicast datagram before its
/// address is configured. Ours can: the reply is matched on the BOOTP reply opcode, not on the
/// destination IP, so the frame is usable whatever address it was sent to. So the normal client
/// behaviour - and the one that tells us something - is to leave it clear.
///
/// WHY IT MATTERS HERE. DHCP is the first exchange the machine completes and the only one it needs to
/// reach the network at all, so it is where a broken receive path should show first. Asking for a
/// broadcast reply made it the one exchange that could NOT show it: on this board DHCP succeeded while
/// every unicast exchange failed - ARP never resolved, ping always timed out - because every frame the
/// port had ever received was broadcast. The lease made the network look present and left the actual
/// failure to surface three layers up as "request timed out". A test that cannot fail is not a test.
///
/// So: unicast first. If that gets no offer and broadcast does, the difference is not a network problem,
/// it is this port refusing frames addressed to itself, and the fallback SAYS SO rather than quietly
/// restoring service and leaving the fault to be rediscovered. The fallback exists because losing the
/// network is not an acceptable price for the diagnosis - but a fallback nobody is told about is the
/// silent kind this system does not allow.
fn dhcp_lease(ctx: &ServiceContext, pending: &mut Displaced, our_mac: &[u8; 6],
              serve_status: Option<&[u8; 19]>) -> Option<([u8; 4], [u8; 4], [u8; 4])> {
    if let Some(cfg) = dhcp_discover(ctx, pending, our_mac, false, serve_status) {
        return Some(cfg);
    }
    ctx.log("net-stack: DHCP got no reply addressed to us - retrying and asking the server to broadcast");
    let cfg = dhcp_discover(ctx, pending, our_mac, true, serve_status)?;
    ctx.log("net-stack: DHCP succeeded ONLY with a broadcast reply - this port is not receiving frames addressed to its own MAC, so ARP and ping cannot work until that is fixed");
    Some(cfg)
}

fn dhcp_discover(ctx: &ServiceContext, pending: &mut Displaced, our_mac: &[u8; 6], bcast: bool,
                 serve_status: Option<&[u8; 19]>) -> Option<([u8; 4], [u8; 4], [u8; 4])> {
    let mut send_fail = 0u32;
    // Ethernet(14) + IPv4(20) + UDP(8) + DHCP/BOOTP(244) = 286 bytes.
    let mut frame = [0u8; 286];
    for b in frame[0..6].iter_mut() { *b = 0xff; }       // eth dest = broadcast
    frame[6..12].copy_from_slice(our_mac);               // eth src
    frame[12] = 0x08; frame[13] = 0x00;                  // ethertype = IPv4
    // IPv4 header.
    frame[14] = 0x45; frame[15] = 0x00;
    let total: u16 = 20 + 8 + 244;                       // 272
    frame[16] = (total >> 8) as u8; frame[17] = total as u8;
    frame[22] = 64;                                      // TTL
    frame[23] = 17;                                      // protocol = UDP
    for b in frame[30..34].iter_mut() { *b = 0xff; }     // dst = 255.255.255.255 (src 0.0.0.0 = zero)
    let ip_ck = checksum(&frame[14..34]);
    frame[24] = (ip_ck >> 8) as u8; frame[25] = ip_ck as u8;
    // UDP header (src port 68 bootpc, dst port 67 bootps; checksum 0 = optional over IPv4).
    frame[34] = 0; frame[35] = 68;
    frame[36] = 0; frame[37] = 67;
    let udp_len: u16 = 8 + 244;                          // 252
    frame[38] = (udp_len >> 8) as u8; frame[39] = udp_len as u8;
    // DHCP / BOOTP.
    frame[42] = 1;                                       // op = BOOTREQUEST
    frame[43] = 1;                                       // htype = Ethernet
    frame[44] = 6;                                       // hlen
    frame[46] = 0x39; frame[47] = 0x03; frame[48] = 0xf3; frame[49] = 0x26; // xid (arbitrary)
    frame[52] = if bcast { 0x80 } else { 0x00 };         // see `dhcp_lease`
    frame[70..76].copy_from_slice(our_mac);              // chaddr (client hardware address)
    frame[278] = 0x63; frame[279] = 0x82; frame[280] = 0x53; frame[281] = 0x63; // DHCP magic cookie
    frame[282] = 53; frame[283] = 1; frame[284] = 1;     // option 53 (message type) = DISCOVER
    frame[285] = 255;                                    // option end

    let req = Message::from_bytes(&frame);
    for _ in 0..DANCE_TRIES {
        // Send the DISCOVER, then DRAIN + SCAN the RX ring for the OFFER: on a busy LAN the offer arrives
        // amid a flood of broadcast, so we scan every frame within the budget, not just the coupled one.
        // COUNT A SEND THAT NEVER LEFT. Discarding this outcome makes "no offer" mean two different
        // things - the server was silent, or we never asked - and they need opposite fixes (§26.7).
        if nic_req(ctx, pending, &req, LINK_SECS).is_none() { send_fail += 1; }
        let mut found: Option<([u8; 4], [u8; 4], [u8; 4], [u8; 4])> = None;
        drain_scan(ctx, pending, DANCE_SECS, serve_status, |f, _| {
            // A DHCP reply: IPv4 (0x0800, IHL 5), UDP (proto 17), BOOTP op = 2 (BOOTREPLY). yiaddr (our
            // offered IP) sits at BOOTP offset 16 = frame offset 58.
            if f.len() >= 62 && f[12] == 0x08 && f[13] == 0x00 && f[14] == 0x45 && f[23] == 17 && f[42] == 2 {
                let ip = [f[58], f[59], f[60], f[61]];
                // Learn the GATEWAY from the offer's options (magic cookie at frame offset 278 -> options
                // at 282), option 3 = router. This is what makes it work on a REAL network (the gateway is
                // 192.168.x.1, not QEMU's 10.0.2.2). Fall back to <subnet>.1.
                let mut gw = [ip[0], ip[1], ip[2], 1];
                let mut dns = [0u8; 4];
                let mut have_dns = false;
                let mut srv = [0u8; 4];
                let mut o = 282usize;
                while o + 1 < f.len() {
                    let opt = f[o];
                    if opt == 255 { break; }          // options end
                    if opt == 0 { o += 1; continue; } // pad
                    let len = f[o + 1] as usize;
                    if opt == 3 && len >= 4 && o + 6 <= f.len() { gw = [f[o + 2], f[o + 3], f[o + 4], f[o + 5]]; }
                    if opt == 6 && len >= 4 && o + 6 <= f.len() { dns = [f[o + 2], f[o + 3], f[o + 4], f[o + 5]]; have_dns = true; }
                    // Option 54, the SERVER IDENTIFIER. A REQUEST must name the server whose offer it
                    // is accepting, or every DHCP server on the segment has to guess whether it was
                    // chosen. We never sent a REQUEST at all, so this was never needed - and never
                    // read.
                    if opt == 54 && len >= 4 && o + 6 <= f.len() { srv = [f[o + 2], f[o + 3], f[o + 4], f[o + 5]]; }
                    o += 2 + len;
                }
                if !have_dns { dns = gw; }            // no DNS option: the gateway usually forwards DNS
                // WHAT DID WE ACTUALLY GET? Placed in THIS closure deliberately: it is the one that
                // demonstrably survives optimisation, where the same logging inside `dhcp_request`
                // does not reach the binary at all.
                //
                // Three unknowns, one line: the frame LENGTH (the option walk starts at offset 282, so
                // anything shorter than 284 means options are unreachable and every option-derived
                // value is really the fallback), the message TYPE seen (2 = OFFER, 5 = ACK), and
                // whether a server identifier was found (option 54 is what a REQUEST must name, and
                // 0.0.0.0 means we never read one).
                let mut mtype = 0u8;
                let mut oo = 282usize;
                while oo + 1 < f.len() {
                    if f[oo] == 255 { break; }
                    if f[oo] == 0 { oo += 1; continue; }
                    if f[oo] == 53 && oo + 2 < f.len() { mtype = f[oo + 2]; }
                    oo += 2 + f[oo + 1] as usize;
                }
                ctx.log_fmt(format_args!(
                    "net-stack: DHCP reply - {} bytes, type {} (2=OFFER 5=ACK), server {}.{}.{}.{}",
                    f.len(), mtype, srv[0], srv[1], srv[2], srv[3]));
                found = Some((ip, gw, dns, srv));
                true
            } else { false }
        });
        if let Some((ip, gw, dns, srv)) = found {
            ctx.log_fmt(format_args!(
                "net-stack: DHCP - offered {}.{}.{}.{}, gw {}.{}.{}.{}, dns {}.{}.{}.{}",
                ip[0], ip[1], ip[2], ip[3], gw[0], gw[1], gw[2], gw[3], dns[0], dns[1], dns[2], dns[3]));
            // ACCEPT THE OFFER. An offer is not a lease (RFC 2131 §3.1): the client must REQUEST the
            // address and the server must ACK it, and only then is the address the client's.
            //
            // This half never existed - the code took the offered address and started using it. QEMU's
            // slirp is permissive enough not to care, which is why it passed there for so long, but a
            // real router hands out an address it has never assigned to us: it will not answer ARP from
            // it, will not route for it, and re-offers a FRESH address on the next DISCOVER. That is
            // exactly what the Pi 2 shows - .66, then .67, then .70, each one used briefly and never
            // owned, with the gateway silent to every ARP.
            if dhcp_request(ctx, pending, our_mac, &ip, &srv, bcast, serve_status) {
                return Some((ip, gw, dns));
            }
            // No ACK: the address is NOT ours, and using it anyway is what produced the silent
            // gateway. Fall through and re-DISCOVER rather than pretend.
            ctx.log("net-stack: DHCP - REQUEST not acknowledged; the address is not ours, retrying");
        }
        let _ = ctx.reacquire_by_name("nic-driver");   // best-effort: we retry either way
    }
    if send_fail > 0 {
        ctx.log_fmt(format_args!(
            "net-stack: DHCP - no offer within the budget, and {} of {} DISCOVERs never left the host - the driver refused them, so this is not a silent server",
            send_fail, DANCE_TRIES));
    } else {
        ctx.log("net-stack: DHCP - no offer within the budget - degrading to the fallback IP");
    }
    None
}

/// Resolve a hostname to an IPv4 address via DNS (UDP to slirp's resolver at 10.0.2.3). Builds a
/// standard A-record query, sends it THROUGH nic-driver, and parses the first A answer. Returns the
/// IP, or None (no gateway, malformed name, or no answer - DNS depends on the host's resolver, which
/// slirp forwards to, so a failure here is a real "no answer", not a bug).
fn dns_resolve(ctx: &ServiceContext, pending: &mut Displaced, hostname: &[u8], gw_mac: &[u8; 6], our_ip: &[u8; 4],
               our_mac: &[u8; 6], dns_server: &[u8; 4], got_reply: &mut bool,
               frames: &mut u16, udp: &mut u16, timeouts: &mut u16) -> Option<[u8; 4]> {
    // frames/udp/timeouts accumulate a DIAGNOSTIC: non-empty frames collected, how many were UDP, and how
    // many nic-driver requests TIMED OUT (net-stack's deadline fired before nic-driver replied). Timeouts
    // dominating => the deadline is too short (a timing bug); empties dominating => the receiver is dead.
    *got_reply = false;   // set true once a matching DNS reply arrives - lets the caller tell
                          // "server did not reply" from "server replied but had no A record".
    let mut frame = [0u8; 512];
    // Ethernet: to the gateway; slirp routes the datagram to its DNS at 10.0.2.3.
    frame[0..6].copy_from_slice(gw_mac);
    frame[6..12].copy_from_slice(our_mac);
    frame[12] = 0x08; frame[13] = 0x00;              // IPv4
    // --- DNS message at offset 42 (14 Ethernet + 20 IPv4 + 8 UDP). Build it first to size the rest.
    const D: usize = 42;
    frame[D] = 0x13; frame[D + 1] = 0x37;            // transaction id (arbitrary)
    frame[D + 2] = 0x01; frame[D + 3] = 0x00;        // flags: standard query, recursion desired
    frame[D + 4] = 0x00; frame[D + 5] = 0x01;        // qdcount = 1 (an/ns/ar counts stay 0)
    // Question: QNAME (length-prefixed labels + 0), QTYPE = A, QCLASS = IN.
    let mut pos = D + 12;
    let mut label_start = 0usize;
    let mut i = 0usize;
    while i <= hostname.len() {
        if i == hostname.len() || hostname[i] == b'.' {
            let len = i - label_start;
            if len == 0 || len > 63 || pos + 1 + len >= frame.len() - 8 { return None; }
            frame[pos] = len as u8; pos += 1;
            frame[pos..pos + len].copy_from_slice(&hostname[label_start..i]);
            pos += len;
            label_start = i + 1;
        }
        i += 1;
    }
    frame[pos] = 0; pos += 1;                         // QNAME terminator
    frame[pos] = 0x00; frame[pos + 1] = 0x01;        // QTYPE = A
    frame[pos + 2] = 0x00; frame[pos + 3] = 0x01;    // QCLASS = IN
    pos += 4;
    let dns_len = pos - D;
    let frame_len = pos;
    // --- IPv4 header.
    frame[14] = 0x45; frame[15] = 0x00;
    let total = (20 + 8 + dns_len) as u16;
    frame[16] = (total >> 8) as u8; frame[17] = total as u8;
    frame[22] = 64; frame[23] = 17;                  // TTL, protocol = UDP
    frame[26..30].copy_from_slice(our_ip);
    frame[30..34].copy_from_slice(dns_server);       // dst = the DHCP-learned DNS server
    let ip_ck = checksum(&frame[14..34]);
    frame[24] = (ip_ck >> 8) as u8; frame[25] = ip_ck as u8;
    // --- UDP header (src port 49153 - a PRIVATE port, deliberately NOT 5353/mDNS: a live LAN's constant
    // mDNS traffic to port 5353 would otherwise get matched as our DNS reply; dst port 53; cksum 0 opt).
    frame[34] = 0xc0; frame[35] = 0x01;
    frame[36] = 0x00; frame[37] = 0x35;
    let udp_len = (8 + dns_len) as u16;
    frame[38] = (udp_len >> 8) as u8; frame[39] = udp_len as u8;

    // Send THROUGH nic-driver, bounded + retrying past stray frames (Stage B: never block on a busy/
    // silent driver). Match the reply to OUR query: a UDP packet to our source port 5353 (0x14e9).
    // Send the query ONCE, then RX-ONLY poll ([4]) for subsequent frames - so a reply arriving BEHIND
    // stray broadcasts on a busy LAN is caught WITHOUT re-transmitting (a re-TX drains+discards it).
    let req     = Message::from_bytes(&frame[..frame_len]);
    let rx_only = Message::from_bytes(&[4u8]);
    let mut arp_out = [0u8; 42];
    // Send the query, then COLLECT with an explicit RX poll. The first frame used to ride back on the
    // send itself; `nic-driver` no longer couples a receive to a transmit, so asking for it is now the
    // only way to get it - and a send that fails is reported rather than discarded, which is what a
    // caller waiting on a reply needs to know.
    if nic_req(ctx, pending, &req, DANCE_SECS).is_none() { *timeouts += 1; }
    let mut reply = nic_req(ctx, pending, &rx_only, DANCE_SECS);
    for _ in 0..DNS_RX_TRIES {
        let (matched, answer_arp) = {
            let f: &[u8] = match &reply { Some(r) => r.payload_bytes(), None => { *timeouts += 1; &[] } };
            if !f.is_empty() {
                *frames += 1;
                if f.len() >= 24 && f[23] == 17 { *udp += 1; }
            }
            // IPv4/UDP to OUR DNS query port (49153)?
            let m = f.len() >= D + 12 && f[12] == 0x08 && f[13] == 0x00 && f[23] == 17
                && f[36] == 0xc0 && f[37] == 0x01;
            // Otherwise: is this someone (the gateway) ARPing for US? Answer so it can address the reply.
            let a = !m && build_arp_reply(f, our_ip, our_mac, &mut arp_out);
            (m, a)
        };
        if matched {
            *got_reply = true;   // a matching DNS reply arrived (whatever it contains)
            // `matched` was computed from this same reply, so it is Some here - but "is" is not
            // "will remain": one refactor of how `matched` is derived and this unwrap halts the
            // network stack. Bind what we already know rather than assert it.
            let Some(r) = reply.as_ref() else { return None };
            let f = r.payload_bytes();
            let ancount = ((f[D + 6] as usize) << 8) | (f[D + 7] as usize);
            if ancount != 0 {
                // Skip the echoed question (QNAME + QTYPE + QCLASS), then walk answers for an A record.
                let mut p = D + 12;
                while p < f.len() {
                    let len = f[p];
                    if len == 0 { p += 1; break; }
                    if len & 0xc0 == 0xc0 { p += 2; break; }   // compression pointer
                    p += 1 + len as usize;
                }
                p += 4;                                        // QTYPE + QCLASS
                let mut n = 0;
                while n < ancount {
                    if p >= f.len() { break; }
                    if f[p] & 0xc0 == 0xc0 { p += 2; }
                    else { while p < f.len() { let len = f[p]; if len == 0 { p += 1; break; } p += 1 + len as usize; } }
                    if p + 10 > f.len() { break; }
                    let atype = ((f[p] as usize) << 8) | (f[p + 1] as usize);
                    let rdlength = ((f[p + 8] as usize) << 8) | (f[p + 9] as usize);
                    p += 10;
                    if atype == 1 && rdlength == 4 && p + 4 <= f.len() {
                        return Some([f[p], f[p + 1], f[p + 2], f[p + 3]]);
                    }
                    p += rdlength;
                    n += 1;
                }
            }
            return None;   // a matching DNS reply but no A record (got_reply=true -> NoRecord)
        }
        // Not our reply. If we owe an ARP reply (the gateway asked for us), send it - and then collect
        // the next frame the same way regardless. The ARP reply's own send used to double as the next
        // receive, which is the coupling this change removed: answering somebody else's ARP is not a
        // reason to consume a frame, and when the caller ignored it that frame was destroyed.
        if answer_arp {
            let _ = ctx.request_with_reply_deadline("nic-driver", &Message::from_bytes(&arp_out), DANCE_SECS);
        }
        // PACE THE POLL, or this loop does not wait at all.
        //
        // Op 4 is a BOUNDED poll - the driver checks for a frame a handful of times and answers, in
        // microseconds, whether or not one arrived. Twelve of those back to back is not a wait for a
        // reply, it is twelve instant questions: the whole loop finished in 78 ms on hardware and
        // reported that DNS could not resolve, 78 ms after the DHCP offer that preceded it. No resolver
        // answers that fast, and the fallback address hid it.
        //
        // This loop used to be paced by accident. Its frames came from the reply to a TRANSMIT, and the
        // driver polled after transmitting; removing that coupling was right, but it took the wait away
        // with it and left the retry counting rather than waiting. `drain_scan` already carries the same
        // pacing for the same reason - a poll is a question, and asking it twelve times in a row does
        // not make the answer arrive sooner.
        ctx.sleep(ctx.duration_cycles(RX_POLL_PACE_MS));
        reply = ctx.request_with_reply_deadline("nic-driver", &rx_only, DANCE_SECS);
    }
    None
}

// --- Socket as capability (§7.10): a UDP socket is a delegated resource cap minted by net-stack,
// the SAME mechanism `fs` uses for a file. A client opens a socket (net-stack mints + grants the cap),
// then INVOKES the cap to send a datagram - the kernel badges the invocation with the socket's
// ResourceId so net-stack knows which socket, without the kernel knowing what a socket is.
const MAX_SOCKETS: usize = 8;
const RIGHT_READ:  u8 = 1 << 0;
const RIGHT_WRITE: u8 = 1 << 1;
const RIGHT_GRANT: u8 = 1 << 4;

#[derive(Clone, Copy)]
struct Socket { rid: u64, port: u16 }

// ── Operations on a TCP capability ─────────────────────────────────────────────────────────────
//
// A badged invocation already names its resource, so the payload's first byte is the OPERATION and
// nothing else has to be threaded through it. Which set applies is decided by what the resource IS -
// a listener, a connection, or a UDP socket - which the service looks up rather than the client
// asserting. A client cannot claim a listener is a connection: it holds a capability to one specific
// resource, and net-stack knows what that resource is.
//
// UDP sockets keep their existing wire shape (`[dest_ip(4), dest_port(2), data..]`, no op byte),
// because they are identified the same way and changing a working surface to look symmetrical is
// the kind of tidying §26.2 asks not to do.

/// Listener: take the next connection that completed its handshake. Reply carries the connection
/// capability, or is empty when nothing is waiting.
const LOP_ACCEPT: u8 = 0;
/// Listener: stop answering on this port and release the slot.
///
/// **This has to be an explicit operation, and its absence was a real leak.** Dropping the client's
/// capability does NOT tell the owner - the kernel revokes the holder's authority, but net-stack's
/// listener table is its own state and nothing walks back from a dropped cap to it. So a `serve`
/// that finished left the port registered forever, and after `MAX_LISTEN` runs the machine could
/// never listen again until net-stack restarted. Found on the Pi 2: the second `serve 8080` was
/// refused, which is exactly right and exactly unhelpful.
///
/// The same rule `fs` follows for a file: the holder closes it, and the owner reclaims.
const LOP_CLOSE: u8 = 1;

/// Connection: read whatever has been delivered in order and not yet taken.
const COP_RECV: u8 = 0;
/// Connection: queue the rest of the payload for sending.
const COP_SEND: u8 = 1;
/// Connection: begin an orderly close.
const COP_CLOSE: u8 = 2;
/// Connection: report state, bytes readable, and bytes unacknowledged - without moving any of it.
const COP_STAT: u8 = 3;

/// Send a UDP datagram (src_port -> dest_ip:dest_port carrying `data`) THROUGH nic-driver and copy the
/// response's UDP payload into `out`. Returns the payload length, or None (no gateway / no reply).
fn udp_roundtrip(ctx: &ServiceContext, pending: &mut Displaced, gw_mac: &[u8; 6], our_ip: &[u8; 4], our_mac: &[u8; 6],
                 src_port: u16, dest_ip: &[u8; 4], dest_port: u16, data: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut frame = [0u8; 1600];
    let dlen = data.len().min(frame.len() - 42);
    frame[0..6].copy_from_slice(gw_mac);
    frame[6..12].copy_from_slice(our_mac);
    frame[12] = 0x08; frame[13] = 0x00;                  // IPv4
    frame[14] = 0x45;
    let total = (20 + 8 + dlen) as u16;
    frame[16] = (total >> 8) as u8; frame[17] = total as u8;
    frame[22] = 64; frame[23] = 17;                      // TTL, UDP
    frame[26..30].copy_from_slice(our_ip);
    frame[30..34].copy_from_slice(dest_ip);
    let ip_ck = checksum(&frame[14..34]);
    frame[24] = (ip_ck >> 8) as u8; frame[25] = ip_ck as u8;
    frame[34] = (src_port >> 8) as u8; frame[35] = src_port as u8;
    frame[36] = (dest_port >> 8) as u8; frame[37] = dest_port as u8;
    let ulen = (8 + dlen) as u16;
    frame[38] = (ulen >> 8) as u8; frame[39] = ulen as u8;
    frame[42..42 + dlen].copy_from_slice(&data[..dlen]);
    let req = Message::from_bytes(&frame[..42 + dlen]);
    // Bounded + retry past stray frames (Stage B: never block on a busy/silent driver). Match the reply
    // to OUR datagram: a UDP packet FROM dest_ip back TO our src_port.
    for _ in 0..DANCE_TRIES {
        let reply = match sifted_req(ctx, pending, &req, DANCE_SECS) {
            DeadlineOutcome::Reply(r) => Some(r),
            _ => None,
        };
        let reply = match reply {
            Some(r) => r,
            None => { let _ = ctx.reacquire_by_name("nic-driver"); continue; }
        };
        let f = reply.payload_bytes();
        if f.len() >= 42 && f[12] == 0x08 && f[13] == 0x00 && f[23] == 17
            && f[26] == dest_ip[0] && f[27] == dest_ip[1] && f[28] == dest_ip[2] && f[29] == dest_ip[3]
            && f[36] == (src_port >> 8) as u8 && f[37] == src_port as u8 {
            let payload_len = (((f[38] as usize) << 8) | (f[39] as usize)).saturating_sub(8);
            let n = payload_len.min(f.len() - 42).min(out.len());
            out[..n].copy_from_slice(&f[42..42 + n]);
            return Some(n);
        }
    }
    None
}

/// Passes of the TCP transaction loop. A BACKSTOP on work, not the bound - `budget_ms` bounds the
/// duration and is meant to be what actually stops us.
///
/// It was 400, and on hardware 400 unpaced passes took 0.4 SECONDS against an 8 second budget: the
/// loop spun through its whole allowance while a 7 to 35 ms round trip was still in flight, then
/// reported "the budget expired" having used a twentieth of it. QEMU hid this completely, because a
/// SLIRP peer answers faster than the loop can spin. The real fix is the pacing below; this number is
/// raised so that the TIME bound is the one that bites, which is what it always claimed to be.
const TCP_STEPS: usize = 20_000;

/// One complete TCP transaction, driven synchronously inside a client request.
///
/// **Why this shape, and what it is not.** `docs/net-tags-design.md` forbids any unsolicited driver
/// traffic until its phase 2/3 land: net-stack serves clients and receives nic-driver replies on ONE
/// untagged endpoint, so a background poll consumes client messages. That makes a BACKGROUND TCP
/// engine a prerequisite-blocked change, and it is recorded as the next step rather than smuggled in
/// here.
///
/// What is not blocked is driving the same state machine from inside a request, which is exactly how
/// `udp_roundtrip` and `ping` already work and opens no window that is not already open. So this
/// connects, sends, reads until the peer closes or the budget expires, and closes - exercising the
/// handshake, sequencing, cumulative ACK, retransmission and the FIN exchange end to end.
///
/// Bounded twice over: `budget_ms` caps the whole transaction, and every inner loop has its own
/// iteration ceiling, so a peer that answers slowly costs a deadline and a peer that answers never
/// costs the same.
/// `#[inline(never)]` DELIBERATELY. Inlined into `service_main` this carried its 1600-byte frame
/// buffer, and `feed_*`'s two more, into the one frame the whole service lives in - measured at 98 KiB
/// against the Pi 2's 256 KiB user stack, the deepest single frame in the system. `stack_fit_check`
/// bounds ONE frame and says so; it cannot see the sum along a call path, which is what a 98 KiB entry
/// frame plus nested callees actually is. Same idiom the shell already uses for its record builders.
#[inline(never)]
fn tcp_transact(ctx: &ServiceContext, pending: &mut Displaced, t: &mut tcp::Tcp, net: &tcp::Net,
                dst: [u8; 4], dport: u16, req: &[u8], out: &mut [u8],
                budget_ms: u64) -> Result<usize, tcp::Fault> {
    let mut frame = [0u8; 1600];
    let start = t.now_ms(ctx).unwrap_or(0);
    let deadline = start + budget_ms;

    t.stat_seen = 0; t.stat_matched = 0; t.stat_sent = 0;
    t.tx_n = 0; t.tx_log = [0u8; 24];
    let i = match t.connect(ctx, 1, dst, dport, net.peer_mac) { Some(i) => i, None => return Err(tcp::Fault::None) };

    // The opening SYN. Sent through the same path every other frame uses, and its reply may already
    // carry the SYN-ACK - nic-driver answers a TX with whatever it has received.
    let n = t.syn_frame(ctx, net, i, &mut frame);
    if n > 0 {
        t.stat_sent = t.stat_sent.saturating_add(1);
        let r = nic_req(ctx, pending, &Message::from_bytes(&frame[..n]), LINK_SECS);
        t.note_tx(&frame[..n], r.is_some());
        feed_tx(ctx, pending, t, net, r);
    }

    let mut wrote = false;
    let mut got = 0usize;
    // Consecutive passes that neither sent nor received anything.
    let mut empty: u32 = 0;

    // ONE loop for the whole connection. Each pass: let the state machine emit whatever it owes
    // (retransmission, data, FIN), then drain one batch of frames into it.
    for _ in 0..TCP_STEPS {
        if t.now_ms(ctx).unwrap_or(0) >= deadline { break; }

        // Hand the request over as soon as the handshake completes.
        if !wrote && t.conns[i].state == tcp::State::Established {
            t.write(1, req);
            wrote = true;
        }

        // Emit. `poll_one` returns at most one frame, so this is bounded by MAX_CONNS trivially.
        let n = t.poll_one(ctx, net, i, &mut frame);
        if n > 0 {
            t.stat_sent = t.stat_sent.saturating_add(1);
            let r = nic_req(ctx, pending, &Message::from_bytes(&frame[..n]), LINK_SECS);
            t.note_tx(&frame[..n], r.is_some());
            feed_tx(ctx, pending, t, net, r);
            empty = 0;
        } else {
            // Nothing to send: ask for received frames explicitly, or a peer that is talking while
            // we are silent would never be heard.
            // Two statements rather than one, because the stash is borrowed by BOTH calls and
            // cannot be lent twice at once. Splitting them is also the truer order: ask the driver,
            // then feed what it said.
            let batch = nic_drain(ctx, pending);
            if feed_batch(ctx, pending, t, net, batch) { empty = 0; } else { empty += 1; }
        }

        // PACE AN EMPTY PASS. Without this the loop spins: nothing to send, nothing received, and
        // another pass immediately - which burned 400 passes in 0.4 s on hardware while the peer's
        // reply was still on the wire. The shape is `drain_scan`'s and the reasoning is the same one
        // recorded there: yield for the first few, because a reply usually follows the frame that
        // prompted it and the moments just after traffic are the worst time to go blind; only sleep
        // once the wire really has nothing.
        if empty > 0 {
            if empty <= EMPTY_YIELDS {
                ctx.yield_cpu();
            } else {
                ctx.sleep(ctx.duration_cycles(RX_POLL_PACE_MS));
            }
        }

        // Collect whatever has been delivered in order.
        if t.conns[i].readable() > 0 && got < out.len() {
            got += t.read(1, &mut out[got..]);
        }

        let st = t.conns[i].state;
        // DRAIN BEFORE LEAVING. A closed connection may still hold delivered bytes the client has
        // not taken, and `forget` below reclaims the arena. Breaking on the state alone threw away
        // whatever arrived in the same pass as the FIN.
        if st == tcp::State::Closed && t.conns[i].readable() == 0 { break; }
        // The peer said it is done sending. Take what is left and close from our side.
        if st == tcp::State::CloseWait && wrote {
            t.close(1);
        }
    }

    let fault = t.conns[i].fault;
    // Carried out of the connection before the slot is released, so the op arm can say how far this
    // got. Without it, a transaction that simply never connected is indistinguishable from one that
    // connected and was answered with nothing.
    t.last_state = t.conns[i].state;
    t.last_retx = t.conns[i].retx_count;
    // Best effort: send our FIN if we still owe one, then let the slot go. A connection left in the
    // table would hold an arena for nothing.
    t.close(1);
    let n = t.poll_one(ctx, net, i, &mut frame);
    if n > 0 { let _ = nic_req(ctx, pending, &Message::from_bytes(&frame[..n]), LINK_SECS); }
    t.forget(1);

    if got == 0 && fault != tcp::Fault::None { Err(fault) } else { Ok(got) }
}

/// Feed one RAW frame into the state machine. Returns true if it was ours.
///
/// THIS FUNCTION DOES NOT TRANSMIT, and that is the point rather than an omission. net-stack has one
/// endpoint and one receive slot, so calling `nic_req` from inside the handling of another
/// `nic_req` reply nests request/reply on a single slot and desyncs them - the failure
/// `docs/net-tags-design.md` describes. On a Pi 2 that lost every acknowledgement this stack built:
/// 3 SYN-ACKs matched, 8 frames handed over, 4 on the wire, and the peer retransmitting its SYN-ACK
/// eight times into a connection that would never complete.
///
/// Anything the state machine wants to say is now RECORDED (`ack_due`) and sent by `poll_one` on the
/// transaction loop's next pass, where there is no outer reply in flight.
///
/// The one exception is ARP, which is answered here because it is not TCP and cannot wait for a
/// connection's poll - a peer that cannot resolve us cannot reach us at all.
fn feed_frame(ctx: &ServiceContext, pending: &mut Displaced, t: &mut tcp::Tcp, net: &tcp::Net, f: &[u8]) -> bool {
    let mut arp_out = [0u8; 42];
    if build_arp_reply(f, &net.our_ip, &net.our_mac, &mut arp_out) {
        // Answered inline because it is the only way a peer learns our MAC while we hold the service,
        // and the reply is fire-and-forget: nothing here depends on what comes back, so the nesting
        // hazard above does not apply to it.
        let _ = nic_req(ctx, pending, &Message::from_bytes(&arp_out), LINK_SECS);
        return true;
    }
    if f.len() < tcp::HDR { return false; }
    let mut sink = [0u8; 1600];
    t.on_frame(ctx, net, f, &mut sink);
    true
}

/// Feed a DRAIN BATCH (op 9) into the state machine.
///
/// A drain reply is `[count, (len_u16_le, frame) x count]` - NOT a bare frame. Treating it as one is
/// what made the first end-to-end run fail with the peer retransmitting its SYN-ACK six times while
/// this stack sat in SynSent: the segment arrived every time and was parsed as garbage every time.
/// The pcap is what made that readable, because the guest's own log could only say "nothing came".
/// The batch shape is `drain_scan`'s, and it is read the same way here rather than re-derived.
/// How often the serve loop wakes to answer for itself when no client is asking.
///
/// A hundred milliseconds. The things it has to be quick enough for are an ARP request (a peer
/// retries about once a second), an inbound ping (one a second, and the poll interval lands directly
/// in the reported round trip), and later a TCP retransmission timer whose floor is `RTO_MIN_MS` =
/// 200 ms. Ten wakes a second is also the ceiling on what this costs: each is ONE drain request to
/// `nic-driver`, and only when nothing else woke us.
const POLL_MS: u64 = 100;

/// How long ONE frame the poll step sends may wait on the driver.
///
/// **Set from what the hardware costs, not from what looks tidy.** The first version of this was
/// 20 ms, chosen because it is comfortably under `POLL_MS` - and it broke a connection that had been
/// working: on a Pi 2 the dwc2 driver needs longer than that for an ordinary frame, so every frame
/// the poll sent timed out, including the one carrying an echo. A bound that the normal case cannot
/// meet is not a bound, it is an outage.
///
/// 200 ms gives the slowest driver in the tree room while still being a fifth of a client's
/// patience. `poll_tx_slow` counts what actually happens, so the next person sets this from a
/// measurement rather than from my estimate.
const POLL_TX_MS: u64 = 200;

/// The whole poll step's budget. Checked between frames, so the step stops issuing new work once it
/// is spent rather than running to completion however long that takes.
///
/// Half a second: long enough for a few frames on a slow driver, and a tenth of the five seconds a
/// client waits before it gives up. The failure this exists to prevent is a poll outlasting the
/// request it is keeping waiting.
const POLL_BUDGET_MS: u64 = 250;

/// A serve pass slower than this is REPORTED. It is not a bound and nothing is aborted - it is the
/// instrument that separates "this loop was starved" from "this loop was running and the message was
/// not delivered", which no existing line could distinguish.
///
/// One second, because a healthy pass is `POLL_MS` (100) plus at most `POLL_BUDGET_MS` (250), so a
/// second is four times the worst legitimate pass and cannot fire on an ordinary busy moment. A
/// `nic_req` waiting out its full `LINK_SECS` is exactly the kind of pass worth hearing about.
const SLOW_PASS_MS: u64 = 1_000;
const _: () = assert!(SLOW_PASS_MS > POLL_MS + POLL_BUDGET_MS,
    "a healthy pass must not trip the slow-pass report, or the report is noise");

// ── The three budgets, and the order they MUST be in ───────────────────────────────────────────
//
// **These collided, and hardware paid for it twice.** `HOLD_MS` (how long a displaced client
// request is kept) and `POLL_BUDGET_MS` (how long a poll step may run) were both 500 ms, set an
// hour apart and never compared: a request stashed at the START of a poll expired at exactly the
// moment that poll finished. On a Pi 2 that ate the echo of a `serve` session - the board logged
// `received 14 byte(s)`, held the reply request, dropped it, and the client timed out against a
// connection that was working perfectly.
//
//     POLL_BUDGET_MS  <  HOLD_MS  <  the shortest client deadline
//
// A held request must outlive the longest thing that can delay it being served (one poll step), and
// must be served well before the client stops waiting (3 s, the shell's status query).
//
// ASSERTED, not written down, because a comment is what failed: two numbers that had to relate were
// recorded separately and drifted into collision. Breaking the ordering now stops the build.
const _: () = assert!(POLL_BUDGET_MS < HOLD_MS,
    "a held request must outlive a poll step, or the poll drops the very request it delays");
// NO UPPER BOUND ANY MORE, and its removal is the fix rather than a relaxation. This used to assert
// `HOLD_MS < 3_000` - "a held request must be served before the shortest client deadline" - which was
// true when every client here waited 3 s and became a defect the moment the transaction path waited
// 20 s: net-stack threw requests away at 1.5 s while their client sat patiently for eighteen seconds
// more, timed out, re-sent, and had the re-send answered in milliseconds (`backlog/29`, measured at
// 19.67 s and 19.99 s on a Dell Wyse). A constant cannot know a client's deadline, so it no longer
// guesses: the CLIENT SAYS, in byte 1, and `HOLD_MS` is now only the default for a badged invocation
// that carries no header. The ordering assertion above still binds, and is the one that matters.

/// One bounded pass of work nobody asked for: drain the NIC once and answer for ourselves.
///
/// **This is the tick that was reverted, and it is only safe to bring back now.** The revert note in
/// this file says why it went: net-stack serves clients and receives driver replies on one endpoint,
/// so anything that talked to the driver unasked stole client messages, and a once-a-second tick
/// turned a latent race into a permanent one. `docs/net-tags-design.md` set the precondition in
/// capitals - do not add a tick before the correlation is fixed. Phase 2 (sifting) and phase 3 (the
/// bounded stash) are both in, and the client hop carries a tag, so a client met here is identified,
/// kept, and served by the loop below rather than consumed.
///
/// What it buys today, which is not speculative: the machine ANSWERS FOR ITSELF while idle. Every
/// ARP reply this service builds is inside a drain loop, so between commands a peer asking "who has
/// this address" got nothing; and an inbound ping was never answered at all, because no echo-request
/// handler existed. Both are the ordinary way one machine checks another is alive.
///
/// Bounded in every direction: one drain, at most the frames it returns, one pass of the connection
/// table. Returns whether anything arrived, so the caller can keep polling while the wire is busy
/// instead of sleeping through a burst.
fn poll_step(ctx: &ServiceContext, pending: &mut Displaced, st: &NetState,
             t: &mut tcp::Tcp, net: &tcp::Net) -> bool {
    // THE BUDGET. Everything below checks it before issuing more driver work, so this step cannot
    // outlast its own interval and starve the client the service exists to answer.
    let t0 = ctx.read_tsc();
    let budget = ctx.duration_cycles(POLL_BUDGET_MS);
    let spent = |ctx: &ServiceContext| ctx.read_tsc().wrapping_sub(t0) >= budget;

    let batch = nic_drain_ms(ctx, pending, POLL_TX_MS);
    let m = match batch { Some(m) => m, None => return false };
    let p = m.payload_bytes();
    if p.is_empty() { return false; }
    let count = p[0] as usize;
    let mut pos = 1usize;
    let mut any = false;
    let mut out = [0u8; 1600];
    for _ in 0..count {
        if pos + 2 > p.len() { break; }
        let fl = u16::from_le_bytes([p[pos], p[pos + 1]]) as usize;
        pos += 2;
        if pos + fl > p.len() { break; }
        let f = &p[pos..pos + fl];
        pos += fl;
        any = true;
        // Out of budget: the frames already read are still fed to the state machine below, but no
        // more REPLIES are sent this pass. Feeding is arithmetic; replying is a driver round trip,
        // and only the second one can hold the service up.
        let quiet = spent(ctx);

        // ARP for us. Answered first because without it nothing else can reach us at all.
        let mut arp_out = [0u8; 42];
        if build_arp_reply(f, &st.our_ip, &st.our_mac, &mut arp_out) {
            if !quiet {
                let _ = nic_req_ms(ctx, pending, &Message::from_bytes(&arp_out), POLL_TX_MS);
            }
            continue;
        }
        // A ping addressed to us.
        let n = build_icmp_reply(f, &st.our_ip, &st.our_mac, &mut out);
        if n > 0 {
            if !quiet {
                let _ = nic_req_ms(ctx, pending, &Message::from_bytes(&out[..n]), POLL_TX_MS);
            }
            continue;
        }
        // Anything else that is TCP for one of our connections. `on_frame` never transmits - it
        // records what is owed and `poll_one` below sends it, which is the separation that took a
        // day of hardware debugging to find (see `Conn::ack_due`).
        if fl >= tcp::HDR {
            let mut sink = [0u8; 1600];
            t.on_frame(ctx, net, f, &mut sink);
        }
    }

    // ---- REAP finished connections ----
    //
    // A connection that has reached `Closed` with nothing left to read is done, but its slot is NOT
    // free: `in_use` is `state != Closed || rid != 0`, so an owned connection holds its slot even
    // after the protocol has finished with it. With `MAX_CONNS` slots that is a leak measured in
    // twos - the table fills and no further connection, inbound or outbound, can be made.
    //
    // Revoking is the right way to tell the client, rather than a reply it has to ask for: its next
    // invocation gets `CapRevoked` from the KERNEL, which is the same answer a deleted file gives
    // and needs no cooperation from a client that may already have moved on (§7.5, §7.10).
    //
    // `readable() == 0` is the guard that matters: a peer's last bytes and its FIN can arrive
    // together, and reaping on the state alone would discard data the client has not taken yet -
    // the same mistake the transaction loop's "DRAIN BEFORE LEAVING" comment records.
    for i in 0..tcp::MAX_CONNS {
        let c = &t.conns[i];
        if c.rid != 0 && c.state == tcp::State::Closed && c.readable() == 0 {
            let rid = c.rid;
            t.forget(rid);
            let _ = ctx.resource_revoke(rid);
            ctx.log("net-stack: a finished connection was reaped and its slot released");
        }
    }

    // Now let every live connection make its own progress: an acknowledgement owed, a retransmission
    // due, a window probe, a FIN to answer. This is the half that makes a connection able to outlive
    // the request that opened it.
    for i in 0..tcp::MAX_CONNS {
        // Out of budget: leave the rest for the next poll, a hundred milliseconds away. A
        // retransmission or an acknowledgement is not urgent enough to hold a client's request.
        if spent(ctx) { break; }
        let n = t.poll_one(ctx, net, i, &mut out);
        if n > 0 {
            // REPORT A FRAME THE DRIVER WOULD NOT TAKE IN TIME. This is the measurement that
            // `POLL_TX_MS` should be set from, and its absence is why the first value was a guess
            // that broke a working connection. A silent drop here looks exactly like a network
            // that lost the frame, which is the one thing it must not be confused with (§26.7).
            if nic_req_ms(ctx, pending, &Message::from_bytes(&out[..n]), POLL_TX_MS).is_none() {
                t.poll_tx_slow = t.poll_tx_slow.saturating_add(1);
                if t.poll_tx_slow == 1 || t.poll_tx_slow % 64 == 0 {
                    ctx.log_fmt(format_args!(
                        "net-stack: a polled frame was not taken by the driver within {} ms                          ({} so far) - the peer will retransmit, but POLL_TX_MS may be too tight                          for this board", POLL_TX_MS, t.poll_tx_slow));
                }
            }
            any = true;
        }
    }
    any
}

#[inline(never)]
fn feed_batch(ctx: &ServiceContext, pending: &mut Displaced, t: &mut tcp::Tcp, net: &tcp::Net, reply: Option<Message>) -> bool {
    let m = match reply { Some(m) => m, None => return false };
    let p = m.payload_bytes();
    if p.is_empty() { return false; }
    let count = p[0] as usize;
    let mut pos = 1usize;
    let mut any = false;
    for _ in 0..count {
        if pos + 2 > p.len() { break; }
        let fl = u16::from_le_bytes([p[pos], p[pos + 1]]) as usize;
        pos += 2;
        if pos + fl > p.len() { break; }
        if feed_frame(ctx, pending, t, net, &p[pos..pos + fl]) { any = true; }
        pos += fl;
    }
    any
}

/// Feed the reply to a TRANSMISSION, which carries at most one raw frame (the shape
/// `udp_roundtrip` already relies on).
#[inline(never)]
fn feed_tx(ctx: &ServiceContext, pending: &mut Displaced, t: &mut tcp::Tcp, net: &tcp::Net, reply: Option<Message>) -> bool {
    match reply {
        Some(m) => feed_frame(ctx, pending, t, net, m.payload_bytes()),
        None => false,
    }
}

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_UNIX_OFFSET: u32 = 2_208_988_800;
/// A fixed anycast NTP server (time.cloudflare.com) used if DNS cannot resolve a pool name - so a DNS
/// hiccup never blocks the clock. Anycast: routed to the nearest instance, reliable from anywhere.
const NTP_FALLBACK_IP: [u8; 4] = [162, 159, 200, 123];
/// A plausible "now" window - reject a garbage/stale/hostile SNTP timestamp outside it rather than adopt
/// it as this machine's time. Floor = 2020-01-01, ceiling = 2100-01-01 (both fit a u32 epoch).
const SNTP_MIN_PLAUSIBLE: u32 = 1_577_836_800;
const SNTP_MAX_PLAUSIBLE: u32 = 4_102_444_800;
/// Tries for the SNTP exchange. Deliberately FEWER than DANCE_TRIES: each try costs a DANCE_SECS drain, and
/// this runs inside net-stack's single-threaded serve loop, so a silent NTP server must not hold every
/// other client op (net/ping/dns) behind it for the full 6-try budget.
const SNTP_TRIES: u32 = 3;

/// How long to leave between automatic SNTP retries while the clock is still unset.
///
/// A minute: long enough that a silent NTP server costs one exchange a minute rather than one per
/// request, short enough that plugging a cable in gets a clock within a minute without anyone asking.
/// Only paid while the clock is UNSET - once it is known this costs a single cheap read.
const RESYNC_SECS: i64 = 60;

/// SNTP: fetch the current time from an NTP server and set the wall clock. The RTC-less Pi 2 has no other
/// time source, so `date` reads zero until this runs (auto on boot after the DHCP dance, and on `date
/// sync`). Resolve pool.ntp.org (fall back to a fixed anycast NTP IP if DNS is down), send a mode-3 client
/// request to UDP 123, parse the 32-bit transmit timestamp (seconds since 1900), convert to Unix, and set
/// the clock via the SET_CLOCK cap. Returns the Unix epoch on success. Bounded (udp_roundtrip's
/// deadline+retry, Commandment VIII: waits on the reply, never hangs); a silent server returns None.
/// The wall clock's current epoch if it already reads a plausible time, else `None`. Two uses: reporting
/// the value after a dance that just synced (without paying for a second exchange), and deciding whether
/// the BOOT sync is needed at all. This is a TRUTH test, not an arch test - a machine whose clock already
/// knows the date (an x86 with a CMOS RTC) needs no network time; the RTC-less Pi 2 reads 0 and does.
fn clock_epoch_if_set(ctx: &ServiceContext) -> Option<u32> {
    // Ask the OWNER. This used to read `datetime()` - the kernel's raw RTC - which the Pi does not
    // have, so a clock this service had just set successfully still read as unset, and `date sync`
    // answered "no time from the network (is the cable in?)" with the cable plainly in.
    let e = match ctx.request_with_reply("time", &Message::from_bytes(&[1])) {
        Some(r) if r.payload_bytes().len() >= 10 && r.payload_bytes()[0] == 1 => {
            let p = r.payload_bytes();
            let mut b = [0u8; 8];
            b.copy_from_slice(&p[1..9]);
            i64::from_le_bytes(b)
        }
        _ => 0,
    };
    if e >= SNTP_MIN_PLAUSIBLE as i64 && e <= SNTP_MAX_PLAUSIBLE as i64 { Some(e as u32) } else { None }
}

fn sntp_sync(ctx: &ServiceContext, pending: &mut Displaced, st: &NetState) -> Option<u32> {
    if !st.gw_known { return None; }                     // no gateway MAC - nothing to send through
    // Resolve an NTP server by name; fall back to the fixed anycast IP if DNS is down - but say so. A
    // recovery that hides the failure it recovered from is a silent fallback (§26.7): without this line an
    // operator cannot tell a resolved pool address from a broken resolver.
    let (mut gf, mut fr, mut ud, mut to) = (false, 0u16, 0u16, 0u16);
    let ntp_ip = match dns_resolve(ctx, pending, b"pool.ntp.org", &st.gw_mac, &st.our_ip, &st.our_mac,
                                   &st.dns_server, &mut gf, &mut fr, &mut ud, &mut to) {
        Some(ip) => ip,
        None => {
            ctx.log("net-stack: SNTP - DNS could not resolve pool.ntp.org - using the fixed anycast NTP IP");
            NTP_FALLBACK_IP
        }
    };
    // A NONCE binds the reply to THIS request (RFC 4330 §5): the client puts it in the TRANSMIT timestamp
    // (SNTP bytes 40..48 = frame 82..90) and the server echoes it back in the ORIGINATE timestamp (SNTP
    // bytes 24..32 = frame 66..74). Without it every match field is a compile-time constant, so ANY host
    // could spray one UDP packet and set this machine's wall clock - the capability system would have
    // granted net-stack the right to set the clock, and net-stack would have handed the VALUE to a
    // stranger (a confused deputy: §3.1/§26.9, authority reached by a principal that holds none).
    let nonce: [u8; 8] = {
        let hi = ctx.hw_random().unwrap_or((ctx.read_tsc() >> 13) as u32);
        let lo = ctx.hw_random().unwrap_or(ctx.read_tsc() as u32);
        let (h, l) = (hi.to_be_bytes(), lo.to_be_bytes());
        [h[0], h[1], h[2], h[3], l[0], l[1], l[2], l[3]]
    };
    // The source port is derived from the nonce too, so it is not a constant an off-path spoofer can assume.
    let src_port: u16 = 40_000 + (u16::from_be_bytes([nonce[0], nonce[1]]) % 20_000);
    ctx.log_fmt(format_args!("net-stack: SNTP - querying {}.{}.{}.{}:123",
        ntp_ip[0], ntp_ip[1], ntp_ip[2], ntp_ip[3]));
    // Build the request frame ONCE: eth(14) + IPv4(20) + UDP(8) + SNTP(48) = 90 bytes.
    let mut frame = [0u8; 90];
    frame[0..6].copy_from_slice(&st.gw_mac);
    frame[6..12].copy_from_slice(&st.our_mac);
    frame[12] = 0x08; frame[13] = 0x00;                  // IPv4
    frame[14] = 0x45;
    let total: u16 = 20 + 8 + 48;
    frame[16] = (total >> 8) as u8; frame[17] = total as u8;
    frame[22] = 64; frame[23] = 17;                      // TTL, UDP
    frame[26..30].copy_from_slice(&st.our_ip);
    frame[30..34].copy_from_slice(&ntp_ip);
    let ip_ck = checksum(&frame[14..34]);
    frame[24] = (ip_ck >> 8) as u8; frame[25] = ip_ck as u8;
    frame[34] = (src_port >> 8) as u8; frame[35] = src_port as u8;
    frame[36] = 0; frame[37] = 123;                      // dest port 123
    frame[38] = 0; frame[39] = 8 + 48;                   // UDP length
    frame[42] = 0x1B;                                    // SNTP: LI 0, VN 3, Mode 3 (client)
    frame[82..90].copy_from_slice(&nonce);               // transmit timestamp = our nonce
    let req = Message::from_bytes(&frame);

    // Send the request, then DRAIN + SCAN the RX ring for the reply until it arrives or the deadline - the
    // same pattern DHCP/ARP use, so a WAN reply that lands tens of ms after the send (which a single-frame
    // rx would have raced and lost) is caught. Retry past stray frames.
    let mut unix: Option<u32> = None;
    let mut arp_out = [0u8; 42];
    let mut send_fail = 0u32;
    for _ in 0..SNTP_TRIES {
        // A query that never left is not a silent time server - see `dhcp_discover`.
        if nic_req(ctx, pending, &req, LINK_SECS).is_none() { send_fail += 1; }
        drain_scan(ctx, pending, DANCE_SECS, None, |f, pending| {
            // A UDP reply FROM ntp_ip:123 TO our source port, ECHOING our nonce. `f[14] == 0x45` pins a
            // 20-byte IP header, without which every offset below (ports at 34/36, SNTP at 42+) would be
            // read from the wrong place on a packet carrying IP options.
            if f.len() >= 90 && f[12] == 0x08 && f[13] == 0x00 && f[14] == 0x45 && f[23] == 17
                && f[26..30] == ntp_ip[..] && f[34] == 0 && f[35] == 123
                && f[36] == (src_port >> 8) as u8 && f[37] == src_port as u8
                && f[66..74] == nonce[..]                        // originate == our nonce: this is OUR reply
                && f[42] & 0x07 == 4                             // mode 4 = server
                && f[42] >> 6 != 3                               // LI 3 = unsynchronized clock
                && f[43] >= 1 && f[43] <= 15                     // stratum (0 = kiss-of-death, no time)
            {
                let ntp_secs = u32::from_be_bytes([f[82], f[83], f[84], f[85]]);
                if ntp_secs > NTP_UNIX_OFFSET {
                    let u = ntp_secs - NTP_UNIX_OFFSET;
                    // Bounded BOTH ways: a garbage or hostile timestamp outside a plausible window is
                    // refused rather than becoming this machine's idea of now.
                    if (SNTP_MIN_PLAUSIBLE..=SNTP_MAX_PLAUSIBLE).contains(&u) { unix = Some(u); return true; }
                }
            }
            // Answer an ARP for us in the meantime so the gateway can keep addressing our unicast replies.
            if build_arp_reply(f, &st.our_ip, &st.our_mac, &mut arp_out) {
                // DECIDED, not overlooked: this is a courtesy reply to somebody else's ARP, sent
                // while we are draining for our own answer. If it fails, that host re-ARPs a moment
                // later and gets another chance - so the outcome carries no information we would act
                // on, and logging it from inside a scan loop would flood the console the moment
                // `nic-driver` is being restarted. Named here so it reads as a decision (§26.7).
                let _ = nic_req(ctx, pending, &Message::from_bytes(&arp_out), LINK_SECS);
            }
            false
        });
        if unix.is_some() { break; }
    }
    if unix.is_none() && send_fail > 0 {
        ctx.log_fmt(format_args!(
            "net-stack: SNTP got no timestamp, and {} of {} queries never left the host - the driver refused them, so this is not a silent time server",
            send_fail, SNTP_TRIES));
    }
    let u = unix?;
    // The kernel can REFUSE this (no SET_CLOCK cap - e.g. on x86, where the CMOS RTC is the authority and
    // nothing is granted the cap). Reporting "wall clock set" after a refusal would be a privileged
    // operation the kernel denied, announced to the operator as done (§26.7, invariant 12).
    // Clock slice 2: the wall clock belongs to the `time` service now, not to a kernel syscall.
    // SNTP is a NETWORK fact, so net-stack fetches it; deciding whether to believe it - plausibility,
    // the floor, provenance - is the clock's own policy, and it says no by replying 0.
    let mut req = [0u8; 9];
    req[0] = 2; // OP_SET
    req[1..9].copy_from_slice(&(u as i64).to_le_bytes());
    let accepted = match ctx.request_with_reply("time", &Message::from_bytes(&req)) {
        Some(r) => { let p = r.payload_bytes(); !p.is_empty() && p[0] != 0 }
        None => {
            // Reacquire once: `find_send_slot` does not resolve a name, so a peer that restarted (or
            // started after us) is unreachable until we ask again. Learned the hard way in arm32 3c.
            let _ = ctx.reacquire_by_name("time");
            match ctx.request_with_reply("time", &Message::from_bytes(&req)) {
                Some(r) => { let p = r.payload_bytes(); !p.is_empty() && p[0] != 0 }
                None => false,
            }
        }
    };
    if !accepted {
        ctx.log("net-stack: SNTP - clock set REFUSED by the kernel (no SET_CLOCK cap) - clock unchanged");
        return None;
    }
    Some(u)
}

/// Send an ICMP echo request to `dest_ip` (via the gateway's MAC) and return true if the matching echo
/// REPLY comes back. Used to probe the gateway (LAN) and a public IP (internet reachability through NAT).
/// If `f` is an inbound ARP REQUEST for `our_ip`, build the matching ARP REPLY into `out` and return
/// true. net-stack MUST answer these: once the gateway's ARP entry for us (the OUR_MAC we advertise)
/// ages out it re-ARPs before it can address our UNICAST replies - stay silent and it only ever reaches
/// us with broadcasts, so the echo/DNS reply never arrives (exactly the T630 serve-loop symptom: 20
/// frames collected, all broadcast, no reply). This fires ONLY when someone is actively asking for us,
/// so on QEMU (slirp already learned us from our own query) it emits nothing - which is why it is safe
/// where a blind gratuitous ARP before every query was not.
/// Turn an inbound ICMP ECHO REQUEST addressed to us into the echo REPLY. Returns its length, or 0
/// if `f` is not an echo request for this machine.
///
/// **This machine could not be pinged.** Every ICMP path here built or matched our OWN outbound
/// echoes; nothing ever answered one addressed to us, busy or idle. A host that cannot be pinged
/// cannot be checked for liveness by the most ordinary tool there is, and on a LAN that reads as "the
/// machine is down" when it is running perfectly.
///
/// The reply is the request REFLECTED: same identifier, same sequence, same payload, which is what
/// makes the sender's round-trip matching work. Only the direction fields change - the MACs and IPs
/// swap, the type becomes 0, the TTL becomes ours - and both checksums are recomputed because the
/// bytes they cover have moved.
fn build_icmp_reply(f: &[u8], our_ip: &[u8; 4], our_mac: &[u8; 6], out: &mut [u8]) -> usize {
    // IPv4, 20-byte header, ICMP, echo REQUEST (type 8), addressed to us.
    if f.len() < 42 || f[12] != 0x08 || f[13] != 0x00 || f[14] != 0x45 || f[23] != 1 || f[34] != 8 {
        return 0;
    }
    if f[30..34] != our_ip[..] { return 0; }
    // The IP header's own length, not the frame's: a short frame is padded to the 60-byte ethernet
    // minimum, and echoing the padding back would make the reply longer than the request.
    let ip_total = ((f[16] as usize) << 8) | f[17] as usize;
    let flen = (14 + ip_total).min(f.len()).min(out.len());
    if flen < 42 { return 0; }
    out[..flen].copy_from_slice(&f[..flen]);

    out[0..6].copy_from_slice(&f[6..12]);            // to whoever asked
    out[6..12].copy_from_slice(our_mac);
    out[26..30].copy_from_slice(our_ip);             // from us
    out[30..34].copy_from_slice(&f[26..30]);         // to them
    out[22] = 64;                                    // our TTL, not theirs
    out[34] = 0;                                     // echo REPLY
    out[24] = 0; out[25] = 0;
    let ip_ck = checksum(&out[14..34]);
    out[24] = (ip_ck >> 8) as u8; out[25] = ip_ck as u8;
    out[36] = 0; out[37] = 0;
    let ic_ck = checksum(&out[34..flen]);
    out[36] = (ic_ck >> 8) as u8; out[37] = ic_ck as u8;
    flen
}

fn build_arp_reply(f: &[u8], our_ip: &[u8; 4], our_mac: &[u8; 6], out: &mut [u8; 42]) -> bool {
    if f.len() < 42 { return false; }
    if f[12] != 0x08 || f[13] != 0x06 { return false; }              // not ARP
    if f[20] != 0x00 || f[21] != 0x01 { return false; }              // not a REQUEST (oper 1)
    if f[38] != our_ip[0] || f[39] != our_ip[1]
        || f[40] != our_ip[2] || f[41] != our_ip[3] { return false; } // not asking for us
    for b in out.iter_mut() { *b = 0; }
    out[0..6].copy_from_slice(&f[22..28]);   // eth dst = the asker (its sender MAC)
    out[6..12].copy_from_slice(our_mac);     // eth src = us
    out[12] = 0x08; out[13] = 0x06;          // ethertype = ARP
    out[14] = 0x00; out[15] = 0x01;          // htype = Ethernet
    out[16] = 0x08; out[17] = 0x00;          // ptype = IPv4
    out[18] = 0x06; out[19] = 0x04;          // hlen 6, plen 4
    out[20] = 0x00; out[21] = 0x02;          // oper = reply
    out[22..28].copy_from_slice(our_mac);    // sender hw = us
    out[28..32].copy_from_slice(our_ip);     // sender ip = us
    out[32..38].copy_from_slice(&f[22..28]); // target hw = the asker
    out[38..42].copy_from_slice(&f[28..32]); // target ip = the asker's ip
    true
}

/// Resolve one host's MAC by ARP: broadcast a who-has, poll for the reply whose SENDER IP is the target
/// (answering any inbound ARP for us in the meantime, so the gateway can still address us). `None` if no
/// reply within the budget. Used by `net arp` (any host) and `net scan` (across the subnet). Same frame
/// path and bound as `ping`/`dns_resolve`, which is why it is reliable now that the receiver no longer
/// stalls (RTL8168 RDU recovery) and the deadline no longer glitches (deglitched RTC).
fn arp_resolve(ctx: &ServiceContext, pending: &mut Displaced, our_ip: &[u8; 4], our_mac: &[u8; 6], target: &[u8; 4],
               serve_status: Option<&[u8; 19]>) -> Option<[u8; 6]> {
    let mut arp = [0u8; 42];
    for b in arp.iter_mut().take(6) { *b = 0xff; }   // eth dst = broadcast
    arp[6..12].copy_from_slice(our_mac);
    arp[12] = 0x08; arp[13] = 0x06;                  // ARP
    arp[14] = 0x00; arp[15] = 0x01;                  // htype Ethernet
    arp[16] = 0x08; arp[17] = 0x00;                  // ptype IPv4
    arp[18] = 0x06; arp[19] = 0x04;                  // hlen 6, plen 4
    arp[20] = 0x00; arp[21] = 0x01;                  // oper = request
    arp[22..28].copy_from_slice(our_mac);
    arp[28..32].copy_from_slice(our_ip);
    arp[38..42].copy_from_slice(target);             // target ip = who we ask for
    let req = Message::from_bytes(&arp);
    let mut arp_out = [0u8; 42];
    // RETRY like DHCP: re-send the request each attempt (a busy LAN, or a burst the device split across
    // bulk-INs, can lose one reply), then DRAIN + SCAN the ring for OUR reply, answering any gateway that
    // ARPs for US along the way so it can reach us.
    // SAY WHAT ARRIVED WHEN THIS FAILS.
    //
    // "no reply for the gateway" is the same message whether nothing came back, or plenty came back and
    // none of it was an ARP reply, or an ARP reply came from a host we did not ask about. Those have
    // different causes and this call has been the blocker for several runs, so count them apart: DHCP
    // completes on this link (OFFER then ACK, a real lease) while ARP does not, and the difference
    // between a broadcast exchange working and a unicast one failing is exactly what these numbers
    // separate.
    let mut seen = 0u32;      // frames scanned
    let mut arps = 0u32;      // ethertype 0x0806, any operation
    let mut replies = 0u32;   // ARP replies, from anyone
    let mut unicast = 0u32;   // frames addressed to OUR mac (not broadcast/multicast)
    // DID THE REQUEST ACTUALLY GO OUT? This was `let _ = nic_req(...)`, and discarding the outcome of a
    // send is the silent-failure this system forbids (CLAUDE.md 26.7): the driver can refuse a transmit
    // and the only trace was a reply that never came, which reads as "the network did not answer" and is
    // not the same thing at all. Three runs were spent asking whether these frames reached the wire when
    // the call site already knew and threw the answer away.
    let mut sent = 0u32;
    let mut send_fail = 0u32;
    // ONE MATCHER, TWO FEEDS. The frames reach this function by two different routes and both must be
    // examined the same way; two copies of a protocol matcher is how one of them drifts.
    //
    // Both routes are the drain now. There used to be a second one, and it is why this function failed
    // every time it ran on this board: `nic-driver` coupled a receive to every transmit and returned the
    // frame it caught as the answer to the SEND, which this call site discarded. A gateway answers an
    // ARP request in about a millisecond - squarely inside that poll - so the coupled receive caught our
    // ARP reply essentially every time and we threw it away. DHCP was unaffected only because an offer
    // takes tens of milliseconds and lands after the poll gives up, arriving through the drain.
    //
    // Scanning that reply fixed it; removing the coupling removed the shape that caused it, so a send
    // now answers nothing and there is one place a frame can arrive. The matcher stays factored anyway -
    // it is the protocol's definition of "is this our reply", and one copy of that is the right number.
    let mut result: Option<[u8; 6]> = None;
    for _ in 0..DANCE_TRIES {
        let mut scan = |f: &[u8], pending: &mut Displaced, result: &mut Option<[u8; 6]>| -> bool {
            seen += 1;
            if f.len() >= 6 && f[0..6] == our_mac[..] { unicast += 1; }
            if f.len() >= 22 && f[12] == 0x08 && f[13] == 0x06 {
                arps += 1;
                if f[20] == 0x00 && f[21] == 0x02 { replies += 1; }
            }
            // An ARP REPLY (oper 2) whose SENDER IP is the target we asked for (not some other host's).
            if f.len() >= 42 && f[12] == 0x08 && f[13] == 0x06 && f[20] == 0x00 && f[21] == 0x02
                && f[28] == target[0] && f[29] == target[1] && f[30] == target[2] && f[31] == target[3] {
                let mut m = [0u8; 6]; m.copy_from_slice(&f[22..28]);
                *result = Some(m);
                true
            } else {
                if build_arp_reply(f, our_ip, our_mac, &mut arp_out) {
                    // DECIDED, not overlooked: this is a courtesy reply to somebody else's ARP, sent
                // while we are draining for our own answer. If it fails, that host re-ARPs a moment
                // later and gets another chance - so the outcome carries no information we would act
                // on, and logging it from inside a scan loop would flood the console the moment
                // `nic-driver` is being restarted. Named here so it reads as a decision (§26.7).
                let _ = nic_req(ctx, pending, &Message::from_bytes(&arp_out), LINK_SECS);
                }
                false
            }
        };
        if nic_req(ctx, pending, &req, LINK_SECS).is_some() { sent += 1; } else { send_fail += 1; }
        drain_scan(ctx, pending, DANCE_SECS, serve_status, |f, pending| scan(f, pending, &mut result));
        if result.is_some() { return result; }
    }
    ctx.log_fmt(format_args!(
        "net-stack: ARP for {}.{}.{}.{} found nothing - {} sent {} SEND-FAILED, {} frames scanned, \
         {} to our MAC, {} ARP, {} ARP replies \
         (asking as {}.{}.{}.{} / {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x})",
        target[0], target[1], target[2], target[3],
        sent, send_fail, seen, unicast, arps, replies,
        our_ip[0], our_ip[1], our_ip[2], our_ip[3],
        our_mac[0], our_mac[1], our_mac[2], our_mac[3], our_mac[4], our_mac[5]));
    None
}

/// Calibrate the TSC frequency (Hz) against the RTC - the portable ground truth. The kernel's CPUID/PIT
/// calibration yields 0 on the AMD T630 (CPUID has no usable leaf; the PIT channel-2 output bit misbehaves),
/// but the RTC and `read_tsc` both work, so measure directly: align to a wall-clock second boundary, sample
/// the TSC, wait one more second, sample again - the delta is one second of TSC. Uses the DEGLITCHED epoch
/// so a CMOS misread cannot shorten the window; returns 0 (RTT then shows 0) if the result is implausible.
fn calibrate_tsc_hz(ctx: &ServiceContext) -> u64 {
    // Bound each wait (audit U11): if the RTC clock is frozen (never advances) this must NOT spin
    // forever and hang net-stack boot. A second is a few thousand yields even under load; ~50M is a
    // generous ceiling. Exceeding it means the clock is dead - return 0 (RTT then reports 0, the same
    // fallback as an out-of-range result) rather than block the whole service on a broken clock.
    const SPIN_MAX: u64 = 50_000_000;
    // Both bailouts SAY SO rather than returning a bare 0, for the same reason the range rejection
    // below does: "the wall clock never advanced" and "the counter reads implausibly" are different
    // faults with different fixes, and both used to arrive at `ping` as the identical symptom.
    let s0 = ctx.epoch_secs_monotonic();
    let mut n = 0u64;
    while ctx.epoch_secs_monotonic() == s0 {
        ctx.yield_cpu(); n += 1;
        if n > SPIN_MAX {
            ctx.log("net-stack: TSC calibration ABANDONED - the wall clock never advanced past its \
                     first reading (it is frozen or unavailable). RTT will read 0.");
            return 0;
        }
    }
    let t0 = ctx.read_tsc();
    let s1 = ctx.epoch_secs_monotonic();
    n = 0;
    while ctx.epoch_secs_monotonic() == s1 {
        ctx.yield_cpu(); n += 1;
        if n > SPIN_MAX {
            ctx.log("net-stack: TSC calibration ABANDONED - the wall clock advanced once and then \
                     stopped. RTT will read 0.");
            return 0;
        }
    }
    let hz = ctx.read_tsc().wrapping_sub(t0);
    // The floor is PER-ARCH, not one range widened to cover both. The ARM generic timer advances ~1 MHz
    // (the old 100 MHz floor rejected it, returning 0 -> the ping poll window `tsc_hz/3` collapsed to ~0
    // cycles and ping only caught a reply inside the initial drain - the ~50% "random" loss, RTT 0). But
    // simply lowering the floor everywhere would strip x86 of its protection: `deglitch_epoch` accepts a
    // forward jump of up to a day, so a CMOS misread that cuts the measurement window short yields a few
    // MHz on a GHz TSC - which the old floor rejected and a 0.5 MHz floor would accept, poisoning every
    // RTT and deadline for the life of the process. Each arch keeps the floor that fits its clock.
    // AArch64 belongs with arm, not with x86. The Pi 4's generic timer runs at ~54 MHz, which is BELOW
    // the 100 MHz x86 floor - so calibration returned 0 on every boot, and the paragraph above then
    // describes exactly what was observed on the board: RTT reported as 0 (rendered `time<1us`, which
    // is impossible for a round trip to 8.8.8.8) AND the poll window `tsc_hz/3` collapsing to ~0 cycles,
    // so a reply was only caught if it landed inside the initial drain. That is the 33% "packet loss" -
    // one broken constant presenting as two unrelated faults, a measurement bug and a throughput bug.
    //
    // The per-arch floor is right and stays; aarch64 was simply never added to it when the port arrived.
    // 500 kHz clears a 54 MHz timer comfortably while still rejecting a clock that is merely creeping.
    //
    // AND NEITHER WAS RISCV64, which the paragraph above then describes for a third time. Its
    // `read_cycle_counter` returns `time`, the constant-rate wall clock, which the device tree puts
    // at 4 MHz on the VisionFive and 10 MHz under QEMU - both far under the 100 MHz floor, so
    // calibration returned 0 on every boot. The operator reported it as "ping feels slow compared
    // with the other architectures", which is the second symptom exactly: the window is `tsc_hz/3`,
    // so at zero a reply is only caught if it happens to land inside the initial drain.
    //
    // Three arches have now hit one constant, which says the shape is wrong rather than the values:
    // a floor exists to reject a clock that is CREEPING, and 100 MHz is not a statement about that -
    // it is x86's own tick rate leaking into a portability check. 500 kHz is the honest floor for
    // every arch whose counter is a wall clock rather than a CPU cycle count, and x86 keeps the
    // higher one only because `deglitch_epoch` lets a CMOS misread yield a few MHz on a GHz TSC.
    //
    // SO THE DEFAULT IS NOW THE SAFE ONE AND X86 OPTS IN, rather than a list of three arches that a
    // fourth had to be added to. The paragraphs above are the record of that list being wrong three
    // times in a row, each time SILENTLY and each time surfacing as something else entirely: RTT
    // reported as 0 on the Pi 4, "33% packet loss" that was one constant presenting as two faults,
    // "ping feels slow" on the VisionFive. Written this way round, a fifth port that nobody has
    // thought about gets the floor that fits a wall-clock counter - which is what every non-x86
    // counter here has turned out to be - instead of inheriting x86's tick rate as a portability
    // check and failing in a way that does not name itself.
    //
    // x86 keeps the high floor by ASKING for it, with the reason attached, which is the only part of
    // this that was ever a real claim about a machine.
    let floor: u64 = if cfg!(target_arch = "x86_64") { 100_000_000 } else { 500_000 };
    if (floor..=10_000_000_000).contains(&hz) { return hz; }
    // AND IT SAYS SO. This returned 0 silently, and 0 means "RTT unavailable" three layers away - so
    // every one of the three failures above was diagnosed from its symptom (a wrong number in `ping`)
    // rather than from its cause, which was sitting right here as a measured value and a bound. A
    // rejected measurement is a failure, and invariant 12 says a failure is loud.
    ctx.log_fmt(format_args!(
        "net-stack: TSC calibration REJECTED - measured {} Hz, outside {}..=10000000000. RTT will \
         read 0 until this is resolved. A counter far BELOW the floor usually means this port's \
         read_tsc() is a fixed-rate wall clock rather than a CPU cycle count, and the floor above \
         needs to know that; far ABOVE means the wall-clock window was short (a misread epoch).",
        hz, floor));
    0
}

/// Send one ICMP echo of `payload_len` data bytes to `dest_ip` and wait for the reply. Returns
/// `Some((rtt_us, reply_ttl))` on an echo reply, `None` on timeout. The round trip is timed with the TSC
/// and converted to microseconds via `tsc_hz` (RTC-calibrated; 0 -> reported as 0).
/// Sends ONCE (the reply arrives with it), then, if the first frame back was a stray broadcast, drains a
/// BATCH of frames in ONE bounded [9] round-trip and scans it - so a reply behind broadcasts on a busy
/// LAN is caught without N slow re-queries (which pushed net-stack past the shell's deadline).
fn ping(ctx: &ServiceContext, pending: &mut Displaced, gw_mac: &[u8; 6], our_ip: &[u8; 4], our_mac: &[u8; 6], dest_ip: &[u8; 4],
        payload_len: usize, seq: u16, tsc_hz: u64, frames: &mut u16, timeouts: &mut u16) -> Option<(u16, u8)> {
    let plen = payload_len.min(PING_MAX_PAYLOAD);
    let flen = 42 + plen;
    let mut frame = [0u8; 42 + PING_MAX_PAYLOAD];
    frame[0..6].copy_from_slice(gw_mac);
    frame[6..12].copy_from_slice(our_mac);
    frame[12] = 0x08; frame[13] = 0x00;              // IPv4
    frame[14] = 0x45;
    let total_len = (20 + 8 + plen) as u16;
    frame[16] = (total_len >> 8) as u8; frame[17] = total_len as u8;
    frame[18] = 0x00; frame[19] = 0x01;
    frame[22] = 64;                                  // TTL (ours, outbound)
    frame[23] = 1;                                   // ICMP
    frame[26..30].copy_from_slice(our_ip);
    frame[30..34].copy_from_slice(dest_ip);
    let ip_ck = checksum(&frame[14..34]);
    frame[24] = (ip_ck >> 8) as u8; frame[25] = ip_ck as u8;
    frame[34] = 8;                                   // echo request
    frame[38] = 0x00; frame[39] = 0x01;              // id
    frame[40] = (seq >> 8) as u8; frame[41] = seq as u8;  // seq: UNIQUE per ping so a stale echo reply
                                                          // from a prior ping cannot match (RTT accuracy)
    // Data pattern (Windows sends the lowercase alphabet cycling); the reply echoes it back.
    for i in 0..plen { frame[42 + i] = b'a' + (i % 23) as u8; }
    let icmp_ck = checksum(&frame[34..42 + plen]);
    frame[36] = (icmp_ck >> 8) as u8; frame[37] = icmp_ck as u8;

    let t1 = ctx.read_tsc();
    let req = Message::from_bytes(&frame[..flen]);
    let mut arp_out = [0u8; 42];

    // Is `f` OUR echo reply? IPv4 / ICMP echo-reply (type 0) from dest_ip, echoing THIS ping's seq - so a
    // gateway ping and an internet ping cannot be confused, and a stale reply from a prior ping cannot
    // match. (`build_arp_reply` handles the other interesting frame: a gateway ARPing for us.)
    let is_echo = |f: &[u8]| -> bool {
        f.len() >= 42 && f[12] == 0x08 && f[13] == 0x00 && f[14] == 0x45
            && f[23] == 1 && f[34] == 0
            && f[26] == dest_ip[0] && f[27] == dest_ip[1] && f[28] == dest_ip[2] && f[29] == dest_ip[3]
            && {
                // Match the CURRENT seq OR a very recent one. At 1 ping/s with a small delivery lag,
                // a reply is delivered a ping or two behind the one that requested it, so there are
                // always a few OUTSTANDING requests - a reply should match any of them (this is how
                // ping tracks outstanding echoes), not only the newest. Exact-seq matching reported
                // loss on a link that works. A stale reply from long ago still cannot match (window is
                // small and backward-only), so a genuine dead link still shows loss.
                let s = ((f[40] as u16) << 8) | (f[41] as u16);
                seq.wrapping_sub(s) <= SEQ_MATCH_WINDOW
            }
    };
    // us = cycles * 1e6 / tsc_hz (RTC-calibrated; the kernel's CPUID/PIT calib yields 0 on the AMD T630).
    // Finer than ms so a sub-ms LAN RTT is distinguishable from a WAN one; capped at 65 ms (u16).
    let rtt_us = || -> u16 {
        let dt = ctx.read_tsc().wrapping_sub(t1);
        if tsc_hz > 0 { (dt.saturating_mul(1_000_000) / tsc_hz).min(65535) as u16 } else { 0 }
    };

    // 1. Send the echo. The reply to a SEND carries nothing now - `nic-driver` no longer couples a
    //    receive to a transmit, because that made every transmit a place a frame could be destroyed by
    //    a caller who did not want an answer. Frames arrive in step 2, which is the part whose job it is.
    if nic_req(ctx, pending, &req, LINK_SECS).is_none() { *timeouts += 1; }

    // 2. Poll for OUR reply until it arrives or a ~330 ms window closes, draining a BATCH of frames
    //    ([9]) each round and scanning it. The reply for a WAN host arrives tens of ms AFTER the echo -
    //    AFTER a single drain - so the old ONE-drain code raced the reply and lost, then discarded the
    //    late reply on the next seq (frames were being RETRIEVED, the ping still timed out). The window
    //    is bounded by read_tsc (tsc_hz-calibrated), a real sub-second wait, so a fast reply returns at
    //    once and a lost one gives up quickly - not the 1 s-granular epoch clock. Batch = [count:u8]
    //    then [len:u16 LE, bytes] per frame; nic-driver stays pure mechanism, the ICMP match lives here.
    // ~900 ms, NOT ~330 ms. The window must cover the worst case of the DELIVERY path, not of the
    // network: a reply that has arrived at the device still has to cross `dwc2` (which time-shares one
    // USB host channel with the keyboard and mass storage) and `nic-driver` before this loop can see it.
    // Measured RTTs here are 14-20 ms, so a 330 ms window looks generous - and the intermittent
    // "Request timed out" on a permanently plugged cable was this window closing on replies that were
    // still in the pipe. The code already knew: `SEQ_MATCH_WINDOW` exists precisely because "a reply is
    // delivered a ping or two behind the one that requested it", which is only true if the window can
    // expire before delivery. That was a compensation for the symptom; this is the cause.
    //
    // Still comfortably inside the shell's ~1 s ping cadence, so a genuinely dead host is still declared
    // dead within the same second and the pace does not change.
    // One drain's share of the window. ~15 fit in 900 ms, so the loop actually LOOPS instead of
    // spending its entire budget inside a single call - which is what `0 drains` meant.
    const DRAIN_SLICE_MS: u64 = 60;
    /// How long to wait for the driver to acknowledge an ARP reply sent from inside the window. The
    /// acknowledgement carries nothing, so this only has to be long enough not to be a busy-wait.
    const ARP_ACK_MS: u64 = 20;
    let deadline_cycles = if tsc_hz > 0 { (tsc_hz * 9) / 10 } else { 0 };   // ~900 ms
    // WITH NO CALIBRATED COUNTER, FALL BACK TO THE COARSE CLOCK - do not give up after one drain.
    //
    // `deadline_cycles == 0` meant "close immediately", so on a board whose counter is not yet
    // calibrated the FIRST ping of every boot got a one-drain window and declared the host dead. The
    // Pi 4 shows exactly that: `tsc_hz 0`, `budget 0 us`, closed after 0 us. A coarse bound is worse
    // than a fine one and far better than none.
    let coarse_t0 = ctx.epoch_secs_monotonic();
    let mut drains: u32 = 0;
    // WHO IS ASKING FOR US, AND WHAT ACTUALLY ARRIVES ADDRESSED TO US.
    //
    // Every failing exchange on this board is UNICAST - the echo reply, the DNS reply, the DHCP ACK -
    // and the one that never fails is the broadcast one, DISCOVER to OFFER. `arp_resolve` already
    // separates those two cases for its own failures and says why; this is the same question asked
    // where it can be seen, because a gateway can only address a unicast frame to a host whose ARP it
    // holds, and this stack answers an ARP for itself ONLY while it happens to be draining for some
    // other reason. Between operations - about 990 ms of every second after a ping SUCCEEDS - nobody
    // here answers at all.
    //
    // So: `arp-for-us` is how often we were asked while listening, and `to-our-mac` is how much of
    // what arrives is addressed to us rather than broadcast. Roughly one ARP per failing window would
    // say the gateway keeps losing us and the missing background responder is the cause; none at all
    // says it is a bystander and the unicast frames are being lost somewhere else entirely.
    let mut arp_for_us = 0u16;
    let mut to_our_mac = 0u16;
    // MEASURE THE DRAIN ITSELF, because a 60 ms bound on it did not change a 1.0 s window and I have
    // already been wrong once about why. Reports the FIRST drain only (one line per window, silent on
    // a healthy one) and prints what the SDK thought its budget was in cycles beside what the call
    // actually cost - if those disagree, the bound is not the number this code believes it is.
    let mut drain_reported = false;
    loop {
        let d_t0 = ctx.read_tsc();
        let d_msg = nic_drain_ms(ctx, pending, DRAIN_SLICE_MS);
        if let Some(b) = d_msg {
            let p = b.payload_bytes();
            let n = if p.is_empty() { 0 } else { p[0] as usize };
            let mut pos = 1usize;
            for _ in 0..n {
                if pos + 2 > p.len() { break; }
                let fl = u16::from_le_bytes([p[pos], p[pos + 1]]) as usize;
                pos += 2;
                if pos + fl > p.len() { break; }
                let f = &p[pos..pos + fl];
                pos += fl;
                *frames += 1;
                if f.len() >= 6 && f[..6] == our_mac[..] { to_our_mac += 1; }
                if is_echo(f) { return Some((rtt_us(), f[22])); }
                if build_arp_reply(f, our_ip, our_mac, &mut arp_out) {
                    // DECIDED, not overlooked: this is a courtesy reply to somebody else's ARP, sent
                // while we are draining for our own answer. If it fails, that host re-ARPs a moment
                // later and gets another chance - so the outcome carries no information we would act
                // on, and logging it from inside a scan loop would flood the console the moment
                // `nic-driver` is being restarted. Named here so it reads as a decision (§26.7).
                // BOUNDED, because this sits INSIDE the reply window and was spending all of it.
                //
                // A ping window is ~900 ms; this send waited up to LINK_SECS = 1 s for an
                // acknowledgement, on the whole-second clock, so a single ARP could outlast the entire
                // window. The Pi 4 showed exactly that: every failing window reported `1 frames seen`
                // and `0 drains` and ran 1.018 s - one frame arrived, it was the gateway ARPing for
                // us, and answering it consumed the window the echo reply needed.
                //
                // The wait is what is bounded, NOT the send: the frame is handed to the driver either
                // way, and the comment on step 1 already records that a SEND's reply "carries nothing
                // now". So this was a full second spent waiting for an acknowledgement with no content,
                // in the one place that could least afford it.
                arp_for_us += 1;
                let _ = ctx.request_with_reply_ms("nic-driver", &Message::from_bytes(&arp_out), ARP_ACK_MS);
                }
            }
        }
        // Give up once the reply window closes (or immediately if the clock is uncalibrated - one drain).
        // MEASURE THE WHOLE PASS, not one call inside it. The first instrument timed only the drain
        // and stayed silent - which was the answer (the drain is fast) but not the location. A pass is
        // drain + scan + any ARP reply, and reporting the pass covers whichever of them is slow next
        // time. First pass only, and only when it overran, so a healthy window prints nothing.
        if !drain_reported {
            drain_reported = true;
            let pass = ctx.read_tsc().wrapping_sub(d_t0);
            if tsc_hz > 0 && deadline_cycles > 0 && pass > deadline_cycles / 2 {
                ctx.log_fmt(format_args!(
                    "net-stack: ping pass #1 took {} us of a {} us window - the window is being spent before the reply can arrive",
                    pass.saturating_mul(1_000_000) / tsc_hz,
                    deadline_cycles.saturating_mul(1_000_000) / tsc_hz));
            }
        }
        let window_closed = if deadline_cycles == 0 {
            ctx.epoch_secs_monotonic().saturating_sub(coarse_t0) >= 1
        } else {
            ctx.read_tsc().wrapping_sub(t1) >= deadline_cycles
        };
        if window_closed {
            // SAY HOW THE WINDOW WAS SPENT. A timeout here is indistinguishable, from the outside, from
            // a network that dropped the packet - and the arithmetic says it is not that: consecutive
            // timeouts arrive 1.007 s apart when a 900 ms window plus the shell's 1 s pace should give
            // ~1.9 s, so the window is not lasting 900 ms. Guessing why has been wrong three times; this
            // prints the three numbers that settle it - how long we actually waited, how many times we
            // asked, and how many frames we saw while asking.
            let spent = ctx.read_tsc().wrapping_sub(t1);
            let us = if tsc_hz > 0 { spent.saturating_mul(1_000_000) / tsc_hz } else { 0 };
            ctx.log_fmt(format_args!(
                "net-stack: ping window closed after {} us ({} drains, {} frames seen, {} to-our-mac, {} arp-for-us, {} nic timeouts) [budget {} us, deadline {} cycles, tsc_hz {}]",
                us, drains, *frames, to_our_mac, arp_for_us, *timeouts,
                if tsc_hz > 0 { deadline_cycles.saturating_mul(1_000_000) / tsc_hz } else { 0 },
                deadline_cycles, tsc_hz));
            return None;
        }
        drains += 1;
        // PACE THE POLL - the same fix `drain_scan` already carries, which this loop never got.
        //
        // Without it this is `loop { nic_req(..) }` for up to 900 ms: thousands of requests a second at
        // `nic-driver` and, behind it, at the USB driver - the two services that have to FETCH the reply
        // we are waiting for. Asking ten thousand times a second does not make the answer arrive sooner;
        // it makes it arrive later, because the machinery that would produce it is busy answering us.
        //
        // The hardware said so plainly: successful replies come back in 13-30 ms, but a failing ping
        // times out at 900 ms and its reply then lands ~110 ms later - about a second of delivery for a
        // 15 ms round trip. Delivery was being starved by the polling that was waiting for it.
        //
        // `sleep` parks the task, so the core is free for the driver mid-fetch. 10 ms is one quantum:
        // fast enough that a reply is picked up promptly, slow enough to leave the driver alone.
        ctx.sleep(ctx.duration_cycles(RX_POLL_PACE_MS));
    }
}

/// What one run of the boot dance (DHCP -> ARP -> ICMP) learns: our IP, the gateway's MAC, whether ARP
/// resolved it, the DNS server, and the frozen 19-byte status record served to clients. Produced by
/// [`run_dance`] at boot AND re-produced on `net renew` (op 8), so a link that comes up AFTER boot is
/// recovered without a reboot - nothing is special; the LINK recovers like any restartable thing.
struct NetState {
    our_ip: [u8; 4],
    our_mac: [u8; 6],   // learned from the NIC (audit U9); [0;6] while unconfigured
    gw_mac: [u8; 6],
    gw_known: bool,
    /// Did DHCP actually grant this address, or is it the fallback guess?
    ///
    /// The distinction is the difference between recovering and not. `FALLBACK_IP` is a QEMU-slirp
    /// address that means nothing on a real network: with it we can ARP the gateway and set `gw_known`,
    /// look configured, and route nothing. The retry below is gated on being unconfigured, so a stack
    /// that fell back once stayed there for good - which is exactly what a restart under chaos produced.
    leased: bool,
    dns_server: [u8; 4],
    status: [u8; 19],
}

/// Learn our own MAC from nic-driver's `[3]` status reply (bytes 1..7). This is the one source of truth
/// for our hardware identity (Commandment III, audit U9): the controller burned it in, nic-driver read
/// it, and every frame we build advertises it. `None` on a short/zero reply = no NIC (or driver not up
/// yet) -> the caller stays unconfigured and retries via the auto-config-on-link path.
fn learn_our_mac(ctx: &ServiceContext, pending: &mut Displaced) -> Option<[u8; 6]> {
    let r = nic_status_req(ctx, pending, &Message::from_bytes(&[3u8]), LINK_SECS)?;
    let p = r.payload_bytes();
    if p.len() < 7 { return None; }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&p[1..7]);
    if mac == [0u8; 6] { None } else { Some(mac) }
}

/// Run the DHCP -> ARP -> ICMP dance once and freeze the 19-byte status. Called at boot, and again by the
/// `net renew` op so a cable plugged in after boot (or a link that came up late) reconfigures the stack in
/// place. Bounded (DHCP/ARP each have their own budget) and loud on each degrade, like the boot path.
fn run_dance(ctx: &ServiceContext, pending: &mut Displaced, serve_status: Option<&[u8; 19]>) -> NetState {
    // ---- Learn our MAC FIRST (audit U9 / Commandment III): every frame below advertises it as the eth
    // source. Without a NIC identity there is nothing to configure - degrade to unconfigured (same state
    // as no link); the auto-config-on-link path retries run_dance once the driver reports a MAC.
    let our_mac = match learn_our_mac(ctx, pending) {
        Some(m) => m,
        None => {
            ctx.log("net-stack: no NIC MAC yet (driver absent/not ready) - staying unconfigured");
            return NetState { our_ip: FALLBACK_IP, our_mac: [0u8; 6], gw_mac: [0u8; 6],
                              gw_known: false, leased: false, dns_server: GATEWAY_IP,
                              status: [0u8; 19] };
        }
    };

    // ---- Phase 3: DHCP FIRST, so net-stack LEARNS its own IP (self-configuring). Falls back to a default
    // only if there is no NIC / no offer (nic-driver serves empty replies). The IP it returns is the one
    // ARP + ICMP use below.
    let leased_cfg = dhcp_lease(ctx, pending, &our_mac, serve_status);
    let leased = leased_cfg.is_some();
    let (our_ip, gateway, dns_server) = leased_cfg.unwrap_or((FALLBACK_IP, GATEWAY_IP, GATEWAY_IP));

    // ---- Phase 2 step 1: ARP - who-has GATEWAY_IP, tell our_ip (a broadcast request).
    let mut arp = [0u8; 42];
    for b in arp.iter_mut().take(6) { *b = 0xff; }   // eth dest = broadcast
    arp[6..12].copy_from_slice(&our_mac);           // eth src
    arp[12] = 0x08; arp[13] = 0x06;                  // ethertype = ARP
    arp[14] = 0x00; arp[15] = 0x01;                  // htype = Ethernet
    arp[16] = 0x08; arp[17] = 0x00;                  // ptype = IPv4
    arp[18] = 0x06; arp[19] = 0x04;                  // hlen 6, plen 4
    arp[20] = 0x00; arp[21] = 0x01;                  // oper = request
    // RESOLVE THE GATEWAY WITH THE FUNCTION THAT WAITS FOR THE ANSWER.
    //
    // What stood here sent the ARP request and then inspected the ONE frame coupled to that transmit -
    // whatever happened to be in the receive ring at that instant, which is nothing, because the reply
    // has not come back yet. Six attempts, no waiting, and the whole loop finished in 31 ms on
    // hardware: "DHCP - ACK" at 16:54:20.200 and "ARP - no reply within the budget" at 16:54:20.231.
    // It never gave the gateway a chance to answer, so it had nothing to do with filters, MACs or the
    // network - the question was asked and the answer was not waited for.
    //
    // `arp_resolve` is the correct one and already existed: it sends the request, then DRAINS AND SCANS
    // the receive path for a reply whose sender IP is the host we asked about, answering anyone who
    // ARPs for us along the way, retrying the request each round. One implementation, used everywhere,
    // rather than two that disagree about whether waiting is part of asking.
    let (gw_mac, gw_known) = match arp_resolve(ctx, pending, &our_ip, &our_mac, &gateway, serve_status) {
        Some(m) => {
            ctx.log_fmt(format_args!(
                "net-stack: ARP - {}.{}.{}.{} is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                gateway[0], gateway[1], gateway[2], gateway[3],
                m[0], m[1], m[2], m[3], m[4], m[5]));
            (m, true)
        }
        None => ([0u8; 6], false),
    };
    if !gw_known {
        ctx.log("net-stack: ARP - no reply for the gateway within the budget - degrading");
    }

    // ---- Phase 2 step 2: ICMP - ping the gateway to confirm it answers. Only once ARP gave us its MAC.
    let (mut _pf, mut _pt) = (0u16, 0u16);
    let ping_ok = gw_known && ping(ctx, pending, &gw_mac, &our_ip, &our_mac, &gateway, 32, 0, 0, &mut _pf, &mut _pt).is_some();
    if ping_ok {
        ctx.log_fmt(format_args!("net-stack: ICMP - {}.{}.{}.{} echo reply (ping OK)",
            gateway[0], gateway[1], gateway[2], gateway[3]));
    } else if gw_known {
        ctx.log("net-stack: ICMP - no echo reply from the gateway");
    }

    // Freeze the result: our IP (4), the gateway IP (4), the gateway MAC (6), a flags byte (bit0 = gateway
    // resolved, bit1 = ping OK), and the DHCP-learned DNS server (4). The client formats it; we report raw
    // facts (utilities/0_conventions.md rule 7).
    let mut status = [0u8; 19];
    status[0..4].copy_from_slice(&our_ip);
    status[4..8].copy_from_slice(&gateway);
    status[8..14].copy_from_slice(&gw_mac);
    // bit 2 = DHCP granted this address (as opposed to the fallback guess). Published because it is
    // the difference between configured and merely reachable, and because `selfcheck` asserts on
    // it - a receive path that has stopped working shows up here as a stack that never got a lease.
    status[14] = (gw_known as u8) | ((ping_ok as u8) << 1) | ((leased as u8) << 2);
    status[15..19].copy_from_slice(&dns_server);
    let state = NetState { our_ip, our_mac, gw_mac, gw_known, leased, dns_server, status };

    // ---- Set the wall clock from the network (SNTP): the RTC-less Pi 2 has no other time source, so
    // `date` reads zero until this runs. Best-effort - a failure just leaves the clock unset (a re-sync is
    // available on demand via `date sync`). SKIPPED when the clock already reads a plausible date: on a
    // machine with a working RTC (x86) the kernel refuses SetClock anyway, and paying a multi-second
    // network exchange at every boot and every `net renew` for a syscall guaranteed to be denied is waste.
    if clock_epoch_if_set(ctx).is_some() {
        // nothing to do - the hardware clock is the authority here
    } else if let Some(unix) = sntp_sync(ctx, pending, &state) {
        ctx.log_fmt(format_args!("net-stack: SNTP - wall clock set (epoch {})", unix));
    } else if gw_known {
        ctx.log("net-stack: SNTP - no time reply within the budget - clock stays unset");
    }
    state
}

/// Read the NIC link state from nic-driver's `[3]` status. RTL8168: byte 7 = link up. On the QEMU e1000
/// path the reply is short (no link byte) - a non-empty reply means "up" (slirp's virtual link is always
/// up). Cheap; lets net-stack notice a cable plugged in after boot and self-configure without `net renew`.


/// Announce a cable coming or going on the CONSOLE, the way the USB drivers announce a keyboard or a
/// stick. Same idea, same place on screen, so "something was plugged in" reads the same whatever it was.
///
/// Uses `console_write` only, NOT `console_push`. That distinction is the whole security story here:
/// `console_write` is gated on LOG_WRITE, which this service already holds, while `console_push`
/// injects into the shell's INPUT ring and puts its holder inside the shell's trust perimeter (§6.4,
/// SEC-2 - keystrokes are commands). A network service has no business holding that, so the newline
/// goes inside the written string instead of being pushed. No new authority for a cosmetic feature.
fn link_notify(ctx: &ServiceContext, msg: &str) {
    ctx.console_write("
 NET: ");
    ctx.console_write(msg);
    // Just the fact, and NOTHING about what to do next.
    //
    // This printed "(press Enter to return to the prompt)" for one build. It is wrong whenever the
    // shell is not sitting at a prompt - during a continuous `ping`, for instance, which is exactly
    // when somebody is most likely to be pulling a cable. net-stack cannot know: it has no idea
    // whether the shell is idle, running a command, or muted behind a full-screen app.
    //
    // That is the SAME mistake as the redraw it replaced, in cheaper clothing. Both assume knowledge
    // of another service's state that this one does not have. The only honest thing to print is what
    // we actually know - the cable moved - so that is all we print.
    ctx.console_write("
");
}

/// Ask `nic-driver` for waiting frames, and PACE AN EMPTY ANSWER.
///
/// Three drain loops asked for frames as fast as the replies came back - one IPC round trip per
/// iteration, tens of thousands inside a ~900 ms ping window. That fills the driver's 16-deep queue
/// and costs both services a large slice of the core they share with `fs` and `block-driver`.
/// Observed on hardware as `nic-driver` at 50% with a full queue, which is what a flood looks like
/// from the receiving end. The user saw it in `observe` before any test did.
///
/// Only the EMPTY case pauses. A drain that returned frames goes straight back for more, so a busy
/// link is never slowed and a measured RTT is unaffected beyond the last idle millisecond. The
/// request rate falls from tens of thousands per window to about nine hundred.
///
/// RECORDED AS A COMPROMISE, not presented as the answer (26.7, Commandment VIII): a frame arriving
/// is truth and a millisecond is not. The honest fix is for `nic-driver` to notify on RX so this can
/// BLOCK instead of ask, which needs a protocol addition and is real work rather than a constant.
/// Living in one helper means that change lands in one place, and that a fourth drain loop cannot
/// quietly reintroduce the flood.
/// `nic_drain`, bounded in MILLISECONDS so it can fit inside a sub-second window.
///
/// The ping window is ~900 ms and cycle-based; this call was bounded by `LINK_SECS = 1` on the
/// WHOLE-SECOND clock, so ONE drain could outlast the entire window. It did: the Pi 4 reported
/// `closed after 1017663 us [budget 900000 us]` with `0 drains` - that counter increments after the
/// deadline check, so zero means the loop completed exactly one pass and that pass ate the budget.
/// Ping paces at 1 s, so a window running 1.018 s misses about every third reply, which reads as a
/// flaky network and is arithmetic.
///
/// A whole-second bound cannot fit inside a 900 ms budget, so the bound and the budget it must fit
/// inside are now read from the same clock. Abandoning on timeout is NOT new - the seconds variant
/// this replaces abandoned at 1 s; this only makes the wait shorter and sub-second.
/// A frame request bounded in MILLISECONDS, for work this service does unasked.
///
/// `nic_req` waits `LINK_SECS` (one second) and retries, so a single call can hold this service for
/// seconds when the driver is slow. That is the right budget for a CLIENT'S request - the client is
/// waiting and wants the answer - and the wrong one for the poll step, which is speculative: if the
/// driver is busy right now, the poll can simply not happen and try again in a hundred milliseconds.
///
/// **A poll that blocks longer than a client's patience is worse than a poll that is skipped.** On a
/// Pi 2 the shell asked net-stack to send ten bytes, net-stack was inside a poll, and the client's
/// five-second budget expired before it was answered: `serve: the echo was not accepted`, on a
/// connection that was working perfectly. Bounded in work is not bounded in time (§26.6).
fn nic_req_ms(ctx: &ServiceContext, pending: &mut Displaced, msg: &Message, ms: u64) -> Option<Message> {
    ctx.request_with_reply_ms_sifted("nic-driver", msg, ms, |m| {
        let badge = ctx.last_recv_badge();
        match ctx.take_pending_cap() {
            Some(cap) => { pending.note(ctx, m, badge, cap); false }
            None => true,
        }
    })
}

fn nic_drain_ms(ctx: &ServiceContext, pending: &mut Displaced, ms: u64) -> Option<Message> {
    // SIFTED, like every other conversation with the driver. This was the last unsifted one, and it
    // is on the ping and TCP paths - the busiest moment in this service, and so the likeliest moment
    // for a client to speak into a wait that would have swallowed it.
    ctx.request_with_reply_ms_sifted("nic-driver", &Message::from_bytes(&[9u8]), ms, |m| {
        let badge = ctx.last_recv_badge();
        match ctx.take_pending_cap() {
            Some(cap) => { pending.note(ctx, m, badge, cap); false }
            None => true,
        }
    })
}

fn nic_drain(ctx: &ServiceContext, pending: &mut Displaced) -> Option<Message> {
    let r = nic_req(ctx, pending, &Message::from_bytes(&[9u8]), LINK_SECS);
    let empty = match r.as_ref() {
        Some(m) => { let p = m.payload_bytes(); p.is_empty() || p[0] == 0 }
        None    => true,
    };
    // NO SLEEP HERE. The caller already paces an empty poll, so this was a SECOND delay stacked on
    // the first. Pacing belongs in one place, and on a NIC where the host must keep a bulk-IN
    // outstanding every millisecond not spent asking is a millisecond of frames its FIFO drops.
    r
}
fn link_is_up(ctx: &ServiceContext, pending: &mut Displaced) -> bool {
    match nic_status_req(ctx, pending, &Message::from_bytes(&[3u8]), LINK_SECS) {
        Some(r) => { let p = r.payload_bytes(); if p.len() > 7 { p[7] != 0 } else { !p.is_empty() } }
        None    => {
            // NOT the same as "the link is down", and saying so cost real time. A timeout means the
            // NIC DRIVER did not answer; the cable may be perfectly well seated. Reporting that as
            // "unplugged" is a silent fallback wearing a diagnosis (26.7) - it sends the reader to
            // check a cable when the fault is that a service was starved of CPU, which is exactly
            // what happened here: `control` was burning 93% of the core `nic-driver` shares.
            //
            // Still returns false, because unconfigured-and-responsive is the right POSTURE when the
            // link cannot be confirmed either way. What changes is that the reason is now on the
            // record instead of a guess presented as a fact.
            ctx.log("net-stack: nic-driver did not answer the link query - treating as no link, but this is a TIMEOUT, not a reading (the cable may be fine)");
            false
        }
    }
}

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // DECLARE THIS SERVICE'S NAME, once. Identity is not ambient - a service cannot ask what it is
    // called - so a traced service says. Without it every event reads `?` in the caller column, and
    // worse, every METRIC published lands under a BLANK owner: the metric key is (owner, name), so
    // ten unnamed services all collide into one row and their counters interleave. Observed as a
    // single `msgs.received 1920` belonging to nobody.
    ctx.trace_as("net-stack");
    // Force the EL0 fault the kernel's recovery path must survive (this crate's `el0-fault-test`
    // feature). The kernel must KILL this task and keep running, and the supervisor must restart it.
    // If the machine stops here instead, the recovery is broken and the last log line names the task.
    #[cfg(feature = "el0-fault-test")]
    {
        ctx.log("net-stack: el0-fault-test - deliberate null read; the kernel must kill ME, not the machine");
        godspeed_sdk::adversarial::fault_null_read();
        ctx.log("net-stack: STILL ALIVE after a null read - the kernel did NOT fault-kill this task");
    }
    ctx.log("net-stack: starting");
    // Announce the API BEFORE the configuration dance. The dance can take seconds (DHCP and ARP each
    // wait out their budget when there is no link), and logging after it meant this line landed on the
    // console AFTER the shell had already printed its prompt - leaving `gsh> net-stack: ...` on one
    // line at boot. Announcing first is also the more honest order: this reports that the service came
    // up, and the dance below reports its own result (offer/no offer) as it happens.
    ctx.log("net-stack: serving the client API (status/dns/socket/tcp)");

    // PROVE THE PARTS THE NETWORK CANNOT REACH, on every boot, on every board.
    //
    // Congestion control reacts to loss, fast retransmit to three duplicate acknowledgements and the
    // persist timer to a window the peer has closed. The QEMU backend this branch is tested against
    // drops nothing, reorders nothing and never shuts its window, so none of those paths is exercised
    // by any test that uses a network - and a guard nobody has seen fire is not evidence.
    //
    // The same shape as the kernel's `iommu: selftest PASS`, and for the same reason: the property is
    // negative and cannot be produced on demand, so it is asserted where it can be, at a cost worth
    // paying. That cost is memory and arithmetic only - no frame is transmitted, no driver is asked
    // anything - and it is paid once, before the first client can be served.
    {
        let (p, f) = tcp::selftest(&ctx);
        if f == 0 {
            ctx.log_fmt(format_args!("net-stack: tcp selftest PASS - {} checks", p));
        } else {
            // LOUD, and it does not stop the service: a stack with a broken congestion window still
            // carries traffic, and refusing to serve would turn a degraded network into none at all
            // (Commandment V). The failing checks named themselves on the lines above.
            ctx.log_fmt(format_args!(
                "net-stack: tcp selftest FAILED - {} of {} checks did not hold; TCP is DEGRADED",
                f, p + f));
        }
    }

    // THE TCP TABLE, owned here rather than in a static: a service holds no unowned global mutable
    // state (Commandment VI). This struct IS the memory cost of TCP in this service, and its bounds
    // are the constants at the top of `tcp.rs`.
    //
    // The clock is read ONCE, here, and its absence is reported once. `backlog/27`: a deadline built
    // from an uncalibrated clock collapses to now, so a stack that asked per-timer would silently
    // retransmit instantly and forever on a port without calibration.
    // The displaced-request counter, owned here for the same reason the TCP table is: a service holds
    // no unowned global mutable state (Commandment VI). `&mut Displaced::new()` rather than a `mut`
    // binding because every call site below wants a `&mut` and reborrowing one binding is quieter
    // than writing `&mut pending` sixteen times.
    let pending = &mut Displaced::new();
    /// When the last poll step ran. The poll is owed every `POLL_MS`, and this is what makes that a
    /// schedule rather than a hope - see the loop below.
    let mut last_poll: u64 = 0;
    // When this service last reached the top of its wait. See `SLOW_PASS_MS`.
    let mut last_pass: u64 = 0;
    let mut slow_passes: u32 = 0;
    // OUTSIDE the loop deliberately: inside, it is a kilobyte of zeroing on every single request, to
    // hold something that is normally not there.
    let mut heldbuf = [0u8; HELD_BYTES];

    let mut tcpst = tcp::Tcp::new(calibrate_tsc_hz(&ctx), ctx.read_tsc());
    tcpst.warn_if_no_clock(&ctx);

    // Configure the stack (DHCP -> ARP -> ICMP). These are `mut` because `net renew` (op 8) re-runs the
    // dance in place - a link that comes up after boot recovers without a reboot.
    //
    // SKIPPED ENTIRELY WHEN THERE IS NO LINK. The dance is ~25 s of DHCP and ARP budgets, and it runs
    // on this thread - net-stack's serve loop is single-threaded, so for that whole time it cannot
    // answer a client. Boot with the cable out and every `ping` reported "net-stack not responding",
    // which is both useless and untrue: the service was alive, the cable was not. Hardware showed the
    // two facts side by side - `nic-driver: genet up ... link down (no cable?)` at 10:33:53, then the
    // dance grinding through its budgets from 10:34:05.
    //
    // The REQUEST path already checks the link before dancing; only this boot call did not, so the
    // guard existed and this one site went around it.
    //
    // Nothing is lost by skipping: `link up while unconfigured - auto-configuring` already re-runs the
    // dance the moment a cable appears, so a machine booted unplugged configures itself on plug-in
    // rather than needing `net renew` or a reboot. Cheap too - one status query to the NIC, seconds
    // saved on every diskless-network boot.
    // SERVE FIRST, CONFIGURE FROM INSIDE THE LOOP. This used to run the whole DHCP -> ARP -> ICMP
    // dance HERE, before the serve loop existed - so on a machine whose link is up but whose DHCP
    // server never answers, net-stack was DEAF for the ~45 s its budgets take. Nothing could reach it:
    // `net` reported "net-stack unavailable", `time` logged "cannot reach net-stack", ping had no
    // stack to ping with, and the shell's own clock probe waited on a service that was not listening.
    // One blocking dependency at startup made the whole machine look broken.
    //
    // The no-link branch already had the right instinct and says so in its own message - "staying
    // unconfigured and RESPONSIVE". The link-up branch never got the same treatment. Now both start
    // the same way: unconfigured, serving, configuring on the first demand. The loop already dances
    // on demand in four places, one of which is `time` asking for the clock - and `time` asks within
    // a second of boot - so a cabled machine still self-configures immediately, with its endpoint
    // live and requests QUEUING behind the dance instead of finding nobody home.
    //
    // RESIDUAL, recorded rather than implied away (§26.7): the in-loop dance still blocks this service
    // while it runs, so a client whose deadline is shorter than the dance still times out - it just
    // gets a bounded, reported timeout instead of a service that was never listening. Making the dance
    // incremental so net-stack answers THROUGHOUT it is the real fix, and that is a rework of the
    // state machine rather than a constant.
    // SELF-CONFIGURE AT BOOT, AND ANSWER WHILE DOING IT. Both, now that the dance serves.
    //
    // The dance used to run here as one blocking sequence, so a cabled machine configured itself but
    // was DEAF for the ~45 s its budgets take when nothing answers - `net` said "net-stack
    // unavailable", `time` said "cannot reach net-stack", ping had no stack. The intermediate fix
    // was to skip the boot dance and configure on first demand, which only moved the deafness inside
    // the loop. Neither is needed: `run_dance_serving` answers throughout, so the boot dance is back
    // and costs no responsiveness. Clients asking during it get the truthful unconfigured status.
    let d = if link_is_up(&ctx, pending) {
        run_dance(&ctx, pending, Some(&[0u8; 19]))
    } else {
        ctx.log("net-stack: no link at boot (cable unplugged?) - staying unconfigured and RESPONSIVE; will configure when the link comes up");
        // The unconfigured state, spelled out rather than defaulted: no IP, no gateway, no DNS. The
        // MAC is still learned - it is our hardware identity and true with or without a cable
        // (Commandment III / audit U9) - so `net` can report who we are while saying we are offline.
        NetState {
            our_ip: [0; 4],
            our_mac: learn_our_mac(&ctx, pending).unwrap_or([0; 6]),
            gw_mac: [0; 6],
            gw_known: false,
            leased: false,     // no cable, so certainly no lease
            dns_server: [0; 4],
            status: *b"link down (no cable",
        }
    };
    let mut our_ip = d.our_ip;
    let mut our_mac = d.our_mac;                   // learned from the NIC (audit U9), re-learned on each dance
    let mut gw_mac = d.gw_mac;
    let mut gw_known = d.gw_known;
    let mut dns_server = d.dns_server;
    let mut status = d.status;
    let mut sockets = [Socket { rid: 0, port: 0 }; MAX_SOCKETS];
    let mut ping_seq: u16 = 0;                    // unique ICMP seq per ping - see ping() (RTT accuracy)
    // LAZY, because calibrating costs two full seconds of spinning and only `ping` needs it.
    // `calibrate_tsc_hz` aligns to a wall-clock second boundary and then waits out another whole
    // second, yielding the entire time - so as a startup step it was two more seconds during which
    // this service answered nobody, and two seconds of a permanently-runnable task on the core it
    // shares with `fs` and `block-driver`. Paid now by whoever actually asks for an RTT, once.
    let mut tsc_hz: u64 = 0;
    // BOUNDED (26.6). `calibrate_tsc_hz` costs two full seconds of spinning, and the call site below
    // re-ran it on EVERY ping while it kept returning 0 - so on a port whose floor was wrong, each
    // ping paid two seconds to fail again. That is a second, larger cause of the "ping feels slow"
    // report the floor comment records, and nobody named it because the failure was silent.
    //
    // Not zero retries, though: an early ping can genuinely precede a usable wall clock (the `time`
    // service sets it from SNTP later), so a later attempt can legitimately succeed. Three attempts,
    // then stop asking and say so once.
    const TSC_CAL_TRIES: u8 = 3;
    let mut tsc_cal_tries: u8 = 0;
    // Outside the loop deliberately: a once-only latch declared inside the loop it guards resets every
    // iteration and reports every time, which is the flood it exists to prevent.
    let mut capless_logged = false;
    // When the last automatic SNTP retry ran (monotonic seconds). See RESYNC_SECS.
    let mut last_resync_at: i64 = 0;
    // Did DHCP grant `our_ip`, or is it the fallback guess? See `NetState::leased`.
    let mut leased = d.leased;
    // When the last automatic re-DHCP ran, so an unleased stack retries without dancing per request.
    // One RESYNC_SECS in the past, so the FIRST time a retry is wanted it happens at once and only the
    // repeats are spaced. Zero meant "sixty seconds of uptime before the first attempt", which on a
    // machine that boots in fifteen is a minute of no network for no reason - and it would now delay the
    // auto-configure below too, which must answer a cable being plugged in promptly.
    let mut last_redhcp_at: i64 = -RESYNC_SECS;
    // Same, for the gateway-only ARP retry below - separate from the DHCP one so a re-dance and a
    // gateway retry cannot consume each other's budget.
    let mut last_gw_arp_at: i64 = -RESYNC_SECS;
    /// Latched once the wall clock is known. A clock never becomes unset, so this is asked at most once.
    let mut clock_known = false;
    // Labelled so the wait below can hand control back here when the poll step displaces a client
    // request into the stash - `pending.take()` at the top of this loop is the only thing that
    // drains it. See the `has_work` call site.
    'serve: loop {
        // A BARE BLOCK, deliberately - the idle tick that was here is REVERTED (audit A10-1/A5-2).
        //
        // The tick called `link_is_up()` every second to announce a cable, and that goes through
        // `nic_req` -> a wait loop that `try_recv`s THIS SAME serve endpoint and returns whatever
        // lands. So once a second it opened a window where a CLIENT request was read as the NIC's
        // status reply: never served, its reply cap left on the kernel's pending FIFO (so the next
        // reply went to the wrong client), and the real NIC reply then parsed as a different op.
        //
        // Two independent audits found it. It is a correctness bug bought with a cosmetic feature -
        // announcing an unplug without being asked - so the feature goes. `NET: ethernet cable
        // connected` still appears on the request path, where net-stack is answering anyway.
        //
        // The lesson for whoever restores it: net-stack serves clients and receives nic-driver replies
        // on ONE untagged endpoint. Anything that talks to the NIC outside of serving a request will
        // steal messages. Fix the correlation BEFORE adding a tick, not after - the design is written
        // up in `docs/net-tags-design.md` (three phases, each independently testable). A second
        // endpoint was considered and is NOT available: there is no CreateEndpoint syscall and the SDK
        // carries one recv_slot.
        // A REQUEST DISPLACED BY OUR OWN WORK IS SERVED FIRST, and only then the endpoint.
        //
        // The one case this covers, measured rather than imagined: `time` nudges this service for the
        // network clock (op 11, one-way, no reply cap), net-stack runs an SNTP exchange inline, and a
        // client that spoke during it used to be lost - the shell then waited out its whole deadline
        // before retrying, and on a slow host the QEMU TCP test failed about one run in three because
        // of it, both before and after this service learned to sift.
        //
        // The three things a request needs are the same whichever way it got here, so from `pl`
        // downwards this loop cannot tell the difference - which is what keeps this from needing a
        // second copy of every op.
        let req;
        // Which door the request came in by. A request served from the stash was displaced by a
        // driver conversation and is arriving LATE; one from the queue arrived directly. When a
        // client says it got no answer, that difference is the first thing worth knowing.
        let mut from_stash = true;
        let (pl_raw, badge, reply_cap) = match pending.take(&ctx, &mut heldbuf) {
            Some((len, badge, reply)) => (&heldbuf[..len], badge, reply),
            None => {
                // WAIT FOR A CLIENT, BUT NOT FOREVER - answer for ourselves in the gaps.
                //
                // Two guards, and both of them refuse to poll rather than poll wrongly:
                //
                // UNCONFIGURED. With no address of our own there is nothing on the wire that is ours
                // to answer, so blocking is both correct and free.
                //
                // NO CALIBRATED CLOCK. `duration_cycles` floors to ONE QUANTUM when the counter is
                // uncalibrated (`backlog/27`), so a bounded wait silently becomes a spin and this
                // loop would ask the driver for frames as fast as it can be scheduled - saturating
                // `nic-driver` and, behind it, the USB stack. That is the silent-clock trap the
                // backlog item is about, and the honest response to a missing clock is to not use it.
                req = loop {
                    // ---- IS THIS SERVICE ACTUALLY LOOPING? ----
                    //
                    // A request that sits in the endpoint queue for seconds has exactly two
                    // explanations, and they are different bugs with different fixes: this loop is
                    // STARVED (blocked inside a driver conversation or a long op, so it never asks
                    // for the message), or this loop is RUNNING and the message was not delivered to
                    // it. Nothing in the log could tell them apart - the service is silent when
                    // healthy, and silence is also what a stall looks like.
                    //
                    // So: measure the gap between consecutive arrivals HERE. It covers both halves,
                    // because `last_pass` is not updated while a request is being served either - a
                    // slow dispatch shows up as the next pass being late.
                    //
                    // Measured on a Dell Wyse, where the SAME command took 0.49 s, 4.8 s and 20 s to
                    // reach dispatch on one boot (backlog/29). Reported rather than counted silently,
                    // and only when it is genuinely slow, so a healthy service still prints nothing.
                    let now_pass = ctx.read_tsc();
                    if last_pass != 0 {
                        let gap = now_pass.wrapping_sub(last_pass);
                        if gap >= ctx.duration_cycles(SLOW_PASS_MS) {
                            slow_passes = slow_passes.saturating_add(1);
                            ctx.log_fmt(format_args!(
                                "net-stack: a serve pass took {} ms (over {}) - not asking for client requests during it (slow pass #{})",
                                gap / ctx.duration_cycles(1).max(1), SLOW_PASS_MS, slow_passes));
                        }
                    }
                    last_pass = now_pass;
                    if !gw_known || !tcpst.have_clock() { break ctx.recv(); }
                    // ---- THE POLL IS A PERIODIC OBLIGATION, NOT AN IDLE-TIME FILLER ----
                    //
                    // Checked BEFORE the wait, and on every pass, so it happens at least every
                    // `POLL_MS` no matter how busy this service is.
                    //
                    // **It used to run only when `recv_timeout` EXPIRED, and hardware found what
                    // that costs.** `serve` polls accept about ten times a second, which is roughly
                    // the poll interval - so the timeout almost never fired, the poll almost never
                    // ran, and the machine stopped answering ARP while it was waiting to be
                    // connected to. The accept polling starved the very poll step that makes accept
                    // possible: a laptop could not even resolve the board's address to send it a
                    // SYN. `ping` to it went from twenty replies out of twenty to `Destination host
                    // unreachable`, with nothing in the log to say why.
                    //
                    // QEMU hid it. There the guest never answers ARP for the host at all - SLIRP
                    // does that - and the timing is fast enough that a poll always slipped through,
                    // so `tcp_serve_test` passed while the same code was starving on a Pi 2.
                    //
                    // Any work owed on a schedule has to be driven by the schedule. Gating it on the
                    // service being idle means the busier it gets, the less it keeps its promises -
                    // which is exactly backwards.
                    if ctx.read_tsc().wrapping_sub(last_poll) >= ctx.duration_cycles(POLL_MS) {
                        last_poll = ctx.read_tsc();
                        let st = NetState { our_ip, our_mac, gw_mac, gw_known, leased,
                                            dns_server, status };
                        // The gateway is the FALLBACK address for anything this poll originates;
                        // every established connection carries its own peer MAC on the `Conn`,
                        // so `poll_one` addresses its frames correctly whatever is passed here.
                        let net = tcp::Net { our_mac, peer_mac: gw_mac, our_ip };
                        poll_step(&ctx, pending, &st, &mut tcpst, &net);
                    }
                    if let Some(m) = ctx.recv_timeout(ctx.duration_cycles(POLL_MS)) { break m; }
                    // ---- DO NOT SLEEP ON WORK WE ALREADY HAVE ----
                    //
                    // The poll step just ran, and `nic_req`'s sifting displaces any client request
                    // that arrives during it INTO THE STASH. The stash is only ever drained by
                    // `pending.take()` at the top of the serve loop - and this wait only exits when a
                    // NEW message arrives. So a displaced request sat here unserved until some
                    // unrelated message happened along, which for a shell blocked on that very
                    // request means nothing ever came: it aged out its client's whole patience and
                    // was dropped, the client re-sent, and the re-send was answered at once.
                    //
                    // That is precisely what the board showed - `dropped a held client request (op
                    // 21) after its client's own 20000 ms of patience`, with only a 2 s slow pass in
                    // the window, and every successful "from the stash" dispatch landing immediately
                    // after an unrelated one had woken the loop (`backlog/29`).
                    //
                    // Going back to the top of the serve loop is the whole fix: `take` is there, and
                    // it is the only thing that drains the stash.
                    if pending.has_work() { continue 'serve; }
                };
                // A nonzero badge = a SOCKET-CAPABILITY invocation the kernel validated (§7.10). A plain
                // name-addressed request (status / DNS / open-socket) carries no badge.
                let badge = ctx.last_recv_badge();
                let reply_cap = match ctx.take_pending_cap() {
                    Some(c) => c,
                    // A request with no reply cap cannot be answered - but dropping it SILENTLY means the
                    // client waits out its deadline and calls net-stack unresponsive while our log shows a
                    // clean run. Say it once (the condition repeats per request, and the report must not
                    // become the flood), then drop it.
                    None => {
                        // OP_SYNC_NOW (11): a ONE-WAY nudge from `time`, deliberately carrying no reply cap.
                        //
                        // `time` owns the wall clock and must be the thing that pursues it, but it cannot ASK
                        // for a sync in the ordinary way: this service calls `time` after SNTP, so a request in
                        // the other direction would have two single-threaded services blocked on each other -
                        // which is why `time`'s contract says it may never send here. A message with nothing to
                        // answer breaks that: `time` sends and forgets, this service does the work and pushes
                        // the result back exactly as it already does, and neither ever waits on the other
                        // (§8.9 - one direction non-blocking is the whole requirement).
                        //
                        // It sits in the capless arm because that is precisely what identifies it. There is no
                        // reply to send, so there is no cap, and no legitimate request can be confused with it.
                        if req.payload_bytes().first() == Some(&11) {
                            let mut configured_now = false;
                            // Only worth attempting with a resolved gateway - SNTP needs somewhere to send.
                            // `time` asks repeatedly while unsynced, so a refusal here costs nothing and the
                            // next nudge finds the network ready.
                            // CONFIGURE FIRST IF THERE IS A CABLE BUT NO ROUTE. Every other request that
                            // needs the network gets this treatment further down the loop, and the nudge never
                            // reached it - it answers here and continues. So a machine booted unplugged, then
                            // plugged in, would sit unconfigured forever unless somebody typed a network
                            // command: `time` asked every twenty seconds and was told "no route yet" every
                            // time, which is true and useless. Asking for the clock IS a request that needs
                            // the network, so it gets the same self-configure as the rest.
                            if !gw_known && link_is_up(&ctx, pending) {
                                ctx.log("net-stack: `time` asked for the clock and the cable is in - configuring");
                                let d = run_dance(&ctx, pending, Some(&status));
                                our_ip = d.our_ip; our_mac = d.our_mac; gw_mac = d.gw_mac;
                                gw_known = d.gw_known; leased = d.leased; dns_server = d.dns_server;
                                status = d.status;
                                // THE DANCE ALREADY SYNCED. `run_dance` ends in its own SNTP exchange, so
                                // falling through to another one queries the server twice in a fifth of a
                                // second for an answer we have - the duplicate `querying` / `wall clock set`
                                // pair in the log. Configuring IS resolving here; there is nothing left to ask.
                                configured_now = true;
                            }
                            if !configured_now && gw_known && link_is_up(&ctx, pending) {
                                let st = NetState { our_ip, our_mac, gw_mac, gw_known, leased, dns_server, status };
                                match sntp_sync(&ctx, pending, &st) {
                                    Some(u) => ctx.log_fmt(format_args!(
                                        "net-stack: clock resolved at `time`'s request ({})", u)),
                                    // SAY SO. A nudge that arrived and got nowhere is a different fault from a
                                    // nudge that never arrived, and with both silent the two are one mystery.
                                    None => ctx.log("net-stack: `time` asked for the clock - no SNTP answer"),
                                }
                            } else {
                                ctx.log("net-stack: `time` asked for the clock - no route yet, will retry");
                            }
                            continue;
                        }
                        if !capless_logged {
                            capless_logged = true;
                            ctx.log("net-stack: request had no reply cap - dropping (cannot answer without one)");
                        }
                        continue;
                    }
                };
                from_stash = false;
                (req.payload_bytes(), badge, reply_cap)
            }
        };
        // ---- THE CORRELATION TAG, stripped HERE and nowhere else ----
        //
        // Every name-addressed request carries one byte at offset 0 that this service echoes back
        // and never interprets. It exists so a client can tell an answer to THIS question from an
        // answer to one it has already given up on and re-asked - the hazard that made
        // `docs/net-tags-design.md` phase 3 unsafe, written up in its §7.2 with the log that killed
        // the first attempt.
        //
        // Stripped in ONE place, echoed in ONE place (`Reply::send`), so not a single op arm below
        // knows the tag exists and there is no per-op shift to get wrong. That is the shape `fs`
        // arrived at, and its comment says exactly why: "the tag is handled here and nowhere else,
        // which is why adding it did not touch a single arm".
        //
        // A BADGED request is untagged: it is a socket capability invoking its owner, the badge
        // already names the socket, and the client holds no ambiguity to resolve.
        let (pl, reply) = match badge {
            Some(_) => (pl_raw, Reply { cap: reply_cap, tag: None }),
            // TWO header bytes: the tag to echo, and how long the client will wait (used by the
            // stash, in `Displaced::note`, and of no interest to any arm below). Stripped together
            // here so that - exactly as with the tag alone - not one op arm knows either exists.
            None => match (pl_raw.first(), pl_raw.len()) {
                (Some(t), n) if n >= 2 => (&pl_raw[2..], Reply { cap: reply_cap, tag: Some(*t) }),
                // A single byte, or none at all. Nothing to strip and nothing to echo; the default
                // arm answers status, exactly as it did before there were tags.
                _ => (pl_raw, Reply { cap: reply_cap, tag: None }),
            },
        };
        // ARRIVAL RECEIPT for the two ops a person waits on, and ONLY those two.
        //
        // `tcp` and `serve`'s listen are typed by hand and answered in one round trip, so one line
        // each is no flood - unlike the status and accept polls, which run ten times a second and are
        // deliberately left silent. Its whole job is to separate "the request never reached dispatch"
        // from "it reached dispatch and the work went wrong", which on a Wyse could not be told apart:
        // net-stack logged nothing for a `tcp ... big` while it went on answering ping, and every
        // silent loss path was eliminated by reading except one (backlog/29).
        if matches!(pl.first(), Some(&21) | Some(&22)) {
            ctx.log_fmt(format_args!("net-stack: op {} reached dispatch (from the {})",
                                     pl[0], if from_stash { "stash" } else { "queue" }));
        }
        // AUTO-CONFIGURE: while UNCONFIGURED (no gateway - booted with no cable, or a boot dance that met a
        // dead link), a request that needs the network first checks the NIC link; if it has come up
        // (cable plugged in), re-run the dance IN PLACE so the network self-configures - no `net renew`.
        // That is EVERY network-using op, not just `net`/`ping` (audit U12): DNS (op 1) and ARP (op 6)
        // equally need `our_ip`/`gw_known`, so a `net dns`/`net arp` on a freshly-plugged cable must
        // trigger the same self-configure. (op 8 `renew` forces a dance already; op 2 `open` only mints.)
        // Gated on !gw_known so a configured stack pays nothing, and retried per request so the PHY's
        // few-second post-cable auto-negotiation eventually catches. Once configured the gateway MAC
        // persists, so a later unplug/replug just resumes (the ICMP flows again) without re-dancing.
        let mut synced_by_dance = false;
        // RE-SYNC THE CLOCK WHILE IT IS STILL UNSET.
        //
        // The boot dance ends in one SNTP attempt, and that used to be the ONLY automatic one: if it
        // failed (DNS not up yet, the server silent, the cable in a second later) the clock stayed unset
        // until somebody typed `date sync`. A machine with a working network and a permanently plugged
        // cable would sit at 1970 indefinitely, which is exactly what a wall clock must not do.
        //
        // So: while the clock is unset and the link is up, retry - spaced by RESYNC_SECS so the cost is
        // one exchange a minute rather than one per request, and skipped entirely the moment the clock
        // is known (the common case pays a cheap `clock_epoch_if_set`). `time` owns the result; this
        // only fetches it.
        // ORDER MATTERS, cheapest first, because this sits on every network request.
        //
        // `clock_known` is a local latch: once the clock is set it can never become unset again (there
        // is no path in `time` back to SRC_NONE), so after the first success this whole block costs one
        // bool test forever. It used to ask `time` over IPC on every request to find that out - a round
        // trip per request, permanently, to re-learn something that cannot change.
        //
        // Then the elapsed-time test (local, free), and only then the two IPCs.
        if badge.is_none()
            && !clock_known
            && matches!(pl.first(), Some(&0) | Some(&1) | Some(&3) | Some(&6))
            && ctx.epoch_secs_monotonic() - last_resync_at >= RESYNC_SECS
        {
            if clock_epoch_if_set(&ctx).is_some() {
                clock_known = true;              // latched: never ask again
            } else if link_is_up(&ctx, pending) {
                last_resync_at = ctx.epoch_secs_monotonic();
                let st = NetState { our_ip, our_mac, gw_mac, gw_known, leased, dns_server, status };
                if let Some(unix) = sntp_sync(&ctx, pending, &st) {
                    ctx.log_fmt(format_args!("net-stack: SNTP retry - wall clock set (epoch {})", unix));
                    clock_known = true;
                    synced_by_dance = true;
                }
            } else {
                // No link: do not burn a minute of the retry budget waiting for a cable. Leave
                // `last_resync_at` alone so the next request after a plug-in tries at once.
            }
        }
        // RE-DHCP WHILE RUNNING ON THE FALLBACK ADDRESS.
        //
        // `gw_known` alone is the wrong test for "configured", in BOTH directions - which is why the
        // flag is no longer called `have_mac` and why neither block below tests it alone.
        //
        // One way: DHCP can fail, leaving `our_ip` as `FALLBACK_IP` - a QEMU-slirp address that routes
        // nothing on a real network - while the ARP for the gateway still succeeds and sets `gw_known`.
        // The stack then LOOKS configured, nothing re-dances, and the machine sits on a useless address
        // for good. A chaos restart that missed its DHCP window ended exactly there, which is why a run
        // could finish with the network down and nothing retrying.
        //
        // The other way, and the one that bit on hardware: DHCP SUCCEEDS and ARP finds no gateway, so
        // `gw_known` is false on a stack holding a real lease. Read as "unconfigured", that discarded
        // the lease and re-ran the dance forever. See the auto-configure block below.
        //
        // So: while there is no lease and the link is up, re-run the dance - spaced by RESYNC_SECS so
        // the cost is one attempt a minute rather than one per request, and skipped entirely once a
        // lease is held (the common case pays one bool test).
        if badge.is_none()
            && !leased
            && gw_known
            && matches!(pl.first(), Some(&0) | Some(&1) | Some(&3) | Some(&6))
            && ctx.epoch_secs_monotonic() - last_redhcp_at >= RESYNC_SECS
            && link_is_up(&ctx, pending)
        {
            last_redhcp_at = ctx.epoch_secs_monotonic();
            ctx.log("net-stack: running on the fallback address without a lease - retrying DHCP");
            let d = run_dance(&ctx, pending, Some(&status));
            our_ip = d.our_ip; our_mac = d.our_mac; gw_mac = d.gw_mac; gw_known = d.gw_known; leased = d.leased; dns_server = d.dns_server; status = d.status;
            if leased {
                ctx.log_fmt(format_args!("net-stack: DHCP recovered - address {}.{}.{}.{}",
                                         our_ip[0], our_ip[1], our_ip[2], our_ip[3]));
            }
            synced_by_dance = true;   // run_dance ends in its own SNTP sync
        }
        // AUTO-CONFIGURE WHEN THERE IS NOTHING TO WORK WITH - which is NOT the same as "the gateway did
        // not answer", and conflating the two cost a working network.
        //
        // This was gated on `!gw_known` while that flag was called `have_mac`, and under that name it
        // read like "we do not know our own MAC, so we are unconfigured". It means the opposite kind of
        // thing: the GATEWAY's MAC, resolved by ARP, which is a fact about the network answering us and
        // not about whether this stack is configured.
        //
        // The consequence on hardware: DHCP granted 192.168.4.66 and ARP for the gateway then found
        // nothing, so `gw_known` went false, so this block declared the stack "unconfigured" and threw a
        // perfectly good lease away to re-run the whole dance - which blocked the serve loop for twelve
        // seconds, answered no requests while it ran (`No reply from 8.8.8.8: net-stack not responding`),
        // failed, degraded to the fallback address, and was set up to do it again forever. An
        // unreachable gateway is a reason to retry ARP; it is never a reason to discard an address the
        // server assigned.
        //
        // `leased` is the honest test and it already exists: it is true when DHCP granted this address
        // and false when we are guessing with the fallback. So re-dance only when we hold no lease - and
        // space it like the block above, because "no DHCP server on this link" is a steady state, and an
        // unspaced re-dance in a steady state is a blocking storm that starves every other request.
        if badge.is_none() && !leased && !gw_known
            && matches!(pl.first(), Some(&0) | Some(&1) | Some(&3) | Some(&6) | Some(&10))
            && ctx.epoch_secs_monotonic() - last_redhcp_at >= RESYNC_SECS
            && link_is_up(&ctx, pending)
        {
            last_redhcp_at = ctx.epoch_secs_monotonic();
            // No settle here. One was added on the theory that a hot-plugged PHY needed time to
            // negotiate before DHCP, and the measurement disproved it: the failure was ZERO frames
            // arriving, because nic-driver only programmed MAC speed and DMA burst during `bring_up`
            // (fixed in 27c719bd - it now re-applies on the link-up transition). The delay was solving
            // a problem that did not exist, so it only postponed every hot-plug configure.
            ctx.log("net-stack: link up while unconfigured - auto-configuring");
            let d = run_dance(&ctx, pending, Some(&status));
            our_ip = d.our_ip; our_mac = d.our_mac; gw_mac = d.gw_mac; gw_known = d.gw_known; leased = d.leased; dns_server = d.dns_server; status = d.status;
            synced_by_dance = true;   // run_dance ends in its own SNTP sync - op 10 must not repeat it
        }
        // RETRY THE GATEWAY ALONE WHEN WE HOLD A LEASE BUT ARP NEVER ANSWERED.
        //
        // This exists because of the block above. That block used to be the per-request ARP retry: it
        // was gated on the gateway being unknown, so an unresolved gateway re-ran the whole dance and
        // ARP came round again as a side effect. Correcting it to stop discarding a valid lease removed
        // that side effect, and with it the only thing that ever retried - a stack that leased an
        // address and missed the gateway would answer "Request timed out" forever without ever asking
        // again. Fixing one silent failure must not install another (26.7).
        //
        // So the retry is kept and narrowed to what actually needs retrying. It re-resolves the GATEWAY
        // and touches nothing else: the lease, the address and the DNS server are all still valid, and
        // re-running DHCP to recover an ARP entry was always the wrong instrument.
        //
        // Spaced by RESYNC_SECS because `arp_resolve` blocks this loop for its whole budget, and an
        // unreachable gateway is a steady state - retrying it per request would block every caller for
        // twelve seconds each, which is the starvation the log showed as `net-stack not responding`.
        if badge.is_none() && leased && !gw_known
            && matches!(pl.first(), Some(&0) | Some(&1) | Some(&3) | Some(&6) | Some(&10))
            && ctx.epoch_secs_monotonic() - last_gw_arp_at >= RESYNC_SECS
            && link_is_up(&ctx, pending)
        {
            last_gw_arp_at = ctx.epoch_secs_monotonic();
            let gateway = [status[4], status[5], status[6], status[7]];
            ctx.log("net-stack: leased but the gateway never answered ARP - retrying the gateway only");
            if let Some(m) = arp_resolve(&ctx, pending, &our_ip, &our_mac, &gateway, None) {
                gw_mac = m;
                gw_known = true;
                status[8..14].copy_from_slice(&gw_mac);
                status[14] |= 1;                       // bit 0 = gateway resolved
                ctx.log_fmt(format_args!(
                    "net-stack: gateway {}.{}.{}.{} resolved on retry - {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    gateway[0], gateway[1], gateway[2], gateway[3],
                    m[0], m[1], m[2], m[3], m[4], m[5]));
            }
        }
        if let Some((rid, right)) = badge {
            // ---- a TCP LISTENER capability ----
            if tcpst.listeners.iter().any(|l| l.rid == rid && l.port != 0) {
                // Accepting hands over authority, so it needs WRITE - the same bar as sending. A
                // read-only listener cap can be held and inspected but cannot take connections.
                let op = pl.first().copied().unwrap_or(LOP_ACCEPT);
                if op == LOP_CLOSE {
                    // Closing takes WRITE - the same bar as accepting, because both change what the
                    // machine does on the wire.
                    let closed = right & RIGHT_WRITE != 0 && tcpst.unlisten(rid);
                    if closed {
                        let _ = ctx.resource_revoke(rid);
                        ctx.log("net-stack: a listener was closed and its port released");
                    }
                    reply.send(&ctx, &[if closed { 1 } else { 0 }]);
                    reply.done(&ctx);
                    continue;
                }
                let mut granted = false;
                if op == LOP_ACCEPT && right & RIGHT_WRITE != 0 {
                    if let Some(i) = tcpst.pending_accept() {
                        // Mint the connection's own capability and hand it to the caller. From here
                        // the connection is a thing the client HOLDS: closing it is a revoke, and a
                        // stale handle gets `CapRevoked` from the kernel rather than a wrong answer
                        // from us (§7.10, exactly as `fs` does for a file).
                        if let Some((crid, cap)) = ctx.resource_mint(RIGHT_READ | RIGHT_WRITE | RIGHT_GRANT) {
                            tcpst.conns[i].rid = crid;
                            granted = ctx.derive_cap(cap)
                                .map(|c| reply.send_with_cap(&ctx, c, &[1]))
                                .unwrap_or(false);
                            ctx.remove_cap(cap);
                            if !granted {
                                // The cap did not reach the client, so neither did the success
                                // reply. Put the connection back to unclaimed rather than stranding
                                // it owned by nobody, and tell the caller (§26.7).
                                tcpst.conns[i].rid = 0;
                                let _ = ctx.resource_revoke(crid);
                            }
                        }
                    }
                }
                if !granted { reply.send(&ctx, &[0]); }
                reply.done(&ctx);
                continue;
            }

            // ---- a TCP CONNECTION capability ----
            if let Some(i) = (0..tcp::MAX_CONNS).find(|&k| {
                tcpst.conns[k].rid == rid && rid != 0 && tcpst.conns[k].state != tcp::State::Closed
            }) {
                let op = pl.first().copied().unwrap_or(COP_STAT);
                let body = if pl.len() > 1 { &pl[1..] } else { &[][..] };
                let mut resp = [0u8; 2048];
                let n = match op {
                    // Reading takes READ; sending takes WRITE. The kernel has already checked the
                    // cap carries `right`; this enforces that the OPERATION is within it, which is
                    // the `op <= right` check `fs` makes for files.
                    COP_RECV if right & RIGHT_READ != 0 => tcpst.read(rid, &mut resp),
                    COP_SEND if right & RIGHT_WRITE != 0 => {
                        let took = tcpst.write(rid, body);
                        // SHORT WRITES ARE REPORTED, not silently truncated. The send arena is
                        // fixed, and a client that offered more than fits has to know how much was
                        // taken or it will lose the tail without being told (§26.7).
                        resp[0] = (took & 0xff) as u8;
                        resp[1] = (took >> 8) as u8;
                        2
                    }
                    // ONE BYTE, NOT ZERO. An empty reply cannot be sent at all: the kernel's
                    // `validate_user_ptr` rejects `len == 0`, so `try_send` fails, the reply is
                    // discarded, and the caller waits out its whole deadline. Every other reply here
                    // happens to carry a byte; this one did not, and it cost five seconds per close
                    // on a Pi 4 - the close itself worked, which is why the connection was reaped
                    // 400 ms later while the client sat waiting.
                    //
                    // A status byte is the better answer anyway: the caller learns whether the close
                    // was accepted rather than inferring it from silence.
                    COP_CLOSE if right & RIGHT_WRITE != 0 => { tcpst.close(rid); resp[0] = 1; 1 }
                    COP_STAT => {
                        let c = &tcpst.conns[i];
                        let rd = c.readable().min(0xffff) as u16;
                        let un = c.unacked().min(0xffff) as u16;
                        resp[0] = c.state as u8;
                        resp[1..3].copy_from_slice(&rd.to_le_bytes());
                        resp[3..5].copy_from_slice(&un.to_le_bytes());
                        resp[5] = c.fault as u8;
                        6
                    }
                    // A refused operation answers `[0]` rather than nothing, for the same
                    // reason: an empty reply is undeliverable and reads as a hang.
                    _ => { resp[0] = 0; 1 }
                };
                reply.send(&ctx, &resp[..n]);
                reply.done(&ctx);
                continue;
            }

            // NOTHING WE OWN. A badged invocation reaching here matched no listener, no connection
            // and - below - possibly no socket either. That is either a capability whose resource we
            // have already released, or a client holding one we never minted, and both are worth
            // saying: a silent empty reply is indistinguishable from a successful no-op, which is
            // how a release that never happened looked like one that did (§26.7).
            if !sockets.iter().any(|sk| sk.rid == rid && sk.rid != 0) {
                ctx.log_fmt(format_args!(
                    "net-stack: a capability invocation named resource {} which is not a listener,                      a connection or a socket here - answering empty", rid));
            }

            // Socket-cap invocation - SOP_SEND: transmit a UDP datagram through this socket. Payload =
            // [dest_ip(4), dest_port(2), data...]. Reply = the response's UDP payload (empty on none).
            // Sending needs WRITE; the kernel already checked the cap holds `right`, we enforce op<=right.
            let mut resp = [0u8; 1500];
            let n = if right & RIGHT_WRITE != 0 && pl.len() >= 6 && gw_known {
                if let Some(s) = sockets.iter().find(|s| s.rid == rid && s.rid != 0) {
                    let dip = [pl[0], pl[1], pl[2], pl[3]];
                    let dport = ((pl[4] as u16) << 8) | pl[5] as u16;
                    udp_roundtrip(&ctx, pending, &gw_mac, &our_ip, &our_mac, s.port, &dip, dport, &pl[6..], &mut resp)
                } else { None }
            } else { None };
            match n {
                // A zero-length UDP response is as undeliverable as a refusal, for the same reason -
                // the kernel rejects a zero-length send - so both answer with a single zero byte.
                // `sock` already reads an empty payload as "nothing came back"; it now gets a reply
                // saying so instead of waiting out its deadline for one that could never arrive.
                Some(len) if len > 0 => { reply.send(&ctx, &resp[..len]); }
                _ => { reply.send(&ctx, &[0]); }
            }
        } else if pl.first() == Some(&2) {
            // OPEN a UDP socket: mint a delegated socket cap (READ|WRITE) and GRANT it to the client -
            // the fs `open_file` pattern (§7.10). Reply carries [1] + the embedded cap on success.
            let slot = sockets.iter().position(|s| s.rid == 0);
            let minted = slot.and_then(|sl| ctx.resource_mint(RIGHT_READ | RIGHT_WRITE | RIGHT_GRANT).map(|m| (sl, m)));
            match minted {
                Some((sl, (rid, cap))) => {
                    sockets[sl] = Socket { rid, port: 40000 + sl as u16 };
                    let granted = ctx.derive_cap(cap)
                        .map(|c| reply.send_with_cap(&ctx, c, &[1]))
                        .unwrap_or(false);
                    ctx.remove_cap(cap);        // net-stack drops its own copy; the client holds it now
                    if !granted {
                        sockets[sl].rid = 0;
                        let _ = ctx.resource_revoke(rid);
                        // The cap did not reach the client, so the success reply above didn't either.
                        // Tell the caller loudly (audit U10) instead of leaving it blocked on a reply
                        // that will never come (inv12 / VIII). A failed [0] send is fine - the caller's
                        // own reply-cap death wakes it as ReplyDead if net-stack itself then dies.
                        reply.send(&ctx, &[0]);
                    }
                }
                None => { reply.send(&ctx, &[0]); }
            }
        } else if pl.first() == Some(&22) {
            // TCP LISTEN (op 22): [22, port_hi, port_lo]. Mints a LISTENER capability and grants it
            // to the client. Reply carries [1] plus the embedded cap on success, [0] otherwise.
            //
            // The same shape as opening a UDP socket (op 2) and opening a file, because it is the
            // same mechanism: the service mints a delegated resource capability (§7.10), the kernel
            // badges every invocation with its ResourceId, and the holder's authority is the cap
            // rather than a number it was told. Closing the listener is a revoke.
            let ok = pl.len() >= 3 && gw_known;
            let port = if ok { ((pl[1] as u16) << 8) | pl[2] as u16 } else { 0 };
            let minted = if ok && port != 0 {
                ctx.resource_mint(RIGHT_READ | RIGHT_WRITE | RIGHT_GRANT)
            } else { None };
            let mut granted = false;
            if let Some((rid, cap)) = minted {
                if tcpst.listen(port, rid) {
                    granted = ctx.derive_cap(cap)
                        .map(|c| reply.send_with_cap(&ctx, c, &[1]))
                        .unwrap_or(false);
                    if !granted { tcpst.unlisten(rid); }
                }
                ctx.remove_cap(cap);
                if !granted { let _ = ctx.resource_revoke(rid); }
            }
            if granted {
                ctx.log_fmt(format_args!("net-stack: listening on TCP port {}", port));
            } else {
                // WHY it failed, not just that it did: an unconfigured stack, a port already taken
                // and a full listener table are three different things for the operator to fix.
                ctx.log_fmt(format_args!(
                    "net-stack: cannot listen on TCP port {} - {}", port,
                    if !gw_known { "the stack is not configured yet" }
                    else if port == 0 { "port 0 is not a port" }
                    else { "that port is already taken, or every listener slot is in use" }));
                reply.send(&ctx, &[0]);
            }
        } else if pl.first() == Some(&21) {
            // TCP TRANSACT (op 21): [21, ip(4), port_hi, port_lo, request bytes...].
            // Connect, send, read until the peer closes or the budget expires, close. Reply carries
            // whatever came back, or is EMPTY on failure - and the failure is logged with its reason
            // rather than folded into "nothing came back" (§26.7: a reported failure beats a
            // detected one).
            //
            // Driven inside the request, not from a background poll, because
            // `docs/net-tags-design.md` forbids unsolicited driver traffic until its phase 2/3 land.
            // That is the next step and it is recorded, not smuggled in here.
            // 3 KiB, not 1.4 KiB: a reply that fits in ONE segment never exercises the receive
            // path's reassembly, window updates or ACK-driven advancement. The Message ceiling is
            // 4 KiB (§8.5), so this leaves headroom while guaranteeing more than one segment.
            let mut resp = [0u8; 3072];
            let n = if pl.len() >= 7 && gw_known {
                let dip = [pl[1], pl[2], pl[3], pl[4]];
                let dport = ((pl[5] as u16) << 8) | pl[6] as u16;
                // LOG THE REQUEST AS RECEIVED, not as anyone believes they typed it. On the Pi 2 the
                // console interleaves driver status with keystrokes, so a garbled line reaches here
                // as a different address entirely - and without this the log shows an outcome for a
                // destination nobody can confirm. This is the only place that knows what was
                // actually asked for.
                ctx.log_fmt(format_args!(
                    "net-stack: tcp -> {}.{}.{}.{}:{}, {} byte request",
                    dip[0], dip[1], dip[2], dip[3], dport, pl.len() - 7));
                // WHO GOES ON THE WIRE? A host on our own subnet must be addressed DIRECTLY, not
                // through the router. Sending a neighbour's traffic to the gateway makes the path
                // asymmetric - it answers us direct, so the router sees only our half of the flow and
                // drops everything after the first packet as invalid.
                //
                // AN ARP REPLY IS THE TEST, and it needs no netmask: a host that answers is on-link
                // by definition, whatever the prefix happens to be. That matters because net-stack
                // does not keep the mask from the DHCP lease, and assuming /24 would be a guess that
                // breaks on anything else. No answer means it is not a neighbour, so the gateway is
                // right and we fall back to it.
                let peer_mac = match arp_resolve(&ctx, pending, &our_ip, &our_mac, &dip, None) {
                    Some(mac) => {
                        ctx.log_fmt(format_args!(
                            "net-stack: tcp {}.{}.{}.{} on-link at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} - direct",
                            dip[0], dip[1], dip[2], dip[3],
                            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]));
                        mac
                    }
                    None => {
                        ctx.log_fmt(format_args!(
                            "net-stack: tcp {}.{}.{}.{} did not answer ARP - routing via the gateway",
                            dip[0], dip[1], dip[2], dip[3]));
                        gw_mac
                    }
                };
                let net = tcp::Net { our_mac, peer_mac, our_ip };
                match tcp_transact(&ctx, pending, &mut tcpst, &net, dip, dport, &pl[7..], &mut resp, 8_000) {
                    // A SUCCESSFUL-BUT-EMPTY transaction used to log NOTHING, while the shell told
                    // the user to "see its log for the reason". A message that points at an absent
                    // explanation is worse than silence: it sends the reader looking for something
                    // that was never written (§26.7). `tcp_transact` now reports how far the
                    // connection actually got, so "nothing came back" is always attributable.
                    Ok(0) => {
                        ctx.log_fmt(format_args!(
                            "net-stack: tcp {}.{}.{}.{}:{} returned no data - reached state '{}' \
                             ({} retransmission(s)); no fault was recorded, so the budget expired",
                            dip[0], dip[1], dip[2], dip[3], dport,
                            tcpst.last_state.name(), tcpst.last_retx));
                        ctx.log_fmt(format_args!(
                            "net-stack: tcp frames - {} offered to the state machine, {} matched a \
                             connection, {} sent",
                            tcpst.stat_seen, tcpst.stat_matched, tcpst.stat_sent));
                        // The journal, in order. Upper case = the driver answered, lower case = it
                        // did not. `SSSS` with no `A` means no acknowledgement was ever built;
                        // `SSSSaaa` means three were built and none was taken.
                        let n = tcpst.tx_n.min(tcpst.tx_log.len());
                        if let Ok(j) = core::str::from_utf8(&tcpst.tx_log[..n]) {
                            ctx.log_fmt(format_args!(
                                "net-stack: tcp transmit journal - {} (S=syn A=ack F=fin D=data, \
                                 lower case = driver did not answer)", j));
                        }
                        0
                    }
                    Ok(got) => {
                        ctx.log_fmt(format_args!(
                            "net-stack: tcp {}.{}.{}.{}:{} ok - {} byte(s)",
                            dip[0], dip[1], dip[2], dip[3], dport, got));
                        // WHERE DID THE TIME GO? Printed on success as well as failure, because a
                        // transaction that returns the right bytes slowly is a different fault from
                        // one that returns nothing, and only the failure arm could say anything at
                        // all. Measured on a Pi 2: a 10-byte reply took ~346 ms where a 2884-byte
                        // reply took ~15 ms, reproducibly, which is backwards and unexplained.
                        //
                        // The state at exit and the transmit journal separate the two candidate
                        // explanations. A journal of a few frames ending in `F`, with the state
                        // short of `Closed`, means the time was spent WAITING for the peer (its
                        // delayed acknowledgement of our FIN, which it has no data to piggyback
                        // on). A journal with repeated frames means we were RETRANSMITTING, which
                        // is a different problem entirely.
                        let n = tcpst.tx_n.min(tcpst.tx_log.len());
                        if let Ok(j) = core::str::from_utf8(&tcpst.tx_log[..n]) {
                            ctx.log_fmt(format_args!(
                                "net-stack: tcp exit state {} after {} frame(s) - {} (S=syn A=ack F=fin D=data, lower case = the driver did not take it)",
                                tcpst.last_state.name(), n, j));
                        }
                        got
                    }
                    Err(f) => {
                        ctx.log_fmt(format_args!(
                            "net-stack: tcp {}.{}.{}.{}:{} failed - {}",
                            dip[0], dip[1], dip[2], dip[3], dport,
                            match f {
                                tcp::Fault::Reset => "peer reset the connection",
                                tcp::Fault::RetxExhausted => "no acknowledgement after 6 retransmissions",
                                tcp::Fault::ConnectTimeout => "no answer to our SYN",
                                tcp::Fault::None => "no data and no fault (budget expired)",
                            }));
                        0
                    }
                }
            } else {
                if !gw_known { ctx.log("net-stack: tcp asked for before the stack is configured"); }
                0
            };
            reply.send(&ctx, &resp[..n]);
        } else if pl.first() == Some(&1) {
            // DNS request (byte 0 = 1, then the hostname) - net-stack-internal resolution.
            // Try the DHCP-learned server, then a public fallback (8.8.8.8). A home router may do DHCP +
            // ICMP but NOT run a DNS forwarder on its LAN IP (the T630: 192.168.4.1 answered ping but was
            // silent on 53), so fall back to a public resolver reached through the gateway.
            let mut any_reply = false;
            let mut ip = None;
            let mut frames = 0u16;    // DIAGNOSTIC: non-empty frames collected across both servers
            let mut udp = 0u16;       //   ... how many were UDP
            let mut timeouts = 0u16;  //   ... how many nic-driver requests timed out (deadline vs poll)
            if gw_known {
                for server in [dns_server, [8, 8, 8, 8]] {
                    let mut got = false;
                    ip = dns_resolve(&ctx, pending, &pl[1..], &gw_mac, &our_ip, &our_mac, &server, &mut got,
                                     &mut frames, &mut udp, &mut timeouts);
                    any_reply |= got;
                    if ip.is_some() { break; }
                }
            }
            let mut rb = [0u8; 8];
            if let Some(a) = ip { rb[0] = 1; rb[1..5].copy_from_slice(&a); }
            else if any_reply { rb[0] = 2; }   // a server replied, but no A record
            rb[5] = frames.min(255) as u8;
            rb[6] = udp.min(255) as u8;
            rb[7] = timeouts.min(255) as u8;
            reply.send(&ctx, &rb);
        } else if pl.first() == Some(&3) && pl.len() >= 5 {
            // Ping an IP (byte 0 = 3, then 4 IP bytes, then an OPTIONAL le-u16 payload size): ICMP echo,
            // no DNS. Runs HERE in the serve loop, so `ping <gateway>` proves the post-boot request path
            // and `ping 8.8.8.8` probes the internet. Reply: [alive, rtt_ms(le u16), reply_ttl].
            let dip = [pl[1], pl[2], pl[3], pl[4]];
            let bytes = if pl.len() >= 7 { u16::from_le_bytes([pl[5], pl[6]]) as usize } else { 32 };
            // Check the link FIRST. With the cable out, an ICMP polls its FULL budget (~seconds) and the
            // ping looks FROZEN - one line every several seconds. A fast [2] "no link" reply keeps the
            // shell's ~1s cadence: it prints "no link" each second and RESUMES real replies the moment the
            // cable is back (the gateway MAC persists, so the ICMP just flows again). Byte 0: 1=reply,
            // 0=timeout (link up, no answer), 2=no link.
            let rb = if !link_is_up(&ctx, pending) {
                [2u8, 0, 0, 0]
            } else {
                let mut frames = 0u16;
                let mut timeouts = 0u16;
                ping_seq = ping_seq.wrapping_add(1);   // distinct per echo so a stale reply can't match
                if tsc_hz == 0 && tsc_cal_tries < TSC_CAL_TRIES {
                    tsc_cal_tries += 1;
                    tsc_hz = calibrate_tsc_hz(&ctx);
                    if tsc_hz == 0 && tsc_cal_tries == TSC_CAL_TRIES {
                        ctx.log("net-stack: TSC calibration failed 3 times - not retrying. RTT is \
                                 reported as 0 from here; ping itself is unaffected.");
                    }
                }
                match if gw_known { ping(&ctx, pending, &gw_mac, &our_ip, &our_mac, &dip, bytes, ping_seq, tsc_hz, &mut frames, &mut timeouts) } else { None } {
                    Some((rtt, ttl)) => { let r = rtt.to_le_bytes(); [1u8, r[0], r[1], ttl] }
                    // No reply: re-check the link. If it dropped DURING the poll it is "no link" (fast
                    // recovery to the 1s cadence), not a real "Request timed out" on a live link.
                    None => if link_is_up(&ctx, pending) { [0u8, 0, 0, 0] } else { [2u8, 0, 0, 0] },
                }
            };
            reply.send(&ctx, &rb);
        } else if pl.first() == Some(&6) && pl.len() >= 5 {
            // ARP (op 6, then 4 IP bytes): resolve one host's MAC. Reply [found, mac(6)]. `net arp` uses
            // it directly; `net scan` calls it across the subnet.
            let target = [pl[1], pl[2], pl[3], pl[4]];
            let rb = match arp_resolve(&ctx, pending, &our_ip, &our_mac, &target, None) {
                Some(m) => [1u8, m[0], m[1], m[2], m[3], m[4], m[5]],
                None    => [0u8; 7],
            };
            reply.send(&ctx, &rb);
        } else if pl.first() == Some(&8) {
            // RENEW (op 8): re-run the boot dance IN PLACE so a link that came up after boot - a cable
            // plugged in later - reconfigures the stack without a reboot. Nothing is special; the link
            // recovers like any restartable thing. Re-assign the mutable state, reply the FRESH status.
            ctx.log("net-stack: renew - re-running DHCP/ARP/ICMP");
            let d = run_dance(&ctx, pending, Some(&status));
            our_ip = d.our_ip;
            our_mac = d.our_mac;
            gw_mac = d.gw_mac;
            gw_known = d.gw_known;
            dns_server = d.dns_server;
            status = d.status;
            reply.send(&ctx, &status);
        } else if pl.first() == Some(&10) {
            // SYNC (op 10): re-fetch the time from the network (SNTP) and set the wall clock - the shell
            // `date sync`. Reply: [1, epoch(4 LE)] on success, [0] on failure (no NIC / server silent).
            // If the auto-configure above just ran the dance (which ends in its own sync), do NOT sync
            // again: that would put two full SNTP exchanges inside one request while every other client op
            // waits behind this single-threaded serve loop.
            let st = NetState { our_ip, our_mac, gw_mac, gw_known, leased, dns_server, status };
            match if synced_by_dance { clock_epoch_if_set(&ctx) } else { sntp_sync(&ctx, pending, &st) } {
                Some(unix) => {
                    let mut r = [0u8; 5];
                    r[0] = 1;
                    r[1..5].copy_from_slice(&unix.to_le_bytes());
                    ctx.log_fmt(format_args!("net-stack: SNTP - wall clock set (epoch {})", unix));
                    reply.send(&ctx, &r);
                }
                None => { reply.send(&ctx, &[0]); }
            }
        } else {
            // Status request (default): reply the CURRENT state, not just the frozen record. Read the link
            // and, if it is down (cable out), clear the "gateway resolved / ping OK" flags so `net` reflects
            // reality instead of stale boot-time info - as adaptable as `ping`. gw_known is NOT cleared (the
            // gateway MAC persists, so `net`/`ping` resume on replug without re-dancing).
            let mut s = status;
            if !link_is_up(&ctx, pending) { s[14] = 0; }
            reply.send(&ctx, &s);
        }
        reply.done(&ctx);
    }
}
