// SPDX-License-Identifier: GPL-2.0-only
//! THE RADIO AS THE LINK'S SECOND BACKEND (`docs/wifi.md` 2): the radio's service reached over the frame
//! ops, and which of the cable and the radio carries `net-stack`'s frames. Moved out of `genet.rs` whole
//! when the VisionFive's `dwmac` gained the same bridge (`docs/wifi-aic8800.md`, phase V6), so the rule -
//! the cable always wins - and the radio's bounded exchange are written once for every board. Included by
//! `#[path]` from each backend that uses it; see the note where `genet.rs`, `dwmac.rs` and `main.rs` (the Pi 2, and the RTL8168) include it.
//!
//! WHICH SERVICE IS THE RADIO is the backend's to say (`Radio::new`): `wifi-driver` beside GENET and
//! `dwmac`, whose radios are on the board, and `wifi-usb` beside the Pi 2's USB ethernet and the PCs'
//! RTL8168, whose radio is a USB dongle (`docs/wifi-usb.md`, R6 and 39). Both answer the same frame ops
//! through the same serve loop.

use godspeed_sdk::{CapHandle, Message, ServiceContext};
use godspeed::driver::wait::{self, Budget};

/// Which link carries `net-stack`'s frames.
///
/// **THE CABLE ALWAYS WINS.** While the PHY reports a link, frames go over the cable; the moment it does
/// not, they go to the radio's service (`wifi-driver` or `wifi-usb`, `Radio::new`) over the same three ops this service answers upward (`docs/wifi.md` 2) - if
/// the radio is joined - and come back to the cable the moment it returns. Decided by the operator on
/// 2026-09-29, in these words: "cable always wins. unplug the cable, switch to wifi automatically." One
/// link at a time, chosen by the cable rather than by a command, and the choice lives here because this
/// service is the link front end: `net-stack` asks one name and never learns there are two links.
///
/// The cost of the switch is the link's ADDRESS: the radio has its own MAC, so the frames' source changes
/// under the stack. `net-stack` notices that (its status query carries the address) and re-configures,
/// which is the one thing a cable never did to it.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Carrier {
    Cable,
    Radio,
    None,
}

/// How often the cable is re-read on a request. A cable read costs two MDIO transactions (GENET, dwmac)
/// or an IPC to `dwc2` (the Pi 2); every drain would pay it for nothing, and a switch half a second late is
/// not something a person can see. The RTL8168 reads one register and re-reads it on every request.
pub(crate) const CABLE_RECHECK_MS: u64 = 500;

/// The bound on one exchange with the radio's service, in milliseconds - and it is SHORT on purpose. This was a
/// second, and the first boot showed what a second costs: `net-stack` gives up on this service well inside
/// it and sends its next request, which queues behind the one still waiting; sixteen of those and this
/// service's inbox is full, at which point the radio's answer cannot land in it and every exchange times
/// out - a livelock that reads as "the radio stopped responding". The radio answers in about a
/// millisecond when it answers at all (a pull of eight frames is a few), so a hundred is generous for a
/// real answer and short enough that this service keeps pace with its callers while the radio is busy
/// (a JOIN holds it for seconds; every request in that window fails fast instead of piling up).
const RADIO_MS: u64 = 100;
/// An answer slower than this is logged: the cost of the radio path, measured from the side that pays it.
const RADIO_SLOW_MS: u64 = 20;
/// Unanswered requests in a row before the radio's cap is reacquired by name (see `Radio::rpc`).
const RADIO_REACQUIRE_AFTER: u32 = 3;
/// How long a radio that has gone silent (`RADIO_REACQUIRE_AFTER` requests in a row) is held DOWN before
/// the next probe. Every request inside the window is answered without asking the radio, so this driver
/// stays answerable to net-stack while the radio's service is being brought up from cold (~30 s after a
/// `wifi radio reload`) or is simply dead. A second: long enough that the serve loop is not spending
/// its time on bounded waits that will fail, short enough that a radio coming back is noticed at once.
const RADIO_BACKOFF_MS: u64 = 1_000;

/// The radio as a backend: the radio's service reached over the frame ops, the way the Pi 2's `nic-driver`
/// reaches `dwc2` (`main.rs`, `kernel_net_main`) - one bounded request, never re-sent; after
/// `RADIO_REACQUIRE_AFTER` silent requests in a row the cap is reacquired by name (the radio is spawned by
/// the supervisor and may be respawned after us) and the radio held down for `RADIO_BACKOFF_MS`; and every reply
/// checked against the op it answers, because the radio's endpoint also serves the `wifi` utility and a
/// late reply would otherwise be read as the next answer.
pub(crate) struct Radio {
    /// The radio's service, by name: who the frame ops go to and who the log blames.
    name: &'static str,
    answered: u32,
    slow: u32,
    timeouts: u32,
    silent_run: u32,
    mismatch: u32,
    sendfail: u32,
    restale: u32,
    /// Cycle count until which the radio is held DOWN without being asked (0 = not held). Set after
    /// `RADIO_REACQUIRE_AFTER` silent requests in a row; cleared by the first answer. See `RADIO_BACKOFF_MS`.
    down_until: u64,
    /// How many backoff windows this instance has entered, for the log and nothing else.
    backoffs: u32,
    /// CLIENT REQUESTS THAT ARRIVED WHILE THIS DRIVER WAS WAITING ON THE RADIO. The wait in `rpc`
    /// used to take whatever landed next as the radio's answer, and a request from net-stack that
    /// landed in that window was checked against the op it was not, discarded, and its reply cap
    /// with it. net-stack waited out its whole deadline, reacquired and retried, and over the radio -
    /// where net-stack sends back to back - that collision repeated every cycle, and was taken for the
    /// three-second ping of `backlog/66` (whose later DNS failure turned out to be an empty reply the
    /// kernel refused, `e3fcf7ed`). The collision was real either way. A message with a reply cap is a request, never
    /// the radio's reply, so it
    /// is kept here and served before the next `recv`. Two slots, because one `serve` iteration can
    /// ask the radio more than once; a third is dropped loudly with its cap reclaimed, and the client
    /// times out and re-asks, which is defined (26.6, 26.7).
    held: [Option<(Message, CapHandle)>; RADIO_HELD_MAX],
    rescued: u32,
    held_dropped: u32,
    /// The OTHER radio service on a board with two (`wifi hardware use`, `utilities/56_wifi.md` 11): the
    /// onboard radio's and the dongle's. `None` where there is one. See `info`.
    other: Option<&'static str>,
    /// When the other radio was last asked whether it is the one in use: at most every `OTHER_EVERY_MS`.
    other_asked: Option<wait::Since>,
}

/// How often the other radio is asked whether it is the one in use, while the current one says it is
/// not, or does not answer. Bounded, because asking a radio that is not there costs a timeout.
const OTHER_EVERY_MS: u64 = 5_000;
/// `wire::USE_*` (`sdk/wifi`), as literals for the reason the op numbers are: this crate does not link it.
const USE_NOT: u8 = 0;
const USE_THIS: u8 = 1;
/// Where `[0x10]`'s reply carries the radio's `USE_*` value.
const INFO_USE_AT: usize = 15;

/// How many client requests the radio wait can keep for the serve loop. See `Radio::held`.
const RADIO_HELD_MAX: usize = 2;

impl Radio {
    pub(crate) fn new(name: &'static str) -> Self {
        Radio {
            name,
            answered: 0, slow: 0, timeouts: 0, silent_run: 0, mismatch: 0, sendfail: 0, restale: 0,
            down_until: 0, backoffs: 0,
            held: [None, None], rescued: 0, held_dropped: 0,
            other: None, other_asked: None,
        }
    }

    /// A radio bridge with a second radio to follow the operator's choice to (see `info`).
    pub(crate) fn with_other(name: &'static str, other: &'static str) -> Self {
        let mut r = Radio::new(name);
        r.other = Some(other);
        r
    }

    /// Ask `[0x10]` of the OTHER radio, leaving the current one's silence count and backoff as they were:
    /// a probe of a radio that is not there must not hold the current one down.
    ///
    /// **The cap is reacquired by name first, every time.** This driver is spawned before the dongle's
    /// service registers, so it starts with no cap to it at all ("peer 'wifi-usb' not yet registered"),
    /// and the silence count that reacquires the current radio's cap is the one this ask leaves alone - so
    /// the other was asked through nothing, forever, and the bridge never moved (Pi 4, 2026-10-07: `use
    /// usb` reported, the link stayed down). A lookup is cheap and this runs at most every
    /// `OTHER_EVERY_MS`; a name that does not resolve means the other radio is not running, and it is not
    /// asked.
    fn ask_other(&mut self, ctx: &ServiceContext, other: &'static str) -> Option<Message> {
        if !godspeed::cap::reacquire(ctx, other) {
            return None;
        }
        let (name, run, until, offs) = (self.name, self.silent_run, self.down_until, self.backoffs);
        self.name = other;
        self.silent_run = 0;
        self.down_until = 0;
        let r = self.rpc(ctx, &Message::from_bytes(&[0x10]));
        self.name = name;
        self.silent_run = run;
        self.down_until = until;
        self.backoffs = offs;
        r
    }

    /// The oldest held request, if any - served before the next `recv`, because it arrived first.
    pub(crate) fn take_held(&mut self) -> Option<(Message, CapHandle)> {
        let first = self.held[0].take()?;
        self.held[0] = self.held[1].take();
        Some(first)
    }

    fn rpc(&mut self, ctx: &ServiceContext, msg: &Message) -> Option<Message> {
        let want = msg.payload_bytes().first().copied().unwrap_or(0);
        // HELD DOWN: answer without asking. The radio stopped answering `RADIO_REACQUIRE_AFTER` times
        // running, and asking again inside the window would cost this driver the full bound per request
        // - which is how net-stack's exchanges came to queue fifteen seconds behind a wifi-driver that
        // was busy bringing its chip up from cold. When the window ends, exactly one probe goes through.
        if self.down_until != 0 {
            if ctx.read_tsc() < self.down_until {
                return None;
            }
            self.down_until = 0;
        }
        let t0 = ctx.read_tsc();
        // SIFTED, not the first thing that lands. A message carrying a reply cap is a client's
        // request - net-stack asking for a frame or the link - and is kept for the serve loop; the
        // wait goes on for the radio's actual answer. `take_pending_cap` reads and CLEARS the cap the
        // kernel installed for THIS message, so it must be asked here, at arrival (see `held`).
        let name = self.name;
        let got = ctx.request_with_reply_ms_sifted(name, msg, RADIO_MS, |m| {
            let Some(cap) = ctx.take_pending_cap() else { return true; };
            let op = m.payload_bytes().first().copied().unwrap_or(0);
            if let Some(slot) = self.held.iter_mut().find(|s| s.is_none()) {
                *slot = Some((Message::from_bytes(m.payload_bytes()), cap));
                self.rescued = self.rescued.saturating_add(1);
                if self.rescued == 1 || self.rescued % 16 == 0 {
                    ctx.log_fmt(format_args!(
                        "nic-driver: a client's request (op {}) arrived while this driver waited on the radio - kept, served next (#{})",
                        op, self.rescued));
                }
            } else {
                ctx.remove_cap(cap);
                self.held_dropped = self.held_dropped.saturating_add(1);
                if self.held_dropped == 1 || self.held_dropped % 16 == 0 {
                    ctx.log_fmt(format_args!(
                        "nic-driver: a client's request (op {}) arrived while this driver waited on the radio and both held slots were full - dropped, the client times out (#{})",
                        op, self.held_dropped));
                }
            }
            false
        });
        let took_ms = ctx.read_tsc().wrapping_sub(t0) / ctx.duration_cycles(1).max(1);
        let got = match got {
            Some(r) => {
                self.answered = self.answered.saturating_add(1);
                if self.backoffs != 0 && self.silent_run != 0 {
                    ctx.log_fmt(format_args!(
                        "nic-driver: the radio answers again after {} backoff window(s) of {} ms",
                        self.backoffs, RADIO_BACKOFF_MS));
                    self.backoffs = 0;
                }
                self.silent_run = 0;
                // The latency, from this side, which is the side that pays it: the first few always,
                // then only the slow ones, so the log shows what the radio path costs without becoming
                // a per-frame flood.
                if self.answered <= 3 || took_ms >= RADIO_SLOW_MS {
                    if took_ms >= RADIO_SLOW_MS {
                        self.slow = self.slow.saturating_add(1);
                    }
                    if self.answered <= 3 || self.slow <= 3 || self.slow % 64 == 0 {
                        ctx.log_fmt(format_args!(
                            "nic-driver: the radio answered {:#04x} in {} ms ({} answered, {} slow, {} unanswered)",
                            want, took_ms, self.answered, self.slow, self.timeouts));
                    }
                }
                r
            }
            None => {
                // No answer inside the bound. A millisecond bound cannot tell a silent peer from a
                // stale cap (only the seconds form does), so after a few in a row the cap is reacquired
                // by name - which is harmless when it was not stale, and is the only recovery when it
                // was (the radio is respawned by the supervisor and may come back after us). Nothing is
                // re-SENT: a request this driver stopped waiting for must not go out twice.
                self.timeouts = self.timeouts.saturating_add(1);
                self.silent_run = self.silent_run.saturating_add(1);
                if self.timeouts <= 3 || self.timeouts % 64 == 0 {
                    ctx.log_fmt(format_args!(
                        "nic-driver: the radio did not answer {:#04x} within {} ms (x{}, {} in a row)",
                        want, RADIO_MS, self.timeouts, self.silent_run));
                }
                if self.silent_run >= RADIO_REACQUIRE_AFTER {
                    // Held down from here until the window ends (`down_until`); said once per entry
                    // into the window and every sixteenth after, so a radio that is simply gone is a
                    // count rather than a flood.
                    self.down_until = ctx.read_tsc().wrapping_add(ctx.duration_cycles(RADIO_BACKOFF_MS));
                    self.backoffs = self.backoffs.saturating_add(1);
                    if self.backoffs == 1 || self.backoffs % 16 == 0 {
                        ctx.log_fmt(format_args!(
                            "nic-driver: the radio has not answered {} time(s) running - held DOWN for {} ms between probes so this driver stays answerable (backoff #{})",
                            self.silent_run, RADIO_BACKOFF_MS, self.backoffs));
                    }
                }
                // REACQUIRE ON EVERY FAILED PROBE past the threshold, not once at it. Once was 160 ms
                // after a `wifi radio reload` killed the driver - before its respawn had registered - and
                // the cap it got went stale the moment the new instance came up (`cap::get: gen mismatch`,
                // boot 2026-10-01 07:35); every probe after failed on it and nothing ever asked again, so
                // the radio joined and this driver said "not joined" for the rest of the session. A name
                // lookup once a second while the radio is down costs nothing; said once and then every
                // sixteenth, so a radio that is simply gone is a count.
                if self.silent_run >= RADIO_REACQUIRE_AFTER {
                    if ctx.reacquire_by_name(self.name) {
                        self.restale = self.restale.saturating_add(1);
                        if self.restale == 1 || self.restale % 16 == 0 {
                            ctx.log_fmt(format_args!(
                                "nic-driver: the radio was silent {} times running - its cap reacquired by name ({} so far); the next probe tells",
                                self.silent_run, self.restale));
                        }
                    } else {
                        // Once, then every sixteenth, as the comment above says: on a PC with no dongle
                        // and the cable out this is every probe, a line a second for as long as it lasts.
                        self.sendfail = self.sendfail.saturating_add(1);
                        if self.sendfail == 1 || self.sendfail % 16 == 0 {
                            ctx.log_fmt(format_args!(
                                "nic-driver: the radio was silent and its name does not resolve - {} is not running (x{})",
                                self.name, self.sendfail));
                        }
                    }
                }
                return None;
            }
        };
        if got.payload_bytes().first().copied() != Some(want) {
            // A driver with no radio answers every op with one byte, "radio down"; a late reply to an
            // earlier op is the other way this happens. Either way it is not the answer to this ask.
            self.mismatch = self.mismatch.saturating_add(1);
            if self.mismatch == 1 || self.mismatch % 128 == 0 {
                ctx.log_fmt(format_args!(
                    "nic-driver: {} answered {:#04x} while we asked {:#04x} - not our reply ({} mismatched, {} unanswered, {} never sent)",
                    self.name, got.payload_bytes().first().copied().unwrap_or(0), want,
                    self.mismatch, self.timeouts, self.sendfail));
            }
            return None;
        }
        Some(got)
    }

    /// `[0x10]` -> `(mac, link up, access point)`, or `None` when the radio did not answer or has no
    /// address yet. The access point is zeros when the radio does not know it.
    pub(crate) fn info(&mut self, ctx: &ServiceContext) -> Option<([u8; 6], bool, [u8; 6])> {
        let mut r = self.rpc(ctx, &Message::from_bytes(&[0x10]));
        // FOLLOW THE RADIO IN USE (`wifi hardware use`). Each radio says in this reply whether it is the
        // one the operator chose; this driver holds no choice of its own. While the current radio says
        // another is in use, or does not answer, the other is asked (bounded, `OTHER_EVERY_MS`), and the
        // frames move to it when it says it is the one - or when the current one is gone and the other
        // answers, because a saved choice is a preference and a missing radio must not leave the machine
        // without the one that is there (`utilities/56_wifi.md` 11).
        let use_now = r.as_ref().and_then(|m| m.payload_bytes().get(INFO_USE_AT).copied());
        if let Some(other) = self.other {
            let want_other = r.is_none() || use_now == Some(USE_NOT);
            let due = self.other_asked.as_ref().map_or(true, |t| t.passed(ctx, Budget::ms(OTHER_EVERY_MS)));
            if want_other && due {
                self.other_asked = Some(wait::Since::now(ctx));
                if let Some(o) = self.ask_other(ctx, other) {
                    let o_use = o.payload_bytes().get(INFO_USE_AT).copied();
                    if o_use == Some(USE_THIS) || r.is_none() {
                        ctx.log_fmt(format_args!(
                            "nic-driver: the radio bridge now goes to {} ({}) - {} {}",
                            other,
                            if o_use == Some(USE_THIS) { "the radio in use, /wifi.radio" } else { "the one that answers" },
                            self.name,
                            if r.is_none() { "does not answer" } else { "is not the one in use" }));
                        self.other = Some(self.name);
                        self.name = other;
                        self.silent_run = 0;
                        self.down_until = 0;
                        self.backoffs = 0;
                        r = Some(o);
                    }
                }
            }
        }
        let r = r?;
        let p = r.payload_bytes();
        if p.len() < 9 || p[1] == 0 {
            return None;
        }
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&p[2..8]);
        let mut peer = [0u8; 6];
        if p.len() >= 15 {
            peer.copy_from_slice(&p[9..15]);
        }
        Some((mac, p[8] != 0, peer))
    }

    pub(crate) fn tx(&mut self, ctx: &ServiceContext, frame: &[u8]) -> bool {
        let mut req = [0u8; 1 + crate::FRAME_MAX];
        let n = frame.len().min(crate::FRAME_MAX);
        req[0] = 0x11;
        req[1..1 + n].copy_from_slice(&frame[..n]);
        match self.rpc(ctx, &Message::from_bytes(&req[..1 + n])) {
            Some(r) => {
                let p = r.payload_bytes();
                p.len() > 1 && p[1] != 0
            }
            None => false,
        }
    }

    pub(crate) fn rx(&mut self, ctx: &ServiceContext, buf: &mut [u8]) -> usize {
        let r = match self.rpc(ctx, &Message::from_bytes(&[0x12])) {
            Some(r) => r,
            None => return 0,
        };
        let p = r.payload_bytes();
        if p.len() < 3 {
            return 0;
        }
        let n = (p[1] as usize) | ((p[2] as usize) << 8);
        if n == 0 || p.len() < 3 + n || n > buf.len() {
            return 0;
        }
        buf[..n].copy_from_slice(&p[3..3 + n]);
        n
    }
}

/// The STATUS answer, `[ok, mac(6), link, carrier]`, for a backend whose cable is `cable` and whose own
/// address is `mac` - and the carrier logged when it changes. Every backend with a radio answers it the
/// same way, which is why it is here.
pub(crate) fn status(ctx: &ServiceContext, radio: &mut Radio, cable: bool, mac: [u8; 6], carrier: &mut Carrier) -> [u8; 9] {
    // STATUS: [ok, mac(6), link, carrier] - net-stack reads the MAC at [1..7] and the link at
    // [7]. The ninth byte names the carrier for `net` (1 the cable, 2 the radio, 0 neither), and
    // is what makes this reply nine bytes where every other backend's is eight or more, so a
    // reader can tell whose it is. The link is LIVE either way: the cable from the PHY, the radio
    // from the radio service's own word on its join.
    let mut out = [0u8; 9];
    out[0] = 1;
    let next = if cable {
        out[1..7].copy_from_slice(&mac);
        out[7] = 1;
        out[8] = 1;
        Carrier::Cable
    } else {
        match radio.info(ctx) {
            Some((rmac, true, _)) => {
                out[1..7].copy_from_slice(&rmac);
                out[7] = 1;
                out[8] = 2;
                Carrier::Radio
            }
            _ => {
                out[1..7].copy_from_slice(&mac);
                out[7] = 0;
                out[8] = 0;
                Carrier::None
            }
        }
    };
    if next != *carrier {
        match next {
            Carrier::Cable => ctx.log("nic-driver: the cable carries the link; the radio stands by"),
            Carrier::Radio => ctx.log_fmt(format_args!(
                "nic-driver: the cable is out - the radio carries the link (MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x})",
                out[1], out[2], out[3], out[4], out[5], out[6])),
            Carrier::None => ctx.log("nic-driver: the cable is out and the radio is not joined - no link"),
        }
        *carrier = next;
    }
    out
}

/// WHICH ACCESS POINT carries the radio's link: `[ok, peer(6)]` (op 10).
pub(crate) fn peer(ctx: &ServiceContext, radio: &mut Radio, cable: bool) -> [u8; 7] {
    // WHICH ACCESS POINT carries the radio's link: `[ok, peer(6)]`, ok 0 while the cable carries
    // the frames or the radio is not joined. `net-stack` asks only after STATUS has said the
    // radio carries the link, which only a backend with a radio ever says - every other backend would take
    // a one-byte request it does not know for a frame to send.
    let mut out = [0u8; 7];
    if !cable {
        if let Some((_, true, peer)) = radio.info(ctx) {
            out[0] = 1;
            out[1..7].copy_from_slice(&peer);
        }
    }
    out
}
