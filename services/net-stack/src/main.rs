// SPDX-License-Identifier: GPL-2.0-only
//! net-stack - the model-AGNOSTIC half of networking (docs/networking.md, Phase 2).
//!
//! nic-driver knows one NIC and speaks raw Ethernet frames; net-stack knows no hardware and speaks
//! ARP/IPv4/ICMP/UDP/TCP over those frames. The seam between them is the **frame interface**: a
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

use godspeed_sdk::{ServiceContext, Message, DeadlineOutcome};

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
/// a distinction the protocol already makes. A client request met during the clear is dropped with its
/// cap reclaimed (§8.5), which is `docs/net-tags-design.md` phase-2 behaviour: it times out and retries,
/// which is defined and recoverable, where consuming it corrupts both sides silently.
fn nic_status_req(ctx: &ServiceContext, msg: &Message, secs: i64) -> Option<Message> {
    while ctx.try_recv().is_some() {
        if let Some(cap) = ctx.take_pending_cap() {
            ctx.remove_cap(cap);
        }
    }
    nic_req(ctx, msg, secs)
}

fn nic_req(ctx: &ServiceContext, msg: &Message, secs: i64) -> Option<Message> {
    match ctx.request_with_reply_deadline_outcome("nic-driver", msg, secs) {
        DeadlineOutcome::Reply(r) => Some(r),
        // The send never left - nic-driver's cap is stale (it was killed + respawned). Reacquire it by
        // name from the kernel directory and retry once against the fresh instance.
        DeadlineOutcome::SendFailed if ctx.reacquire_by_name("nic-driver") =>
            ctx.request_with_reply_deadline("nic-driver", msg, secs),
        // A genuine timeout (the driver got it but the host was silent), or a reacquire that still
        // could not resolve the driver: return failure WITHOUT retrying - retrying a silent host would
        // just double every no-answer wait (net arp/dns to a host that does not answer).
        _ => None,
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
fn drain_scan_hit(ctx: &ServiceContext, secs: i64, mut on_frame: impl FnMut(&[u8]) -> bool) -> bool {
    let t0 = ctx.epoch_secs_monotonic();
    loop {
        if let Some(b) = nic_req(ctx, &Message::from_bytes(&[9u8]), LINK_SECS) {
            let p = b.payload_bytes();
            let n = if p.is_empty() { 0 } else { p[0] as usize };
            let mut pos = 1usize;
            for _ in 0..n {
                if pos + 2 > p.len() { break; }
                let fl = u16::from_le_bytes([p[pos], p[pos + 1]]) as usize;
                pos += 2;
                if pos + fl > p.len() { break; }
                if on_frame(&p[pos..pos + fl]) { return true; }
                pos += fl;
            }
        }
        if ctx.epoch_secs_monotonic() - t0 >= secs { return false; }
        ctx.sleep(ctx.duration_cycles(RX_POLL_PACE_MS));   // see RX_POLL_PACE_MS
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

fn drain_scan(ctx: &ServiceContext, secs: i64, mut on_frame: impl FnMut(&[u8]) -> bool) {
    let t0 = ctx.epoch_secs_monotonic();
    loop {
        if let Some(b) = nic_req(ctx, &Message::from_bytes(&[9u8]), LINK_SECS) {
            let p = b.payload_bytes();
            let n = if p.is_empty() { 0 } else { p[0] as usize };
            let mut pos = 1usize;
            for _ in 0..n {
                if pos + 2 > p.len() { break; }
                let fl = u16::from_le_bytes([p[pos], p[pos + 1]]) as usize;
                pos += 2;
                if pos + fl > p.len() { break; }
                if on_frame(&p[pos..pos + fl]) { return; }
                pos += fl;
            }
        }
        if ctx.epoch_secs_monotonic() - t0 >= secs { return; }
        // Wait before asking again - see RX_POLL_PACE_MS. `sleep` parks the task, so the core is free
        // for the driver that is trying to hand us the very frame we are waiting for.
        ctx.sleep(ctx.duration_cycles(RX_POLL_PACE_MS));
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
fn dhcp_request(ctx: &ServiceContext, our_mac: &[u8; 6], ip: &[u8; 4], srv: &[u8; 4], bcast: bool) -> bool {
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
    for _ in 0..DANCE_TRIES {
        let _ = nic_req(ctx, &req, LINK_SECS);
        let acked = drain_scan_hit(ctx, DANCE_SECS, |f| {
            // A BOOTREPLY carrying option 53 = 5 (DHCPACK) for the address we asked for. A NAK (6) is
            // a definite refusal and is treated as "not acknowledged" by simply not matching - the
            // caller re-DISCOVERs, which is what RFC 2131 asks of a NAKed client anyway.
            if f.len() >= 62 && f[12] == 0x08 && f[13] == 0x00 && f[14] == 0x45 && f[23] == 17
                && f[42] == 2 && f[58] == ip[0] && f[59] == ip[1] && f[60] == ip[2] && f[61] == ip[3]
            {
                // Walk the options, then decide OUTSIDE the loop.
                //
                // Written first as "set the flag and return from inside the walk", which reads more
                // directly and which the optimiser deleted outright: the ACK branch and its log never
                // reached the binary at all, so on hardware neither the ACK line nor the failure line
                // ever printed and the REQUEST looked as though it had not run. `dhcp_discover`
                // twenty lines down does the same job with the flag set AFTER its walk and survives,
                // so this matches the shape that is known to compile rather than the one that reads
                // best. Verified by grepping the built ELF for this format string, which is now part
                // of the build check for this file.
                let mut is_ack = false;
                let mut o = 282usize;
                while o + 1 < f.len() {
                    let opt = f[o];
                    if opt == 255 { break; }
                    if opt == 0 { o += 1; continue; }
                    let len = f[o + 1] as usize;
                    if opt == 53 && len >= 1 && o + 2 < f.len() && f[o + 2] == 5 { is_ack = true; }
                    o += 2 + len;
                }
                if is_ack {
                    return true;
                }
            }
            false
        });
        if acked {
            ctx.log_fmt(format_args!(
                "net-stack: DHCP - ACK, {}.{}.{}.{} is ours (server {}.{}.{}.{})",
                ip[0], ip[1], ip[2], ip[3], srv[0], srv[1], srv[2], srv[3]));
            return true;
        }
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
fn dhcp_lease(ctx: &ServiceContext, our_mac: &[u8; 6]) -> Option<([u8; 4], [u8; 4], [u8; 4])> {
    if let Some(cfg) = dhcp_discover(ctx, our_mac, false) {
        return Some(cfg);
    }
    ctx.log("net-stack: DHCP got no reply addressed to us - retrying and asking the server to broadcast");
    let cfg = dhcp_discover(ctx, our_mac, true)?;
    ctx.log("net-stack: DHCP succeeded ONLY with a broadcast reply - this port is not receiving frames              addressed to its own MAC, so ARP and ping cannot work until that is fixed");
    Some(cfg)
}

fn dhcp_discover(ctx: &ServiceContext, our_mac: &[u8; 6], bcast: bool) -> Option<([u8; 4], [u8; 4], [u8; 4])> {
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
        let _ = nic_req(ctx, &req, LINK_SECS);
        let mut found: Option<([u8; 4], [u8; 4], [u8; 4], [u8; 4])> = None;
        drain_scan(ctx, DANCE_SECS, |f| {
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
            if dhcp_request(ctx, our_mac, &ip, &srv, bcast) {
                return Some((ip, gw, dns));
            }
            // No ACK: the address is NOT ours, and using it anyway is what produced the silent
            // gateway. Fall through and re-DISCOVER rather than pretend.
            ctx.log("net-stack: DHCP - REQUEST not acknowledged; the address is not ours, retrying");
        }
        let _ = ctx.reacquire_by_name("nic-driver");   // best-effort: we retry either way
    }
    ctx.log("net-stack: DHCP - no offer within the budget - degrading to the fallback IP");
    None
}

/// Resolve a hostname to an IPv4 address via DNS (UDP to slirp's resolver at 10.0.2.3). Builds a
/// standard A-record query, sends it THROUGH nic-driver, and parses the first A answer. Returns the
/// IP, or None (no gateway, malformed name, or no answer - DNS depends on the host's resolver, which
/// slirp forwards to, so a failure here is a real "no answer", not a bug).
fn dns_resolve(ctx: &ServiceContext, hostname: &[u8], gw_mac: &[u8; 6], our_ip: &[u8; 4],
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
    let mut reply = nic_req(ctx, &req, DANCE_SECS);
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
        // Not our reply. If we owe an ARP reply (the gateway asked for us), send it - its request also
        // returns the next frame; otherwise collect the NEXT frame WITHOUT re-TX.
        reply = if answer_arp {
            ctx.request_with_reply_deadline("nic-driver", &Message::from_bytes(&arp_out), DANCE_SECS)
        } else {
            ctx.request_with_reply_deadline("nic-driver", &rx_only, DANCE_SECS)
        };
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

/// Send a UDP datagram (src_port -> dest_ip:dest_port carrying `data`) THROUGH nic-driver and copy the
/// response's UDP payload into `out`. Returns the payload length, or None (no gateway / no reply).
fn udp_roundtrip(ctx: &ServiceContext, gw_mac: &[u8; 6], our_ip: &[u8; 4], our_mac: &[u8; 6],
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
        let reply = match ctx.request_with_reply_deadline("nic-driver", &req, DANCE_SECS) {
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

fn sntp_sync(ctx: &ServiceContext, st: &NetState) -> Option<u32> {
    if !st.gw_known { return None; }                     // no gateway MAC - nothing to send through
    // Resolve an NTP server by name; fall back to the fixed anycast IP if DNS is down - but say so. A
    // recovery that hides the failure it recovered from is a silent fallback (§26.7): without this line an
    // operator cannot tell a resolved pool address from a broken resolver.
    let (mut gf, mut fr, mut ud, mut to) = (false, 0u16, 0u16, 0u16);
    let ntp_ip = match dns_resolve(ctx, b"pool.ntp.org", &st.gw_mac, &st.our_ip, &st.our_mac,
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
    for _ in 0..SNTP_TRIES {
        let _ = nic_req(ctx, &req, LINK_SECS);
        drain_scan(ctx, DANCE_SECS, |f| {
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
                let _ = nic_req(ctx, &Message::from_bytes(&arp_out), LINK_SECS);
            }
            false
        });
        if unix.is_some() { break; }
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
fn arp_resolve(ctx: &ServiceContext, our_ip: &[u8; 4], our_mac: &[u8; 6], target: &[u8; 4]) -> Option<[u8; 6]> {
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
    for _ in 0..DANCE_TRIES {
        let _ = nic_req(ctx, &req, LINK_SECS);
        let mut result: Option<[u8; 6]> = None;
        drain_scan(ctx, DANCE_SECS, |f| {
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
                result = Some(m);
                true
            } else {
                if build_arp_reply(f, our_ip, our_mac, &mut arp_out) {
                    let _ = nic_req(ctx, &Message::from_bytes(&arp_out), LINK_SECS);
                }
                false
            }
        });
        if let Some(m) = result { return Some(m); }
    }
    ctx.log_fmt(format_args!(
        "net-stack: ARP for {}.{}.{}.{} found nothing - {} frames scanned, {} to our MAC, {} ARP, \
         {} ARP replies (asking as {}.{}.{}.{} / {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x})",
        target[0], target[1], target[2], target[3],
        seen, unicast, arps, replies,
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
    let s0 = ctx.epoch_secs_monotonic();
    let mut n = 0u64;
    while ctx.epoch_secs_monotonic() == s0 { ctx.yield_cpu(); n += 1; if n > SPIN_MAX { return 0; } }
    let t0 = ctx.read_tsc();
    let s1 = ctx.epoch_secs_monotonic();
    n = 0;
    while ctx.epoch_secs_monotonic() == s1 { ctx.yield_cpu(); n += 1; if n > SPIN_MAX { return 0; } }
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
    let floor: u64 = if cfg!(any(target_arch = "arm", target_arch = "aarch64")) {
        500_000
    } else {
        100_000_000
    };
    if (floor..=10_000_000_000).contains(&hz) { hz } else { 0 }
}

/// Send one ICMP echo of `payload_len` data bytes to `dest_ip` and wait for the reply. Returns
/// `Some((rtt_us, reply_ttl))` on an echo reply, `None` on timeout. The round trip is timed with the TSC
/// and converted to microseconds via `tsc_hz` (RTC-calibrated; 0 -> reported as 0).
/// Sends ONCE (the reply arrives with it), then, if the first frame back was a stray broadcast, drains a
/// BATCH of frames in ONE bounded [9] round-trip and scans it - so a reply behind broadcasts on a busy
/// LAN is caught without N slow re-queries (which pushed net-stack past the shell's deadline).
fn ping(ctx: &ServiceContext, gw_mac: &[u8; 6], our_ip: &[u8; 4], our_mac: &[u8; 6], dest_ip: &[u8; 4],
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

    // 1. Send the echo; the frame that returns with it is the first candidate.
    match nic_req(ctx, &req, LINK_SECS) {
        Some(r) => {
            let f = r.payload_bytes();
            *frames += 1;
            if is_echo(f) { return Some((rtt_us(), f[22])); }
            if build_arp_reply(f, our_ip, our_mac, &mut arp_out) {   // gateway ARPing for us - answer it
                let _ = nic_req(ctx, &Message::from_bytes(&arp_out), LINK_SECS);
            }
        }
        None => *timeouts += 1,
    }

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
    let deadline_cycles = if tsc_hz > 0 { (tsc_hz * 9) / 10 } else { 0 };   // ~900 ms
    let mut drains: u32 = 0;
    loop {
        if let Some(b) = nic_req(ctx, &Message::from_bytes(&[9u8]), LINK_SECS) {
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
                if is_echo(f) { return Some((rtt_us(), f[22])); }
                if build_arp_reply(f, our_ip, our_mac, &mut arp_out) {
                    let _ = nic_req(ctx, &Message::from_bytes(&arp_out), LINK_SECS);
                }
            }
        }
        // Give up once the reply window closes (or immediately if the clock is uncalibrated - one drain).
        if deadline_cycles == 0 || ctx.read_tsc().wrapping_sub(t1) >= deadline_cycles {
            // SAY HOW THE WINDOW WAS SPENT. A timeout here is indistinguishable, from the outside, from
            // a network that dropped the packet - and the arithmetic says it is not that: consecutive
            // timeouts arrive 1.007 s apart when a 900 ms window plus the shell's 1 s pace should give
            // ~1.9 s, so the window is not lasting 900 ms. Guessing why has been wrong three times; this
            // prints the three numbers that settle it - how long we actually waited, how many times we
            // asked, and how many frames we saw while asking.
            let spent = ctx.read_tsc().wrapping_sub(t1);
            let us = if tsc_hz > 0 { spent.saturating_mul(1_000_000) / tsc_hz } else { 0 };
            ctx.log_fmt(format_args!(
                "net-stack: ping window closed after {} us ({} drains, {} frames seen, {} nic timeouts)                  [budget {} us, deadline {} cycles, tsc_hz {}]",
                us, drains, *frames, *timeouts,
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
fn learn_our_mac(ctx: &ServiceContext) -> Option<[u8; 6]> {
    let r = nic_status_req(ctx, &Message::from_bytes(&[3u8]), LINK_SECS)?;
    let p = r.payload_bytes();
    if p.len() < 7 { return None; }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&p[1..7]);
    if mac == [0u8; 6] { None } else { Some(mac) }
}

/// Run the DHCP -> ARP -> ICMP dance once and freeze the 19-byte status. Called at boot, and again by the
/// `net renew` op so a cable plugged in after boot (or a link that came up late) reconfigures the stack in
/// place. Bounded (DHCP/ARP each have their own budget) and loud on each degrade, like the boot path.
fn run_dance(ctx: &ServiceContext) -> NetState {
    // ---- Learn our MAC FIRST (audit U9 / Commandment III): every frame below advertises it as the eth
    // source. Without a NIC identity there is nothing to configure - degrade to unconfigured (same state
    // as no link); the auto-config-on-link path retries run_dance once the driver reports a MAC.
    let our_mac = match learn_our_mac(ctx) {
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
    let leased_cfg = dhcp_lease(ctx, &our_mac);
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
    let (gw_mac, gw_known) = match arp_resolve(ctx, &our_ip, &our_mac, &gateway) {
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
    let ping_ok = gw_known && ping(ctx, &gw_mac, &our_ip, &our_mac, &gateway, 32, 0, 0, &mut _pf, &mut _pt).is_some();
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
    } else if let Some(unix) = sntp_sync(ctx, &state) {
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

fn link_is_up(ctx: &ServiceContext) -> bool {
    match nic_status_req(ctx, &Message::from_bytes(&[3u8]), LINK_SECS) {
        Some(r) => { let p = r.payload_bytes(); if p.len() > 7 { p[7] != 0 } else { !p.is_empty() } }
        None    => false,
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
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
    ctx.log("net-stack: serving the client API (status/dns/socket)");

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
    let d = if link_is_up(&ctx) {
        run_dance(&ctx)
    } else {
        ctx.log("net-stack: no link at boot (cable unplugged?) - staying unconfigured and RESPONSIVE; will configure when the link comes up");
        // The unconfigured state, spelled out rather than defaulted: no IP, no gateway, no DNS. The
        // MAC is still learned - it is our hardware identity and true with or without a cable
        // (Commandment III / audit U9) - so `net` can report who we are while saying we are offline.
        NetState {
            our_ip: [0; 4],
            our_mac: learn_our_mac(&ctx).unwrap_or([0; 6]),
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
    let tsc_hz = calibrate_tsc_hz(&ctx);          // RTC-calibrated TSC Hz for RTT (kernel calib is 0 on T630)
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
    /// Latched once the wall clock is known. A clock never becomes unset, so this is asked at most once.
    let mut clock_known = false;
    loop {
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
        let req = ctx.recv();
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
                if !capless_logged {
                    capless_logged = true;
                    ctx.log("net-stack: request had no reply cap - dropping (cannot answer without one)");
                }
                continue;
            }
        };
        let pl = req.payload_bytes();
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
            } else if link_is_up(&ctx) {
                last_resync_at = ctx.epoch_secs_monotonic();
                let st = NetState { our_ip, our_mac, gw_mac, gw_known, leased, dns_server, status };
                if let Some(unix) = sntp_sync(&ctx, &st) {
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
            && link_is_up(&ctx)
        {
            last_redhcp_at = ctx.epoch_secs_monotonic();
            ctx.log("net-stack: running on the fallback address without a lease - retrying DHCP");
            let d = run_dance(&ctx);
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
            && link_is_up(&ctx)
        {
            last_redhcp_at = ctx.epoch_secs_monotonic();
            // No settle here. One was added on the theory that a hot-plugged PHY needed time to
            // negotiate before DHCP, and the measurement disproved it: the failure was ZERO frames
            // arriving, because nic-driver only programmed MAC speed and DMA burst during `bring_up`
            // (fixed in 27c719bd - it now re-applies on the link-up transition). The delay was solving
            // a problem that did not exist, so it only postponed every hot-plug configure.
            ctx.log("net-stack: link up while unconfigured - auto-configuring");
            let d = run_dance(&ctx);
            our_ip = d.our_ip; our_mac = d.our_mac; gw_mac = d.gw_mac; gw_known = d.gw_known; leased = d.leased; dns_server = d.dns_server; status = d.status;
            synced_by_dance = true;   // run_dance ends in its own SNTP sync - op 10 must not repeat it
        }
        if let Some((rid, right)) = badge {
            // Socket-cap invocation - SOP_SEND: transmit a UDP datagram through this socket. Payload =
            // [dest_ip(4), dest_port(2), data...]. Reply = the response's UDP payload (empty on none).
            // Sending needs WRITE; the kernel already checked the cap holds `right`, we enforce op<=right.
            let mut resp = [0u8; 1500];
            let n = if right & RIGHT_WRITE != 0 && pl.len() >= 6 && gw_known {
                if let Some(s) = sockets.iter().find(|s| s.rid == rid && s.rid != 0) {
                    let dip = [pl[0], pl[1], pl[2], pl[3]];
                    let dport = ((pl[4] as u16) << 8) | pl[5] as u16;
                    udp_roundtrip(&ctx, &gw_mac, &our_ip, &our_mac, s.port, &dip, dport, &pl[6..], &mut resp)
                } else { None }
            } else { None };
            match n {
                Some(len) => { let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&resp[..len])); }
                None      => { let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[])); }
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
                        .map(|c| ctx.send_with_cap_by_handle(reply_cap, c, &Message::from_bytes(&[1])).is_ok())
                        .unwrap_or(false);
                    ctx.remove_cap(cap);        // net-stack drops its own copy; the client holds it now
                    if !granted {
                        sockets[sl].rid = 0;
                        let _ = ctx.resource_revoke(rid);
                        // The cap did not reach the client, so the success reply above didn't either.
                        // Tell the caller loudly (audit U10) instead of leaving it blocked on a reply
                        // that will never come (inv12 / VIII). A failed [0] send is fine - the caller's
                        // own reply-cap death wakes it as ReplyDead if net-stack itself then dies.
                        let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[0]));
                    }
                }
                None => { let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[0])); }
            }
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
                    ip = dns_resolve(&ctx, &pl[1..], &gw_mac, &our_ip, &our_mac, &server, &mut got,
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
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rb));
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
            let rb = if !link_is_up(&ctx) {
                [2u8, 0, 0, 0]
            } else {
                let mut frames = 0u16;
                let mut timeouts = 0u16;
                ping_seq = ping_seq.wrapping_add(1);   // distinct per echo so a stale reply can't match
                match if gw_known { ping(&ctx, &gw_mac, &our_ip, &our_mac, &dip, bytes, ping_seq, tsc_hz, &mut frames, &mut timeouts) } else { None } {
                    Some((rtt, ttl)) => { let r = rtt.to_le_bytes(); [1u8, r[0], r[1], ttl] }
                    // No reply: re-check the link. If it dropped DURING the poll it is "no link" (fast
                    // recovery to the 1s cadence), not a real "Request timed out" on a live link.
                    None => if link_is_up(&ctx) { [0u8, 0, 0, 0] } else { [2u8, 0, 0, 0] },
                }
            };
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rb));
        } else if pl.first() == Some(&6) && pl.len() >= 5 {
            // ARP (op 6, then 4 IP bytes): resolve one host's MAC. Reply [found, mac(6)]. `net arp` uses
            // it directly; `net scan` calls it across the subnet.
            let target = [pl[1], pl[2], pl[3], pl[4]];
            let rb = match arp_resolve(&ctx, &our_ip, &our_mac, &target) {
                Some(m) => [1u8, m[0], m[1], m[2], m[3], m[4], m[5]],
                None    => [0u8; 7],
            };
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rb));
        } else if pl.first() == Some(&8) {
            // RENEW (op 8): re-run the boot dance IN PLACE so a link that came up after boot - a cable
            // plugged in later - reconfigures the stack without a reboot. Nothing is special; the link
            // recovers like any restartable thing. Re-assign the mutable state, reply the FRESH status.
            ctx.log("net-stack: renew - re-running DHCP/ARP/ICMP");
            let d = run_dance(&ctx);
            our_ip = d.our_ip;
            our_mac = d.our_mac;
            gw_mac = d.gw_mac;
            gw_known = d.gw_known;
            dns_server = d.dns_server;
            status = d.status;
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&status));
        } else if pl.first() == Some(&10) {
            // SYNC (op 10): re-fetch the time from the network (SNTP) and set the wall clock - the shell
            // `date sync`. Reply: [1, epoch(4 LE)] on success, [0] on failure (no NIC / server silent).
            // If the auto-configure above just ran the dance (which ends in its own sync), do NOT sync
            // again: that would put two full SNTP exchanges inside one request while every other client op
            // waits behind this single-threaded serve loop.
            let st = NetState { our_ip, our_mac, gw_mac, gw_known, leased, dns_server, status };
            match if synced_by_dance { clock_epoch_if_set(&ctx) } else { sntp_sync(&ctx, &st) } {
                Some(unix) => {
                    let mut r = [0u8; 5];
                    r[0] = 1;
                    r[1..5].copy_from_slice(&unix.to_le_bytes());
                    ctx.log_fmt(format_args!("net-stack: SNTP - wall clock set (epoch {})", unix));
                    let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&r));
                }
                None => { let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[0])); }
            }
        } else {
            // Status request (default): reply the CURRENT state, not just the frozen record. Read the link
            // and, if it is down (cable out), clear the "gateway resolved / ping OK" flags so `net` reflects
            // reality instead of stale boot-time info - as adaptable as `ping`. gw_known is NOT cleared (the
            // gateway MAC persists, so `net`/`ping` resume on replug without re-dancing).
            let mut s = status;
            if !link_is_up(&ctx) { s[14] = 0; }
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&s));
        }
        ctx.remove_cap(reply_cap);
    }
}
