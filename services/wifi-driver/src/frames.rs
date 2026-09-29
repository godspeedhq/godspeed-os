// SPDX-License-Identifier: GPL-2.0-only
//! The frame interface, served to `nic-driver`: ops `0x10` INFO, `0x11` TX and `0x12` RX - the three
//! `dwc2` serves it on the Pi 2, and the three it serves `net-stack` upward (`docs/wifi.md` 2). Once a
//! station is associated the radio is a frame source, `nic-driver` is the link front end, and this is its
//! fifth backend. Nothing above it changes shape.
//!
//! What lives here is the RECEIVE QUEUE and the pull that fills it. The driver blocks in `recv` when it is
//! idle, so a frame the access point sends waits in the chip until somebody asks. `nic-driver` asks on
//! `net-stack`'s pace, and each ask reads what the chip has waiting - at most `PULL_MAX_READS` frames -
//! into a bounded queue (26.6.1), watching the link as it goes: a `LINK` event without its up bit, or a
//! deauthentication, means the access point dropped us, and the join is forgotten then and there rather
//! than at the next `wifi status`.
//!
//! Transmit is `ctrl::send_data`, the path the handshake proved. Credit for it comes back on RECEIVED
//! frames (`Session::tx_max_seq`), so a stack that only sends would run dry; a dry window pulls first.

use godspeed_sdk::ServiceContext;

use crate::backplane::Window;
use crate::ctrl::{self, Session};
use crate::host::Host;
use crate::join::EVENT_MSG_LINK;
use crate::scan::{self, code, ev, CHANNEL_DATA, CHANNEL_EVENT, CHANNEL_MASK};

/// `[0x10]` -> `[0x10, ok, mac(6), link]`.
pub const OP_NET_INFO: u8 = 0x10;
/// `[0x11, ethernet frame...]` -> `[0x11, sent]`.
pub const OP_NET_TX: u8 = 0x11;
/// `[0x12]` -> `[0x12, len_lo, len_hi, ethernet frame...]`; a length of 0 is "nothing waiting".
pub const OP_NET_RX: u8 = 0x12;

/// The largest ethernet frame handed up - `nic-driver`'s own `FRAME_MAX`, so a reply always fits its
/// buffer and a 4 KiB message.
pub const FRAME_MAX: usize = 1600;
/// Frames the queue holds. Eight is one `nic-driver` batch drain; `net-stack` drains every hundred
/// milliseconds when it has a link, and what does not fit stays in the chip for the next pull.
pub const RX_SLOTS: usize = 8;
/// Frames read off the chip in one pull. A bound in READS, not time: each read is one SDIO header fetch
/// and, when a frame is there, its body; the queue's room bounds it again from the other side.
pub const PULL_MAX_READS: u32 = 8;

/// A bounded ring of received ethernet frames, oldest first. About 13 KiB on the serve loop's stack.
pub struct RxQueue {
    slots: [[u8; FRAME_MAX]; RX_SLOTS],
    lens: [u16; RX_SLOTS],
    head: usize,
    count: usize,
    /// Frames queued and handed up over the driver's life, for `wifi debug stats`.
    pub queued: u32,
    pub handed: u32,
}

impl RxQueue {
    pub fn new() -> Self {
        RxQueue { slots: [[0; FRAME_MAX]; RX_SLOTS], lens: [0; RX_SLOTS], head: 0, count: 0, queued: 0, handed: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn has_room(&self) -> bool {
        self.count < RX_SLOTS
    }

    /// Forget what is queued - on `leave`, on `radio off`, and on a fresh join, because a frame from the
    /// old link handed to the stack on the new one is a frame from nowhere.
    pub fn clear(&mut self) {
        self.head = 0;
        self.count = 0;
    }

    fn push(&mut self, frame: &[u8]) -> bool {
        if !self.has_room() || frame.len() > FRAME_MAX {
            return false;
        }
        let i = (self.head + self.count) % RX_SLOTS;
        self.slots[i][..frame.len()].copy_from_slice(frame);
        self.lens[i] = frame.len() as u16;
        self.count += 1;
        self.queued = self.queued.wrapping_add(1);
        true
    }

    /// The oldest frame, copied into `out`; 0 when nothing is queued or `out` cannot hold it.
    pub fn pop(&mut self, out: &mut [u8]) -> usize {
        if self.count == 0 {
            return 0;
        }
        let n = self.lens[self.head] as usize;
        if n > out.len() {
            return 0;
        }
        out[..n].copy_from_slice(&self.slots[self.head][..n]);
        self.head = (self.head + 1) % RX_SLOTS;
        self.count -= 1;
        self.handed = self.handed.wrapping_add(1);
        n
    }
}

/// What one pull saw, so the serve loop can act on it without this module knowing the join state.
pub struct Pulled {
    /// Data frames queued for the stack.
    pub data: u32,
    /// EAPOL-Key frames that arrived after the join: the access point rekeying the group key. Counted
    /// here and answered nowhere yet (`backlog/64`).
    pub rekey: u32,
    /// The link went down: `(event code, reason)`. A `LINK` event without the up bit, or a
    /// deauthentication or disassociation, in either direction.
    pub dropped_link: Option<(u32, u32)>,
}

/// Read what the chip has waiting, queueing data frames and watching the link.
///
/// The same walk as the join loop's: every frame is noted for the trace and stats, split into its
/// sub-frames if it is a superframe, and each one is either a DATA frame (an ethernet frame behind a BDC
/// header - queued, unless it is EAPOL) or an EVENT (read for the link). Anything else is counted by
/// `note_frame` and left.
pub fn pull(
    h: &Host,
    w: &mut Window,
    s: &mut Session,
    q: &mut RxQueue,
    frame: &mut [u8; ctrl::FRAME],
    ctx: &ServiceContext,
) -> Pulled {
    let mut got = Pulled { data: 0, rekey: 0, dropped_link: None };
    let mut reads = 0u32;
    while reads < PULL_MAX_READS && q.has_room() {
        let f = match ctrl::read_frame(h, w, frame, ctx) {
            Some(f) => f,
            None => break,
        };
        reads += 1;
        s.note_frame(ctx, &f, &frame[..], false);
        let mut subs = [ctrl::Sub::default(); ctrl::MAX_SUBS];
        let nsubs = ctrl::subframes(&f, &frame[..], s.glom_descriptor(), &mut subs, ctx);
        for sub in subs.iter().take(nsubs) {
            let channel = sub.chanflag & CHANNEL_MASK;
            let body = &frame[sub.off..sub.off + sub.len];
            if channel == CHANNEL_DATA {
                let eth = match scan::ethernet_at(body) {
                    Some(eth) => eth,
                    None => continue,
                };
                let eth_frame = &body[eth..];
                if eth_frame.len() < ev::ETHHDR {
                    continue;
                }
                let ethertype = u16::from_be_bytes([eth_frame[ev::ETHERTYPE], eth_frame[ev::ETHERTYPE + 1]]);
                if ethertype == crate::eapol::ETHERTYPE_EAPOL {
                    got.rekey += 1;
                    continue;
                }
                if q.push(eth_frame) {
                    got.data += 1;
                }
            } else if channel == CHANNEL_EVENT {
                let e = match scan::parse_event(body, s.stats.rx_event, ctx) {
                    Some(e) => e,
                    None => continue,
                };
                match e.event_type {
                    code::LINK if e.flags & EVENT_MSG_LINK == 0 => {
                        got.dropped_link = Some((e.event_type, e.reason));
                    }
                    code::DEAUTH | code::DEAUTH_IND | code::DISASSOC | code::DISASSOC_IND => {
                        got.dropped_link = Some((e.event_type, e.reason));
                    }
                    _ => {}
                }
            }
        }
    }
    got
}
