// SPDX-License-Identifier: GPL-2.0-only
//! net-stack - the model-AGNOSTIC half of networking (docs/networking.md, Phase 2 begins here).
//!
//! nic-driver knows one NIC and speaks raw Ethernet frames; net-stack knows no hardware and speaks
//! ARP/IPv4/ICMP/UDP/TCP over those frames. The seam between them is the **frame interface**: a
//! request/reply (§8.2) where the request payload IS a frame to transmit and the reply payload IS the
//! frame that came back. So ARP lives HERE, in net-stack, over raw frames - not in the driver (where
//! step 4 had it as a bring-up hack). This is Commandment X: the driver is mechanism (put bytes on the
//! wire), the protocol is policy (what the bytes mean), and they live in different services.
//!
//! Phase 2 step 1 (this commit): resolve the QEMU user-net gateway (10.0.2.2) by ARP, THROUGH
//! nic-driver, and log its hardware address. That proves the frame interface end to end and is the
//! first real protocol on the wire. IPv4 + ICMP (ping the host) build on exactly this seam next.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message};

// The NIC's MAC. In QEMU the e1000 default is 52:54:00:12:34:56; a real net-stack learns this from
// nic-driver at init (a small refinement), but nic-driver runs the NIC promiscuous, so the ARP reply
// is received whatever sender MAC we advertise - this keeps step 5 focused on the frame interface.
const OUR_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
// QEMU user-net: the guest is 10.0.2.15, the virtual gateway (which answers ARP) is 10.0.2.2.
const OUR_IP:     [u8; 4] = [10, 0, 2, 15];
const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("net-stack: starting");

    // Build a broadcast ARP request: who-has GATEWAY_IP, tell OUR_IP.
    let mut arp = [0u8; 42];
    for b in arp.iter_mut().take(6) { *b = 0xff; }   // eth dest = broadcast
    arp[6..12].copy_from_slice(&OUR_MAC);            // eth src
    arp[12] = 0x08; arp[13] = 0x06;                  // ethertype = ARP
    arp[14] = 0x00; arp[15] = 0x01;                  // htype = Ethernet
    arp[16] = 0x08; arp[17] = 0x00;                  // ptype = IPv4
    arp[18] = 0x06; arp[19] = 0x04;                  // hlen 6, plen 4
    arp[20] = 0x00; arp[21] = 0x01;                  // oper = request
    arp[22..28].copy_from_slice(&OUR_MAC);           // sender hw
    arp[28..32].copy_from_slice(&OUR_IP);            // sender ip
    // target hw (32..38) = 0 (unknown - that's what we're asking)
    arp[38..42].copy_from_slice(&GATEWAY_IP);        // target ip

    // Send it THROUGH nic-driver's frame interface and wait on the TRUTH of the reply (Commandment
    // VIII): request_with_reply is a synchronous Call, so a dead/absent nic-driver wakes us with
    // None (ReplyDead) rather than hanging - we reacquire by name and retry (Commandment IX).
    let request = Message::from_bytes(&arp);
    loop {
        match ctx.request_with_reply("nic-driver", &request) {
            Some(reply) => {
                let f = reply.payload_bytes();
                // A valid ARP reply: >=42 bytes, ethertype 0x0806, oper=2 (reply). Sender hw addr
                // (bytes 22..28) is the gateway's MAC - the answer to our question.
                if f.len() >= 42 && f[12] == 0x08 && f[13] == 0x06 && f[20] == 0x00 && f[21] == 0x02 {
                    let m = &f[22..28];
                    ctx.log_fmt(format_args!(
                        "net-stack: ARP - {}.{}.{}.{} is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        GATEWAY_IP[0], GATEWAY_IP[1], GATEWAY_IP[2], GATEWAY_IP[3],
                        m[0], m[1], m[2], m[3], m[4], m[5]));
                    break;
                }
                // nic-driver degraded (no e1000) replies empty; or a non-ARP frame arrived. Report
                // loudly and stop - there is no NIC to resolve through (Commandment V, degrade).
                ctx.log_fmt(format_args!(
                    "net-stack: no ARP reply ({}-byte frame back) - no NIC, or nothing answered", f.len()));
                break;
            }
            None => {
                ctx.log("net-stack: nic-driver unreachable - reacquiring by name, retrying");
                ctx.reacquire_by_name("nic-driver");
                ctx.yield_cpu();
            }
        }
    }

    // ARP proven over the frame interface. IPv4 + ICMP (ping the host) build on this same seam next.
    loop {
        while ctx.try_recv().is_some() {}
        ctx.yield_cpu();
    }
}
