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

/// Phase 3: a DHCP DISCOVER over UDP - ask QEMU slirp's built-in DHCP server for our IP and read the
/// OFFER. This proves the UDP transport (the layer the socket capability sits on) over the frame
/// interface. Returns the offered IP, or None (no NIC / nothing answered). A real net-stack would use
/// this to LEARN its own IP instead of hardcoding it; here it demonstrates the round-trip.
fn dhcp_discover(ctx: &ServiceContext) -> Option<[u8; 4]> {
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
    loop {
        match ctx.request_with_reply("nic-driver", &req) {
            Some(reply) => {
                let f = reply.payload_bytes();
                // A DHCP reply: IPv4 (0x0800, IHL 5), UDP (proto 17), BOOTP op = 2 (BOOTREPLY). yiaddr
                // (our offered IP) sits at BOOTP offset 16 = frame offset 58.
                if f.len() >= 62 && f[12] == 0x08 && f[13] == 0x00 && f[14] == 0x45
                    && f[23] == 17 && f[42] == 2 {
                    let ip = [f[58], f[59], f[60], f[61]];
                    ctx.log_fmt(format_args!(
                        "net-stack: DHCP - offered {}.{}.{}.{} (UDP works)", ip[0], ip[1], ip[2], ip[3]));
                    return Some(ip);
                }
                ctx.log_fmt(format_args!(
                    "net-stack: DHCP - {}-byte reply, no offer (no NIC, or nothing answered)", f.len()));
                return None;
            }
            None => {
                // nic-driver still spawning (or restarted): reacquire by name and retry (Commandment
                // IX). This is the FIRST request net-stack makes, so it is where we wait for the driver.
                ctx.log("net-stack: DHCP - nic-driver unreachable, reacquiring, retrying");
                ctx.reacquire_by_name("nic-driver");
                ctx.yield_cpu();
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("net-stack: starting");

    // ---- Phase 3: DHCP FIRST, so net-stack LEARNS its own IP (self-configuring) instead of
    // hardcoding it. This is also where we wait for nic-driver (dhcp_discover retries the first
    // request until the driver answers). Falls back to a default only if there is no NIC / no offer
    // (a non-e1000 host, where nic-driver serves empty replies). The IP it returns is the one ARP +
    // ICMP use below - so DHCP is no longer a hollow demo; it configures the stack.
    let our_ip = dhcp_discover(&ctx).unwrap_or(FALLBACK_IP);

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
    arp[38..42].copy_from_slice(&GATEWAY_IP);        // target ip (target hw stays 0 - the question)

    // Send it THROUGH nic-driver's frame interface, waiting on the TRUTH of the reply (Commandment
    // VIII): request_with_reply is a synchronous Call, so a dead/absent nic-driver wakes us with None
    // (ReplyDead) rather than hanging - we reacquire by name and retry (Commandment IX).
    let arp_req = Message::from_bytes(&arp);
    let mut gw_mac = [0u8; 6];
    let mut have_mac = false;
    loop {
        match ctx.request_with_reply("nic-driver", &arp_req) {
            Some(reply) => {
                let f = reply.payload_bytes();
                if f.len() >= 42 && f[12] == 0x08 && f[13] == 0x06 && f[20] == 0x00 && f[21] == 0x02 {
                    gw_mac.copy_from_slice(&f[22..28]);
                    have_mac = true;
                    ctx.log_fmt(format_args!(
                        "net-stack: ARP - {}.{}.{}.{} is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        GATEWAY_IP[0], GATEWAY_IP[1], GATEWAY_IP[2], GATEWAY_IP[3],
                        gw_mac[0], gw_mac[1], gw_mac[2], gw_mac[3], gw_mac[4], gw_mac[5]));
                } else {
                    ctx.log_fmt(format_args!(
                        "net-stack: no ARP reply ({}-byte frame back) - no NIC, or nothing answered", f.len()));
                }
                break;
            }
            None => {
                ctx.log("net-stack: nic-driver unreachable - reacquiring by name, retrying");
                ctx.reacquire_by_name("nic-driver");
                ctx.yield_cpu();
            }
        }
    }

    // ---- Phase 2 step 2: ICMP - ping the gateway (echo request -> echo reply). Only once ARP gave us
    // the gateway's MAC (an IPv4 packet to it needs a destination hardware address).
    if have_mac {
        // Ethernet (14) + IPv4 (20) + ICMP header (8) + 8-byte payload = 50 bytes.
        let mut frame = [0u8; 50];
        // Ethernet: unicast to the gateway's MAC.
        frame[0..6].copy_from_slice(&gw_mac);
        frame[6..12].copy_from_slice(&OUR_MAC);
        frame[12] = 0x08; frame[13] = 0x00;          // ethertype = IPv4
        // IPv4 header (frame[14..34]).
        frame[14] = 0x45;                            // version 4, IHL 5 (20-byte header, no options)
        frame[15] = 0x00;                            // DSCP/ECN
        let total_len: u16 = 20 + 8 + 8;             // IP + ICMP header + payload = 36
        frame[16] = (total_len >> 8) as u8; frame[17] = total_len as u8;
        frame[18] = 0x00; frame[19] = 0x01;          // identification
        frame[20] = 0x00; frame[21] = 0x00;          // flags / fragment offset
        frame[22] = 64;                              // TTL
        frame[23] = 1;                               // protocol = ICMP
        // frame[24..26] header checksum: left 0, filled after.
        frame[26..30].copy_from_slice(&our_ip);      // source (learned via DHCP)
        frame[30..34].copy_from_slice(&GATEWAY_IP);  // destination
        let ip_ck = checksum(&frame[14..34]);
        frame[24] = (ip_ck >> 8) as u8; frame[25] = ip_ck as u8;
        // ICMP echo request (frame[34..50]).
        frame[34] = 8;                               // type = echo request
        frame[35] = 0;                               // code
        // frame[36..38] checksum: left 0, filled after.
        frame[38] = 0x00; frame[39] = 0x01;          // identifier
        frame[40] = 0x00; frame[41] = 0x01;          // sequence
        for i in 0..8 { frame[42 + i] = b"godspeed"[i]; }  // an identifiable payload
        let icmp_ck = checksum(&frame[34..50]);
        frame[36] = (icmp_ck >> 8) as u8; frame[37] = icmp_ck as u8;

        let ping = Message::from_bytes(&frame);
        match ctx.request_with_reply("nic-driver", &ping) {
            Some(reply) => {
                let f = reply.payload_bytes();
                // A valid echo reply: IPv4 (0x0800), IHL 5, protocol 1 (ICMP), ICMP type 0 (reply).
                // f[26..30] is the source IP - the host that answered our ping.
                if f.len() >= 42 && f[12] == 0x08 && f[13] == 0x00 && f[14] == 0x45
                    && f[23] == 1 && f[34] == 0 {
                    ctx.log_fmt(format_args!(
                        "net-stack: ICMP - {}.{}.{}.{} echo reply (ping OK, {} bytes on the wire)",
                        f[26], f[27], f[28], f[29], f.len()));
                } else {
                    ctx.log_fmt(format_args!(
                        "net-stack: ping - {}-byte frame back, not an echo reply (gateway silent?)", f.len()));
                }
            }
            None => ctx.log("net-stack: nic-driver unreachable during ping - reacquire needed"),
        }
    }

    // DHCP + ARP + ICMP proven over the frame interface, and net-stack now SELF-CONFIGURES its IP
    // (learned via DHCP above, used by ARP + ICMP). The SOCKET CAPABILITY (a socket is a delegated
    // resource cap, minted/revoked by net-stack, §7.10) builds on this seam next.
    loop {
        while ctx.try_recv().is_some() {}
        ctx.yield_cpu();
    }
}
