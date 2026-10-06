// SPDX-License-Identifier: GPL-2.0-only
//! R4: the dongle as a `Station`, so the serve loop every radio shares (`godspeed_wifi::serve`) answers the
//! shell for it - `wifi scan`, `wifi list`, `wifi status` - exactly as it does for the Pi 4's and the
//! VisionFive's radios.
//!
//! **The sweep is the host's to run.** A full-MAC radio is told "scan" and reports what its firmware heard;
//! this chip is soft-MAC, so the sweep is this file: tune a channel, listen for `DWELL_MS`, tune the next,
//! through 1 to 13, then back to the channel it was on. Since R5a it also ASKS: one wildcard probe request
//! on each channel as it is tuned, the first frame this driver sends. Networks that beacon answer it with a
//! probe response addressed to us (`rx.rs` counts those); a hidden network answers only a probe that names
//! it, which is the join's to send.
//!
//! **Frames arrive as they always did** (R3b): the host takes the bulk IN on its interrupt and sends
//! `NOTE_BULK_IN`, which the serve loop hands to this service's `Host` (`rx.rs`) with the running sweep, and
//! each beacon heard is kept in it. `scan_step` only moves the dial.
//!
//! **What it cannot do yet, and says.** Join, leave and the frame path are R5 and R6; until then a join is
//! refused with a line naming the card that brings it, and the link is reported as not associated - the
//! truth, not a stub.

use godspeed as gs;
use gs::driver::wait::{Budget, Since};
use godspeed_sdk::ServiceContext;
use godspeed_wifi::bss::Scan;
use godspeed_wifi::rxq::RxQueue;
use godspeed_wifi::station::{Link, Outcome, Pulled, ScanStep, Secret, Station};
use godspeed_wifi::wire;

use godspeed_wifi::{mgmt, usbfn};

use crate::{rtl8188, rtl_tx};

/// The channels swept: 1 to 13, the 2.4 GHz channels outside Japan's 14. Listening transmits nothing, so a
/// channel a regulatory domain does not allow us to TRANSMIT on is still one we may hear.
const FIRST: u8 = 1;
const LAST: u8 = 13;
/// How long each channel is listened to. A network beacons every 100 TU (102.4 ms) unless it says otherwise,
/// so this covers one interval with room for the hop itself; a network with a longer interval may be missed
/// on one sweep and heard on the next.
const DWELL_MS: u64 = 150;
/// The serve loop's bound on empty turns, each about a millisecond (`ScanStep::Empty` sleeps 1 ms). A sweep
/// is 13 dwells; three times that is room for a slow turn, and a sweep that runs past it is discarded loudly.
const EMPTY_BOUND: u32 = (LAST - FIRST + 1) as u32 * DWELL_MS as u32 * 3;

/// The dongle, brought up: its address, the channel it rests on, and the sweep when one is running.
pub struct Dongle {
    mac: [u8; 6],
    /// The channel R3a tuned, and the one a sweep returns to.
    home: u8,
    sweep: Option<Hop>,
    /// Hops the RF chip did not take, this boot - each one is said once by the sweep that met it.
    hops_failed: u32,
    /// The 802.11 sequence number of the next frame sent, 12 bits.
    seq: u16,
    /// Probe requests the host took, and refused, in the sweep running now.
    probes_sent: u32,
    probes_refused: u32,
}

/// Where a sweep is: the channel tuned now, and when it was tuned.
struct Hop {
    channel: u8,
    since: Since,
}

impl Dongle {
    pub fn new(mac: [u8; 6], home: u8) -> Self {
        Dongle { mac, home, sweep: None, hops_failed: 0, seq: 0, probes_sent: 0, probes_refused: 0 }
    }

    /// The dongle's own address, from its efuse.
    pub fn address(&self) -> [u8; 6] {
        self.mac
    }

    /// One wildcard probe request on the channel tuned now: the frame (`mgmt::probe_request`), its
    /// descriptor (`rtl_tx::mgmt`), and the host's bulk OUT for the management queue. Counted; the first
    /// refusal is said, with the host's status.
    fn probe(&mut self, ctx: &ServiceContext) {
        let mut frame = [0u8; mgmt::PROBE_REQUEST_MAX];
        let n = mgmt::probe_request(&self.mac, self.seq, &[], &mut frame);
        let desc = rtl_tx::mgmt(n as u16, self.seq, true);
        self.seq = (self.seq + 1) & 0x0FFF;
        let mut req = [0u8; 2 + rtl_tx::TX_DESC_LEN + mgmt::PROBE_REQUEST_MAX];
        req[0] = usbfn::OP_BULK_OUT;
        req[1] = rtl_tx::MGNT_OUT;
        req[2..2 + rtl_tx::TX_DESC_LEN].copy_from_slice(&desc);
        req[2 + rtl_tx::TX_DESC_LEN..2 + rtl_tx::TX_DESC_LEN + n].copy_from_slice(&frame[..n]);
        let took = match crate::host(ctx, &req[..2 + rtl_tx::TX_DESC_LEN + n]) {
            Ok(r) => match r.payload_bytes() {
                [usbfn::OP_BULK_OUT, usbfn::ST_OK, ..] => Ok(()),
                [usbfn::OP_BULK_OUT, st, ..] => Err(match *st {
                    usbfn::ST_NO_DEVICE => "the dongle is no longer bound",
                    usbfn::ST_BAD_REQUEST => "the host has no such bulk OUT, or refused the transfer as malformed",
                    _ => "the device or the bus did not take it",
                }),
                _ => Err("the host answered something other than BULK_OUT - not speaking it yet"),
            },
            Err(why) => Err(why),
        };
        match took {
            Ok(()) => self.probes_sent = self.probes_sent.saturating_add(1),
            Err(why) => {
                if self.probes_refused == 0 {
                    ctx.log_fmt(format_args!("wifi-usb: a probe request was not sent - {}", why));
                }
                self.probes_refused = self.probes_refused.saturating_add(1);
            }
        }
    }

    /// Tune `channel` for the sweep; `false` when the RF chip did not take it, said here.
    fn tune(&mut self, ctx: &ServiceContext, channel: u8) -> bool {
        match rtl8188::set_channel(ctx, channel) {
            Ok(()) => true,
            Err(why) => {
                self.hops_failed = self.hops_failed.saturating_add(1);
                ctx.log_fmt(format_args!("wifi-usb: the sweep could not tune channel {} - {}", channel, why));
                false
            }
        }
    }
}

impl Station for Dongle {
    fn scan_start(&mut self, ctx: &ServiceContext) -> bool {
        if !self.tune(ctx, FIRST) {
            return false;
        }
        self.probes_sent = 0;
        self.probes_refused = 0;
        self.probe(ctx);
        self.sweep = Some(Hop { channel: FIRST, since: Since::now(ctx) });
        ctx.log_fmt(format_args!(
            "wifi-usb: sweep started - listening on channels {} to {}, {} ms each", FIRST, LAST, DWELL_MS));
        true
    }

    fn scan_step(&mut self, scan: &mut Scan, ctx: &ServiceContext) -> ScanStep {
        let Some(hop) = self.sweep.as_ref() else {
            return ScanStep::Ended("no sweep was running");
        };
        if !hop.since.passed(ctx, Budget::ms(DWELL_MS)) {
            return ScanStep::Empty;
        }
        let next = hop.channel + 1;
        if next > LAST {
            self.sweep = None;
            let home = self.home;
            let back = self.tune(ctx, home);
            ctx.log_fmt(format_args!(
                "wifi-usb: sweep done - {} network(s) on channels {} to {}, {} probe request(s) sent, {} not{}",
                scan.count(), FIRST, LAST, self.probes_sent, self.probes_refused,
                if back { "; back on the channel it was on" } else { "; NOT back on the channel it was on (above)" }));
            return ScanStep::Ended("the last channel's dwell");
        }
        if self.tune(ctx, next) {
            self.probe(ctx);
        }
        // A channel the chip would not take is skipped, said once above, and the sweep goes on: the rest of
        // the band is still worth hearing - and is not probed, since a probe would go out on the wrong one.
        self.sweep = Some(Hop { channel: next, since: Since::now(ctx) });
        ScanStep::Frame
    }

    fn scan_abort(&mut self, ctx: &ServiceContext) -> bool {
        self.sweep = None;
        let home = self.home;
        self.tune(ctx, home)
    }

    fn scan_empty_bound(&self) -> u32 {
        EMPTY_BOUND
    }

    fn join(&mut self, _ssid: &[u8], _secret: Secret, ctx: &ServiceContext) -> Outcome {
        ctx.log("wifi-usb: a join needs the transmit path, which is R5 (docs/wifi-usb.md 3) - not attempted");
        Outcome::Failed
    }

    fn forget_keys(&mut self) {}

    fn disassoc(&mut self, _ctx: &ServiceContext) -> bool {
        // Never associated, so there is nothing to leave.
        true
    }

    fn radio_down(&mut self, ctx: &ServiceContext) -> bool {
        ctx.log("wifi-usb: `wifi radio off` for this dongle is not built yet - its receive stays on");
        false
    }

    fn radio_up(&mut self, _ctx: &ServiceContext) -> bool {
        // It is never taken down (above), so it is up.
        true
    }

    fn is_up(&mut self, _ctx: &ServiceContext) -> Option<bool> {
        Some(true)
    }

    fn link(&mut self, _ctx: &ServiceContext) -> Option<Link> {
        // Not associated: no join has been made, because none can be yet.
        Some(Link { bssid: [0; 6], rssi: 0, chanspec: 0 })
    }

    fn mac(&mut self, _ctx: &ServiceContext) -> Option<[u8; 6]> {
        Some(self.mac)
    }

    fn tx_ok(&self) -> bool {
        false
    }

    fn send(&mut self, _eth: &[u8], _ctx: &ServiceContext) -> bool {
        false
    }

    fn pull(&mut self, _rxq: &mut RxQueue, _ctx: &ServiceContext) -> Pulled {
        // Frames reach this service as host notices (`rx.rs`), never by a pull; with no join there are no
        // data frames for the stack either.
        Pulled { data: 0, rekeyed: 0, rekey_failed: 0, pairwise_rekeyed: 0, pairwise_failed: 0, dropped_link: None }
    }

    fn event_name(&self, _code: u32) -> &'static str {
        "(this radio has no firmware events)"
    }

    fn debug(&mut self, _sub: u8, _live: bool, out: &mut [u8], _ctx: &ServiceContext) -> usize {
        out[0] = wire::UNKNOWN_OP;
        1
    }
}
