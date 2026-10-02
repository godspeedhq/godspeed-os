// SPDX-License-Identifier: GPL-2.0-only
//! The Broadcom CYW43455 as a `Station`: each method is the call the serve loop used to make directly,
//! with the bus, the backplane window, the firmware session and the join's keys held here instead of
//! threaded through the loop.

use godspeed_sdk::ServiceContext;
use godspeed_wifi::bss::Scan;
use godspeed_wifi::rxq::RxQueue;
use godspeed_wifi::sdio::SdioHost;
use godspeed_wifi::station::{Link, Outcome, Pulled, ScanStep, Secret, Station};

use crate::{backplane, ctrl, frames, join, scan};

pub struct Bcm<'a> {
    h: &'a dyn SdioHost,
    w: &'a mut backplane::Window,
    s: ctrl::Session,
    /// The keys a WPA2 join keeps for the rekeys to come; `None` on an open network and after any end of
    /// the association.
    keys: Option<join::Keys>,
    /// The frame buffer the sweep and the pulls read into - here, so the loop does not carry it.
    frame: [u8; ctrl::FRAME],
}

impl<'a> Bcm<'a> {
    pub fn new(h: &'a dyn SdioHost, w: &'a mut backplane::Window, s: ctrl::Session) -> Self {
        Bcm { h, w, s, keys: None, frame: [0u8; ctrl::FRAME] }
    }
}

impl Station for Bcm<'_> {
    fn scan_start(&mut self, ctx: &ServiceContext) -> bool {
        scan::start(self.h, &mut *self.w, &mut self.s, ctx)
    }
    fn scan_step(&mut self, sc: &mut Scan, ctx: &ServiceContext) -> ScanStep {
        scan::step(self.h, &mut *self.w, &mut self.s, sc, &mut self.frame, ctx)
    }
    fn scan_abort(&mut self, ctx: &ServiceContext) -> bool {
        scan::abort(self.h, &mut *self.w, &mut self.s, ctx)
    }
    fn scan_empty_bound(&self) -> u32 {
        scan::MAX_EMPTY_POLLS
    }
    fn join(&mut self, ssid: &[u8], secret: Secret, ctx: &ServiceContext) -> Outcome {
        join::join(self.h, &mut *self.w, &mut self.s, ssid, secret, &mut self.keys, ctx)
    }
    fn forget_keys(&mut self) {
        join::forget(&mut self.keys);
    }
    fn disassoc(&mut self, ctx: &ServiceContext) -> bool {
        ctrl::disassoc(self.h, &mut *self.w, &mut self.s, ctx)
    }
    fn radio_down(&mut self, ctx: &ServiceContext) -> bool {
        ctrl::radio_down(self.h, &mut *self.w, &mut self.s, ctx)
    }
    fn radio_up(&mut self, ctx: &ServiceContext) -> bool {
        ctrl::interface_up(self.h, &mut *self.w, &mut self.s, ctx)
    }
    fn is_up(&mut self, ctx: &ServiceContext) -> Option<bool> {
        ctrl::is_up(self.h, &mut *self.w, &mut self.s, ctx)
    }
    fn link(&mut self, ctx: &ServiceContext) -> Option<Link> {
        ctrl::link_now(self.h, &mut *self.w, &mut self.s, ctx)
    }
    fn mac(&mut self, ctx: &ServiceContext) -> Option<[u8; 6]> {
        let mut mac = [0u8; 6];
        match ctrl::query_iovar(self.h, &mut *self.w, &mut self.s, "cur_etheraddr", &mut mac, ctx) {
            Some(n) if n >= 6 => Some(mac),
            _ => None,
        }
    }
    fn tx_ok(&self) -> bool {
        self.s.tx_ok()
    }
    fn send(&mut self, eth: &[u8], ctx: &ServiceContext) -> bool {
        ctrl::send_data(self.h, &mut *self.w, &mut self.s, eth, ctx)
    }
    fn pull(&mut self, rxq: &mut RxQueue, ctx: &ServiceContext) -> Pulled {
        frames::pull(self.h, &mut *self.w, &mut self.s, rxq, &mut self.frame, self.keys.as_mut(), ctx)
    }
    fn event_name(&self, code: u32) -> &'static str {
        scan::code::name(code)
    }
    fn debug(&mut self, sub: u8, live: bool, out: &mut [u8], ctx: &ServiceContext) -> usize {
                out[0] = scan::reply::OK;
                match sub {
                    scan::reply::dbg::TRACE => {
                        let held = self.s.trace.held();
                        out[1] = held as u8;
                        let mut at = 2;
                        for i in 0..held {
                            let e = self.s.trace.entry(i);
                            out[at..at + 4].copy_from_slice(&e.ms.to_le_bytes());
                            out[at + 4] = e.kind;
                            out[at + 5] = e.chanflag;
                            out[at + 6..at + 8].copy_from_slice(&e.id.to_le_bytes());
                            out[at + 8..at + 12].copy_from_slice(&e.what.to_le_bytes());
                            out[at + 12..at + 16].copy_from_slice(&e.status.to_le_bytes());
                            // Entry stride is 18: len takes the last two bytes.
                            out[at + 16..at + 18].copy_from_slice(&e.len.to_le_bytes());
                            at += 18;
                        }
                        at
                    }
                    scan::reply::dbg::FIRMWARE => {
                        // Asked of the firmware now, not remembered from boot - but not mid-sweep, for the
                        // reason `OP_STATUS` gives.
                        let mut at = 1;
                        let mut ver = [0u8; 128];
                        let mut cap = [0u8; 512];
                        let mut mac = [0u8; 6];
                        if live {
                            let _ = ctrl::query_iovar(self.h, &mut *self.w, &mut self.s, "ver", &mut ver, ctx);
                            let _ = ctrl::query_iovar(self.h, &mut *self.w, &mut self.s, "cap", &mut cap, ctx);
                            let _ = ctrl::query_iovar(self.h, &mut *self.w, &mut self.s, "cur_etheraddr", &mut mac, ctx);
                        }
                        let vlen = ver.iter().position(|&b| b == 0).unwrap_or(ver.len());
                        let clen = cap.iter().position(|&b| b == 0).unwrap_or(cap.len());
                        out[at] = vlen as u8;
                        at += 1;
                        out[at..at + 128].copy_from_slice(&ver);
                        at += 128;
                        out[at..at + 2].copy_from_slice(&(clen as u16).to_le_bytes());
                        at += 2;
                        out[at..at + 512].copy_from_slice(&cap);
                        at += 512;
                        out[at..at + 6].copy_from_slice(&mac);
                        at + 6
                    }
                    _ => {
                        let st = &self.s.stats;
                        let words: [u32; 20] = [
                            st.ctrl_sent, st.ctrl_accepted, st.ctrl_refused, st.ctrl_unanswered,
                            st.rx_ctrl, st.rx_event, st.rx_data, st.rx_glom, st.rx_header_only, st.rx_other,
                            st.tx_bytes, st.rx_bytes, st.rx_skipped_in_ctrl_wait,
                            st.events[0], st.events[1], st.events[2], st.events[3], st.events[4],
                            st.events[5], st.events[6],
                        ];
                        let mut at = 1;
                        for w32 in words.iter() {
                            out[at..at + 4].copy_from_slice(&w32.to_le_bytes());
                            at += 4;
                        }
                        // The last three event buckets, then the last-seen facts.
                        for w32 in [st.events[7], st.events[8], st.events[9]].iter() {
                            out[at..at + 4].copy_from_slice(&w32.to_le_bytes());
                            at += 4;
                        }
                        out[at..at + 4].copy_from_slice(&st.last_event_code.to_le_bytes());
                        out[at + 4..at + 8].copy_from_slice(&st.last_event_status.to_le_bytes());
                        out[at + 8..at + 12].copy_from_slice(&st.last_refused_cmd.to_le_bytes());
                        out[at + 12..at + 16].copy_from_slice(&st.last_refused_status.to_le_bytes());
                        at += 16;
                        // And the driver's clock, so the shell can say how long the session has run.
                        out[at..at + 4].copy_from_slice(&self.s.now_ms(ctx).to_le_bytes());
                        at += 4;
                        out[at..at + 4].copy_from_slice(&self.s.trace.total().to_le_bytes());
                        at += 4;
                        out[at..at + 4].copy_from_slice(&st.rx_glom_sub.to_le_bytes());
                        at + 4
                    }
                }
            }
}
