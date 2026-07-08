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

use godspeed_sdk::{ServiceContext, Message};

// The NIC's MAC. In QEMU the e1000 default is 52:54:00:12:34:56; a real net-stack learns this from
// nic-driver at init (a small refinement), but nic-driver runs the NIC promiscuous, so a reply is
// received whatever sender MAC we advertise - this keeps the focus on the protocols.
const OUR_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
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
// A few tries per step: on a LIVE network the first frame back can be a background broadcast, so a step
// retries past stray frames (each retry is fast - a frame is already waiting) to find its real reply.
const DANCE_TRIES: u32 = 6;
// DNS collects frames after ONE query TX (the [4] RX-only path): up to this many frames pulled without
// re-transmitting, so a reply behind stray broadcasts is caught (a re-TX would drain+discard it).
const DNS_RX_TRIES: u32 = 12;
/// ICMP echo RX budget (loop iterations AFTER the initial send-and-wait). Kept tight so an interactive
/// `ping` pauses as little as possible on a lost echo: a live reply lands in the initial wait or the first
/// extra try, so the total no-reply wait is ~1.5s (initial + 1), not ~2.25s (initial + 2) or ~9s. The link
/// is checked BEFORE the ICMP and AGAIN on a timeout, so a mid-poll drop reports "no link", not a stalled
/// "Request timed out", and recovers to the ~1s cadence immediately.
const PING_RX_TRIES: u32 = 1;
/// Max ICMP echo DATA bytes `ping` will send (the Windows default is 32). Bounds the frame buffer.
const PING_MAX_PAYLOAD: usize = 1024;

/// Phase 3: a DHCP DISCOVER over UDP - ask QEMU slirp's built-in DHCP server for our IP and read the
/// OFFER. This proves the UDP transport (the layer the socket capability sits on) over the frame
/// interface. Returns the offered IP, or None (no NIC / nothing answered). A real net-stack would use
/// this to LEARN its own IP instead of hardcoding it; here it demonstrates the round-trip.
fn dhcp_discover(ctx: &ServiceContext) -> Option<([u8; 4], [u8; 4], [u8; 4])> {
    // Ethernet(14) + IPv4(20) + UDP(8) + DHCP/BOOTP(244) = 286 bytes.
    let mut frame = [0u8; 286];
    for b in frame[0..6].iter_mut() { *b = 0xff; }       // eth dest = broadcast
    frame[6..12].copy_from_slice(&OUR_MAC);              // eth src
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
    frame[52] = 0x80;                                    // flags = broadcast (OFFER comes back broadcast)
    frame[70..76].copy_from_slice(&OUR_MAC);             // chaddr (client hardware address)
    frame[278] = 0x63; frame[279] = 0x82; frame[280] = 0x53; frame[281] = 0x63; // DHCP magic cookie
    frame[282] = 53; frame[283] = 1; frame[284] = 1;     // option 53 (message type) = DISCOVER
    frame[285] = 255;                                    // option end

    let req = Message::from_bytes(&frame);
    for _ in 0..DANCE_TRIES {
        match ctx.request_with_reply_deadline("nic-driver", &req, DANCE_SECS) {
            Some(reply) => {
                let f = reply.payload_bytes();
                // A DHCP reply: IPv4 (0x0800, IHL 5), UDP (proto 17), BOOTP op = 2 (BOOTREPLY). yiaddr
                // (our offered IP) sits at BOOTP offset 16 = frame offset 58.
                if f.len() >= 62 && f[12] == 0x08 && f[13] == 0x00 && f[14] == 0x45
                    && f[23] == 17 && f[42] == 2 {
                    let ip = [f[58], f[59], f[60], f[61]];
                    // Learn the GATEWAY from the offer's options (magic cookie at frame offset 278 ->
                    // options at 282), option 3 = router. This is what makes it work on a REAL network
                    // (the gateway is 192.168.x.1, not QEMU's 10.0.2.2). Fall back to <subnet>.1.
                    let mut gw = [ip[0], ip[1], ip[2], 1];
                    let mut dns = [0u8; 4];
                    let mut have_dns = false;
                    let mut o = 282usize;
                    while o + 1 < f.len() {
                        let opt = f[o];
                        if opt == 255 { break; }          // options end
                        if opt == 0 { o += 1; continue; } // pad
                        let len = f[o + 1] as usize;
                        if opt == 3 && len >= 4 && o + 6 <= f.len() {           // router = gateway
                            gw = [f[o + 2], f[o + 3], f[o + 4], f[o + 5]];
                        }
                        if opt == 6 && len >= 4 && o + 6 <= f.len() {           // domain name server
                            dns = [f[o + 2], f[o + 3], f[o + 4], f[o + 5]];
                            have_dns = true;
                        }
                        o += 2 + len;
                    }
                    if !have_dns { dns = gw; }            // no DNS option: the gateway usually forwards DNS
                    ctx.log_fmt(format_args!(
                        "net-stack: DHCP - offered {}.{}.{}.{}, gw {}.{}.{}.{}, dns {}.{}.{}.{}",
                        ip[0], ip[1], ip[2], ip[3], gw[0], gw[1], gw[2], gw[3], dns[0], dns[1], dns[2], dns[3]));
                    return Some((ip, gw, dns));
                }
                // A frame came back but not our offer (a background broadcast on a live network) - retry
                // within the budget rather than giving up on the first stray frame.
            }
            None => {
                // No reply within the deadline: nic-driver still spawning, or not answering frames.
                ctx.reacquire_by_name("nic-driver");
            }
        }
    }
    ctx.log("net-stack: DHCP - no offer within the budget - degrading to the fallback IP");
    None
}

/// Resolve a hostname to an IPv4 address via DNS (UDP to slirp's resolver at 10.0.2.3). Builds a
/// standard A-record query, sends it THROUGH nic-driver, and parses the first A answer. Returns the
/// IP, or None (no gateway, malformed name, or no answer - DNS depends on the host's resolver, which
/// slirp forwards to, so a failure here is a real "no answer", not a bug).
fn dns_resolve(ctx: &ServiceContext, hostname: &[u8], gw_mac: &[u8; 6], our_ip: &[u8; 4],
               dns_server: &[u8; 4], got_reply: &mut bool,
               frames: &mut u16, udp: &mut u16, timeouts: &mut u16) -> Option<[u8; 4]> {
    // frames/udp/timeouts accumulate a DIAGNOSTIC: non-empty frames collected, how many were UDP, and how
    // many nic-driver requests TIMED OUT (net-stack's deadline fired before nic-driver replied). Timeouts
    // dominating => the deadline is too short (a timing bug); empties dominating => the receiver is dead.
    *got_reply = false;   // set true once a matching DNS reply arrives - lets the caller tell
                          // "server did not reply" from "server replied but had no A record".
    let mut frame = [0u8; 512];
    // Ethernet: to the gateway; slirp routes the datagram to its DNS at 10.0.2.3.
    frame[0..6].copy_from_slice(gw_mac);
    frame[6..12].copy_from_slice(&OUR_MAC);
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
    let mut reply = ctx.request_with_reply_deadline("nic-driver", &req, DANCE_SECS);
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
            let a = !m && build_arp_reply(f, our_ip, &mut arp_out);
            (m, a)
        };
        if matched {
            *got_reply = true;   // a matching DNS reply arrived (whatever it contains)
            let f = reply.as_ref().unwrap().payload_bytes();
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
fn udp_roundtrip(ctx: &ServiceContext, gw_mac: &[u8; 6], our_ip: &[u8; 4], src_port: u16,
                 dest_ip: &[u8; 4], dest_port: u16, data: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut frame = [0u8; 1600];
    let dlen = data.len().min(frame.len() - 42);
    frame[0..6].copy_from_slice(gw_mac);
    frame[6..12].copy_from_slice(&OUR_MAC);
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
            None => { ctx.reacquire_by_name("nic-driver"); continue; }
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

/// Send an ICMP echo request to `dest_ip` (via the gateway's MAC) and return true if the matching echo
/// REPLY comes back. Used to probe the gateway (LAN) and a public IP (internet reachability through NAT).
/// If `f` is an inbound ARP REQUEST for `our_ip`, build the matching ARP REPLY into `out` and return
/// true. net-stack MUST answer these: once the gateway's ARP entry for us (the OUR_MAC we advertise)
/// ages out it re-ARPs before it can address our UNICAST replies - stay silent and it only ever reaches
/// us with broadcasts, so the echo/DNS reply never arrives (exactly the T630 serve-loop symptom: 20
/// frames collected, all broadcast, no reply). This fires ONLY when someone is actively asking for us,
/// so on QEMU (slirp already learned us from our own query) it emits nothing - which is why it is safe
/// where a blind gratuitous ARP before every query was not.
fn build_arp_reply(f: &[u8], our_ip: &[u8; 4], out: &mut [u8; 42]) -> bool {
    if f.len() < 42 { return false; }
    if f[12] != 0x08 || f[13] != 0x06 { return false; }              // not ARP
    if f[20] != 0x00 || f[21] != 0x01 { return false; }              // not a REQUEST (oper 1)
    if f[38] != our_ip[0] || f[39] != our_ip[1]
        || f[40] != our_ip[2] || f[41] != our_ip[3] { return false; } // not asking for us
    for b in out.iter_mut() { *b = 0; }
    out[0..6].copy_from_slice(&f[22..28]);   // eth dst = the asker (its sender MAC)
    out[6..12].copy_from_slice(&OUR_MAC);    // eth src = us
    out[12] = 0x08; out[13] = 0x06;          // ethertype = ARP
    out[14] = 0x00; out[15] = 0x01;          // htype = Ethernet
    out[16] = 0x08; out[17] = 0x00;          // ptype = IPv4
    out[18] = 0x06; out[19] = 0x04;          // hlen 6, plen 4
    out[20] = 0x00; out[21] = 0x02;          // oper = reply
    out[22..28].copy_from_slice(&OUR_MAC);   // sender hw = us
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
fn arp_resolve(ctx: &ServiceContext, our_ip: &[u8; 4], target: &[u8; 4]) -> Option<[u8; 6]> {
    let mut arp = [0u8; 42];
    for b in arp.iter_mut().take(6) { *b = 0xff; }   // eth dst = broadcast
    arp[6..12].copy_from_slice(&OUR_MAC);
    arp[12] = 0x08; arp[13] = 0x06;                  // ARP
    arp[14] = 0x00; arp[15] = 0x01;                  // htype Ethernet
    arp[16] = 0x08; arp[17] = 0x00;                  // ptype IPv4
    arp[18] = 0x06; arp[19] = 0x04;                  // hlen 6, plen 4
    arp[20] = 0x00; arp[21] = 0x01;                  // oper = request
    arp[22..28].copy_from_slice(&OUR_MAC);
    arp[28..32].copy_from_slice(our_ip);
    arp[38..42].copy_from_slice(target);             // target ip = who we ask for
    let req     = Message::from_bytes(&arp);
    let rx_only = Message::from_bytes(&[4u8]);
    let mut arp_out = [0u8; 42];
    let mut reply = ctx.request_with_reply_deadline("nic-driver", &req, DANCE_SECS);
    for _ in 0..DNS_RX_TRIES {
        let (mac, answer_arp) = {
            let f: &[u8] = match &reply { Some(r) => r.payload_bytes(), None => &[] };
            // An ARP REPLY (oper 2) whose SENDER IP is the target we asked for (not some other host's).
            let hit = f.len() >= 42 && f[12] == 0x08 && f[13] == 0x06 && f[20] == 0x00 && f[21] == 0x02
                && f[28] == target[0] && f[29] == target[1] && f[30] == target[2] && f[31] == target[3];
            let mac = if hit { let mut m = [0u8; 6]; m.copy_from_slice(&f[22..28]); Some(m) } else { None };
            let a = mac.is_none() && build_arp_reply(f, our_ip, &mut arp_out);
            (mac, a)
        };
        if let Some(m) = mac { return Some(m); }
        reply = if answer_arp {
            ctx.request_with_reply_deadline("nic-driver", &Message::from_bytes(&arp_out), DANCE_SECS)
        } else {
            ctx.request_with_reply_deadline("nic-driver", &rx_only, DANCE_SECS)
        };
    }
    None
}

/// Calibrate the TSC frequency (Hz) against the RTC - the portable ground truth. The kernel's CPUID/PIT
/// calibration yields 0 on the AMD T630 (CPUID has no usable leaf; the PIT channel-2 output bit misbehaves),
/// but the RTC and `read_tsc` both work, so measure directly: align to a wall-clock second boundary, sample
/// the TSC, wait one more second, sample again - the delta is one second of TSC. Uses the DEGLITCHED epoch
/// so a CMOS misread cannot shorten the window; returns 0 (RTT then shows 0) if the result is implausible.
fn calibrate_tsc_hz(ctx: &ServiceContext) -> u64 {
    let s0 = ctx.epoch_secs_monotonic();
    while ctx.epoch_secs_monotonic() == s0 { ctx.yield_cpu(); }   // align to a second boundary
    let t0 = ctx.read_tsc();
    let s1 = ctx.epoch_secs_monotonic();
    while ctx.epoch_secs_monotonic() == s1 { ctx.yield_cpu(); }   // exactly one second later
    let hz = ctx.read_tsc().wrapping_sub(t0);
    if (100_000_000..=10_000_000_000).contains(&hz) { hz } else { 0 }   // 100 MHz .. 10 GHz is sane
}

/// Send one ICMP echo of `payload_len` data bytes to `dest_ip` and wait for the reply. Returns
/// `Some((rtt_us, reply_ttl))` on an echo reply, `None` on timeout. The round trip is timed with the TSC
/// and converted to microseconds via `tsc_hz` (RTC-calibrated; 0 -> reported as 0).
/// Sends ONCE then RX-only polls ([4]) so a reply behind stray broadcasts is caught without re-TX.
fn ping(ctx: &ServiceContext, gw_mac: &[u8; 6], our_ip: &[u8; 4], dest_ip: &[u8; 4],
        payload_len: usize, seq: u16, tsc_hz: u64, frames: &mut u16, timeouts: &mut u16) -> Option<(u16, u8)> {
    let plen = payload_len.min(PING_MAX_PAYLOAD);
    let flen = 42 + plen;
    let mut frame = [0u8; 42 + PING_MAX_PAYLOAD];
    frame[0..6].copy_from_slice(gw_mac);
    frame[6..12].copy_from_slice(&OUR_MAC);
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
    let req     = Message::from_bytes(&frame[..flen]);
    let rx_only = Message::from_bytes(&[4u8]);
    let mut arp_out = [0u8; 42];
    let mut reply = ctx.request_with_reply_deadline("nic-driver", &req, DANCE_SECS);
    for _ in 0..PING_RX_TRIES {
        let (matched, ttl, answer_arp) = {
            let f: &[u8] = match &reply { Some(r) => r.payload_bytes(), None => { *timeouts += 1; &[] } };
            if !f.is_empty() { *frames += 1; }
            // Echo REPLY (type 0) from dest_ip. Match the source so a gateway ping and an internet ping
            // cannot be confused, and skip stray frames.
            let m = f.len() >= 42 && f[12] == 0x08 && f[13] == 0x00 && f[14] == 0x45
                && f[23] == 1 && f[34] == 0
                && f[26] == dest_ip[0] && f[27] == dest_ip[1] && f[28] == dest_ip[2] && f[29] == dest_ip[3]
                && f[40] == (seq >> 8) as u8 && f[41] == seq as u8;  // reply must echo THIS ping's seq
            let ttl = if m { f[22] } else { 0 };     // the reply's TTL (the pinged host's)
            // Otherwise: is this the gateway ARPing for US? Answer it so it can address the echo reply.
            let a = !m && build_arp_reply(f, our_ip, &mut arp_out);
            (m, ttl, a)
        };
        if matched {
            let dt = ctx.read_tsc().wrapping_sub(t1);
            // MICROSECONDS: dt cycles * 10000 / cycles-per-10ms. Finer than ms so a sub-millisecond LAN RTT
            // is visible and distinguishable from a WAN one; capped at 65 ms (u16).
            // us = cycles * 1e6 / tsc_hz. tsc_hz is calibrated against the RTC (the kernel's CPUID/PIT
            // calibration yields 0 on the AMD T630), so this holds when q16 does not.
            let rtt_us = if tsc_hz > 0 { (dt.saturating_mul(1_000_000) / tsc_hz).min(65535) as u16 } else { 0 };
            return Some((rtt_us, ttl));
        }
        // Owe an ARP reply? Send it (its request also returns the next frame). Else just poll RX-only.
        reply = if answer_arp {
            ctx.request_with_reply_deadline("nic-driver", &Message::from_bytes(&arp_out), DANCE_SECS)
        } else {
            ctx.request_with_reply_deadline("nic-driver", &rx_only, DANCE_SECS)
        };
    }
    None
}

/// What one run of the boot dance (DHCP -> ARP -> ICMP) learns: our IP, the gateway's MAC, whether ARP
/// resolved it, the DNS server, and the frozen 19-byte status record served to clients. Produced by
/// [`run_dance`] at boot AND re-produced on `net renew` (op 8), so a link that comes up AFTER boot is
/// recovered without a reboot - nothing is special; the LINK recovers like any restartable thing.
struct NetState {
    our_ip: [u8; 4],
    gw_mac: [u8; 6],
    have_mac: bool,
    dns_server: [u8; 4],
    status: [u8; 19],
}

/// Run the DHCP -> ARP -> ICMP dance once and freeze the 19-byte status. Called at boot, and again by the
/// `net renew` op so a cable plugged in after boot (or a link that came up late) reconfigures the stack in
/// place. Bounded (DHCP/ARP each have their own budget) and loud on each degrade, like the boot path.
fn run_dance(ctx: &ServiceContext) -> NetState {
    // ---- Phase 3: DHCP FIRST, so net-stack LEARNS its own IP (self-configuring). Falls back to a default
    // only if there is no NIC / no offer (nic-driver serves empty replies). The IP it returns is the one
    // ARP + ICMP use below.
    let (our_ip, gateway, dns_server) = dhcp_discover(ctx).unwrap_or((FALLBACK_IP, GATEWAY_IP, GATEWAY_IP));

    // ---- Phase 2 step 1: ARP - who-has GATEWAY_IP, tell our_ip (a broadcast request).
    let mut arp = [0u8; 42];
    for b in arp.iter_mut().take(6) { *b = 0xff; }   // eth dest = broadcast
    arp[6..12].copy_from_slice(&OUR_MAC);            // eth src
    arp[12] = 0x08; arp[13] = 0x06;                  // ethertype = ARP
    arp[14] = 0x00; arp[15] = 0x01;                  // htype = Ethernet
    arp[16] = 0x08; arp[17] = 0x00;                  // ptype = IPv4
    arp[18] = 0x06; arp[19] = 0x04;                  // hlen 6, plen 4
    arp[20] = 0x00; arp[21] = 0x01;                  // oper = request
    arp[22..28].copy_from_slice(&OUR_MAC);           // sender hw
    arp[28..32].copy_from_slice(&our_ip);           // sender ip (learned via DHCP)
    arp[38..42].copy_from_slice(&gateway);           // target ip = DHCP-learned gateway (0 hw = the question)

    // Send it THROUGH nic-driver's frame interface, waiting on the TRUTH of the reply (Commandment
    // VIII): request_with_reply is a synchronous Call, so a dead/absent nic-driver wakes us with None
    // (ReplyDead) rather than hanging - we reacquire by name and retry (Commandment IX).
    let arp_req = Message::from_bytes(&arp);
    let mut gw_mac = [0u8; 6];
    let mut have_mac = false;
    for _ in 0..DANCE_TRIES {
        match ctx.request_with_reply_deadline("nic-driver", &arp_req, DANCE_SECS) {
            Some(reply) => {
                let f = reply.payload_bytes();
                // An ARP REPLY (oper = 2). On a live network the first frame back may be a background
                // broadcast; keep trying (skip it) rather than giving up on one stray frame.
                if f.len() >= 42 && f[12] == 0x08 && f[13] == 0x06 && f[20] == 0x00 && f[21] == 0x02 {
                    gw_mac.copy_from_slice(&f[22..28]);
                    have_mac = true;
                    ctx.log_fmt(format_args!(
                        "net-stack: ARP - {}.{}.{}.{} is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        gateway[0], gateway[1], gateway[2], gateway[3],
                        gw_mac[0], gw_mac[1], gw_mac[2], gw_mac[3], gw_mac[4], gw_mac[5]));
                    break;
                }
            }
            None => { ctx.reacquire_by_name("nic-driver"); }
        }
    }
    if !have_mac {
        ctx.log("net-stack: ARP - no reply for the gateway within the budget - degrading");
    }

    // ---- Phase 2 step 2: ICMP - ping the gateway to confirm it answers. Only once ARP gave us its MAC.
    let (mut _pf, mut _pt) = (0u16, 0u16);
    let ping_ok = have_mac && ping(ctx, &gw_mac, &our_ip, &gateway, 32, 0, 0, &mut _pf, &mut _pt).is_some();
    if ping_ok {
        ctx.log_fmt(format_args!("net-stack: ICMP - {}.{}.{}.{} echo reply (ping OK)",
            gateway[0], gateway[1], gateway[2], gateway[3]));
    } else if have_mac {
        ctx.log("net-stack: ICMP - no echo reply from the gateway");
    }

    // Freeze the result: our IP (4), the gateway IP (4), the gateway MAC (6), a flags byte (bit0 = gateway
    // resolved, bit1 = ping OK), and the DHCP-learned DNS server (4). The client formats it; we report raw
    // facts (utilities/0_conventions.md rule 7).
    let mut status = [0u8; 19];
    status[0..4].copy_from_slice(&our_ip);
    status[4..8].copy_from_slice(&gateway);
    status[8..14].copy_from_slice(&gw_mac);
    status[14] = (have_mac as u8) | ((ping_ok as u8) << 1);
    status[15..19].copy_from_slice(&dns_server);
    NetState { our_ip, gw_mac, have_mac, dns_server, status }
}

/// Read the NIC link state from nic-driver's `[3]` status. RTL8168: byte 7 = link up. On the QEMU e1000
/// path the reply is short (no link byte) - a non-empty reply means "up" (slirp's virtual link is always
/// up). Cheap; lets net-stack notice a cable plugged in after boot and self-configure without `net renew`.
fn link_is_up(ctx: &ServiceContext) -> bool {
    match ctx.request_with_reply_deadline("nic-driver", &Message::from_bytes(&[3u8]), DANCE_SECS) {
        Some(r) => { let p = r.payload_bytes(); if p.len() > 7 { p[7] != 0 } else { !p.is_empty() } }
        None    => false,
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("net-stack: starting");

    // Configure the stack (DHCP -> ARP -> ICMP). These are `mut` because `net renew` (op 8) re-runs the
    // dance in place - a link that comes up after boot recovers without a reboot.
    let d = run_dance(&ctx);
    let mut our_ip = d.our_ip;
    let mut gw_mac = d.gw_mac;
    let mut have_mac = d.have_mac;
    let mut dns_server = d.dns_server;
    let mut status = d.status;
    let mut sockets = [Socket { rid: 0, port: 0 }; MAX_SOCKETS];
    let mut ping_seq: u16 = 0;                    // unique ICMP seq per ping - see ping() (RTT accuracy)
    let tsc_hz = calibrate_tsc_hz(&ctx);          // RTC-calibrated TSC Hz for RTT (kernel calib is 0 on T630)
    ctx.log("net-stack: serving the client API (status/dns/socket)");
    loop {
        let req = ctx.recv();                   // block for a client request
        // A nonzero badge = a SOCKET-CAPABILITY invocation the kernel validated (§7.10). A plain
        // name-addressed request (status / DNS / open-socket) carries no badge.
        let badge = ctx.last_recv_badge();
        let reply_cap = match ctx.take_pending_cap() {
            Some(c) => c,
            None => continue,                   // a request with no reply cap - drop it
        };
        let pl = req.payload_bytes();
        // AUTO-CONFIGURE: while UNCONFIGURED (no gateway - booted with no cable, or a boot dance that met a
        // dead link), a `net` (op 0) or `ping` (op 3) request first checks the NIC link; if it has come up
        // (cable plugged in), re-run the dance IN PLACE so the network self-configures - no `net renew`.
        // Gated on !have_mac so a configured stack pays nothing, and retried per request so the PHY's
        // few-second post-cable auto-negotiation eventually catches. Once configured the gateway MAC
        // persists, so a later unplug/replug just resumes (the ICMP flows again) without re-dancing.
        if badge.is_none() && !have_mac && matches!(pl.first(), Some(&0) | Some(&3)) && link_is_up(&ctx) {
            ctx.log("net-stack: link up while unconfigured - auto-configuring");
            let d = run_dance(&ctx);
            our_ip = d.our_ip; gw_mac = d.gw_mac; have_mac = d.have_mac; dns_server = d.dns_server; status = d.status;
        }
        if let Some((rid, right)) = badge {
            // Socket-cap invocation - SOP_SEND: transmit a UDP datagram through this socket. Payload =
            // [dest_ip(4), dest_port(2), data...]. Reply = the response's UDP payload (empty on none).
            // Sending needs WRITE; the kernel already checked the cap holds `right`, we enforce op<=right.
            let mut resp = [0u8; 1500];
            let n = if right & RIGHT_WRITE != 0 && pl.len() >= 6 && have_mac {
                if let Some(s) = sockets.iter().find(|s| s.rid == rid && s.rid != 0) {
                    let dip = [pl[0], pl[1], pl[2], pl[3]];
                    let dport = ((pl[4] as u16) << 8) | pl[5] as u16;
                    udp_roundtrip(&ctx, &gw_mac, &our_ip, s.port, &dip, dport, &pl[6..], &mut resp)
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
                    if !granted { sockets[sl].rid = 0; let _ = ctx.resource_revoke(rid); }
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
            if have_mac {
                for server in [dns_server, [8, 8, 8, 8]] {
                    let mut got = false;
                    ip = dns_resolve(&ctx, &pl[1..], &gw_mac, &our_ip, &server, &mut got,
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
                match if have_mac { ping(&ctx, &gw_mac, &our_ip, &dip, bytes, ping_seq, tsc_hz, &mut frames, &mut timeouts) } else { None } {
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
            let rb = match arp_resolve(&ctx, &our_ip, &target) {
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
            gw_mac = d.gw_mac;
            have_mac = d.have_mac;
            dns_server = d.dns_server;
            status = d.status;
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&status));
        } else {
            // Status request (default): reply the CURRENT state, not just the frozen record. Read the link
            // and, if it is down (cable out), clear the "gateway resolved / ping OK" flags so `net` reflects
            // reality instead of stale boot-time info - as adaptable as `ping`. have_mac is NOT cleared (the
            // gateway MAC persists, so `net`/`ping` resume on replug without re-dancing).
            let mut s = status;
            if !link_is_up(&ctx) { s[14] = 0; }
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&s));
        }
        ctx.remove_cap(reply_cap);
    }
}
