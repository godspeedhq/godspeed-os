// SPDX-License-Identifier: GPL-2.0-only
//! The serve loop every radio shares: what `wifi` asks, answered; the frame ops `nic-driver` asks,
//! answered; the scan cache, the credential table and `/wifi.keys`, auto-join and the rejoin after
//! `radio on`. ONE copy, run by every radio service over its [`Station`].
//!
//! It lived in `wifi-driver`'s `main.rs` until the third radio (2026-10-06): the Broadcom and the AIC8800
//! both ran it from there, and the Pi 2's USB dongle is driven from another service, `wifi-usb`, which
//! must answer the shell the same way. Moved unchanged in what it decides - the reply layouts, the
//! order of every arm, the key rules - with three differences, each forced by the move:
//!
//! - **`who`**: the service's name opens every line it logs, where `wifi-driver:` was written in.
//! - **[`Host`]**: what is AROUND the radio rather than in it. The power operations (the Pi 4 and the
//!   VisionFive cut and restore the chip's power through `DevicePower`; the dongle powers its chip down
//!   by register (R9), its port's 5 V staying on), and the notices that arrive with no reply cap - which
//!   were counted and dropped, and on `wifi-usb` are the host saying a transfer is waiting or the
//!   dongle went away.
//! - **`gs`**: the receive and the reply are the standard library's (`gs::ipc`), the one way a service
//!   is written (backlog/71). `gs::ipc::reply` is exactly the `try_send` and the reclaim the loop made.

use godspeed as gs;
use gs::driver::wait::{Budget, Since};
use godspeed_sdk::{Message, ServiceContext};

use crate::bss::{sec, write_records, write_reply, Network, Scan};
use crate::rxq::RxQueue;
use crate::station::{Outcome, Pulled, ScanStep, Secret, Station};
use crate::wire::{self, PASS_MAX, SSID_MAX};
use crate::{crypto, keyfile};

/// An ethernet header, `ETH_HLEN`: the shortest frame `OP_NET_TX` will hand a radio.
const ETH_HEADER: usize = 14;

/// How long a joined radio may go unread before the serve loop reads it itself. `nic-driver` reads the chip
/// through NET_RX many times a second where it is bridged to the radio; where it is not (the VisionFive
/// until phase V6), nothing did, and the first AIC8800 link was found dropped within 2.5 minutes with its
/// frames unread (boot 2026-10-05 10:05). The read is the same `pull` NET_RX makes, so a group rekey is
/// answered and a dropped link is seen when it happens rather than at the next `wifi status`.
const IDLE_PULL_MS: u64 = 250;

/// What a [`Host`] made of a message that arrived with no reply cap.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// Not the host's: counted and dropped as a request with nothing to answer on.
    Ignored,
    /// The host took it, and the radio is the one it was.
    Taken,
    /// The radio under the loop changed - bound, removed, or replaced - so the loop returns and its
    /// caller brings up whatever is there now.
    Changed,
}

/// What is around the radio rather than in it: who can cut its power, and who sends it notices.
///
/// Every method has the answer for a radio with neither, so a host implements only what it has.
pub trait Host {
    /// Whether this host can cut the chip's power at all. Asked BEFORE `wifi radio off hard` leaves the
    /// network: a host with no way to power its chip down says no, and leaving first and then finding no power to cut left
    /// the station off its network while the shell said "the radio is as it was" (seen on the Pi 2,
    /// 2026-10-06). The kernel may still refuse a host that says yes; that case is unchanged.
    fn can_cut_power(&self) -> bool {
        false
    }
    /// Cut the chip's power and leave the host's lines quiet. `false`: this machine has no control over
    /// the radio's power (the kernel refused, or there is no such thing to ask for).
    fn cut_power(&mut self, _ctx: &ServiceContext) -> bool {
        false
    }
    /// After a cut: is it really off? A `wire::OFF_*` verdict, the host's lines left quiet again.
    fn verify_off(&mut self, _ctx: &ServiceContext) -> u8 {
        wire::OFF_UNVERIFIED
    }
    /// Restore the power the way a board power-on would. `false` when it could not.
    fn restore_power(&mut self, _ctx: &ServiceContext) -> bool {
        false
    }
    /// Cut the power and restore it. `units` is the request's: 100 ms each, 0 for the host's own
    /// default. `false` when there is no control over the power.
    fn power_cycle(&mut self, _ctx: &ServiceContext, _units: u8) -> bool {
        false
    }
    /// What this radio is, for `wifi hardware` (`wire::OP_HARDWARE`): the chip's name and the bus it is
    /// reached over. The host's to say, not the station's: it is true while the radio is down, when there
    /// is no station to ask.
    fn hardware(&self) -> Hardware {
        Hardware { chip: "unknown", bus: "unknown" }
    }
    /// The host's facts for `wifi hardware <radio>` (`wire::OP_HARDWARE_DETAIL`): the ones true while the
    /// radio is down - the chip as identified, its IDs, the bus and its endpoints. The station adds the
    /// ones that need a running chip (`Station::details`).
    fn details(&self, _d: &mut Details) {}
    /// A message with no reply cap. `sweep` is the scan running now, so a host whose frames arrive as
    /// notices can keep what a sweep hears.
    fn notice(&mut self, _msg: &[u8], _sweep: Option<&mut Scan>, _ctx: &ServiceContext) -> Notice {
        Notice::Ignored
    }
}

/// A [`Host`] with neither power control nor notices.
pub struct NoHost;
impl Host for NoHost {}

/// One line, opened with the service's name.
fn say(ctx: &ServiceContext, who: &str, line: &str) {
    ctx.log_fmt(format_args!("{}: {}", who, line));
}

/// [`say`], formatted.
fn say_fmt(ctx: &ServiceContext, who: &str, args: core::fmt::Arguments) {
    ctx.log_fmt(format_args!("{}: {}", who, args));
}

/// The labelled facts of `wire::OP_HARDWARE_DETAIL`, written into the reply as they are added. Bounded by
/// the buffer it was given: a fact that does not fit is dropped, the ones before it kept. A label that is
/// not one of `wire::DETAIL_LABELS` is dropped too, so the shell's record and the reply cannot disagree.
pub struct Details<'b> {
    buf: &'b mut [u8],
    at: usize,
    count: u8,
}

impl<'b> Details<'b> {
    /// Facts written from the start of `buf`.
    pub fn new(buf: &'b mut [u8]) -> Self {
        Details { buf, at: 0, count: 0 }
    }
    /// One fact: `label` and its value, formatted, cut at `wire::DETAIL_VALUE_MAX` bytes.
    pub fn add(&mut self, label: &str, value: core::fmt::Arguments) {
        struct Cap<'c> { b: &'c mut [u8; wire::DETAIL_VALUE_MAX], n: usize }
        impl core::fmt::Write for Cap<'_> {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                for &c in s.as_bytes() {
                    if self.n < self.b.len() {
                        self.b[self.n] = c;
                        self.n += 1;
                    }
                }
                Ok(())
            }
        }
        if !wire::DETAIL_LABELS.contains(&label) || label.len() > wire::DETAIL_LABEL_MAX {
            return;
        }
        let mut v = [0u8; wire::DETAIL_VALUE_MAX];
        let mut cap = Cap { b: &mut v, n: 0 };
        let _ = core::fmt::write(&mut cap, value);
        let n = cap.n;
        let need = 2 + label.len() + n;
        if self.at + need > self.buf.len() || self.count == u8::MAX {
            return;
        }
        self.buf[self.at] = label.len() as u8;
        self.buf[self.at + 1..self.at + 1 + label.len()].copy_from_slice(label.as_bytes());
        let vat = self.at + 1 + label.len();
        self.buf[vat] = n as u8;
        self.buf[vat + 1..vat + 1 + n].copy_from_slice(&v[..n]);
        self.at += need;
        self.count += 1;
    }
    /// Whether `label` has been given already, so a later source does not repeat it.
    pub fn has(&self, label: &str) -> bool {
        let mut at = 0;
        for _ in 0..self.count {
            let l = self.buf[at] as usize;
            if &self.buf[at + 1..at + 1 + l] == label.as_bytes() {
                return true;
            }
            let v = self.buf[at + 1 + l] as usize;
            at += 2 + l + v;
        }
        false
    }
    /// Facts written, and the bytes they take.
    pub fn done(&self) -> (u8, usize) {
        (self.count, self.at)
    }
}

/// A radio's chip and bus, as `wifi hardware` shows them (`Host::hardware`).
pub struct Hardware {
    pub chip: &'static str,
    pub bus: &'static str,
}

/// A request's correlation tag, if it carries one (`wire::TAGGED`), and the request without it.
pub fn untag(raw: &[u8]) -> (Option<u8>, &[u8]) {
    match raw {
        [wire::TAGGED, tag, rest @ ..] => (Some(*tag), rest),
        _ => (None, raw),
    }
}

/// The reply to send: `body` as it is for an untagged request, `[TAGGED, tag, body...]` for a tagged one.
pub fn tagged_reply(tag: Option<u8>, body: &[u8]) -> Message {
    match tag {
        None => Message::from_bytes(body),
        Some(t) => {
            let mut m = Message::from_bytes(&[wire::TAGGED, t]);
            let k = body.len().min(m.payload.len() - 2);
            m.payload[2..2 + k].copy_from_slice(&body[..k]);
            m.payload_len = 2 + k;
            m
        }
    }
}

/// Serve the shell and `nic-driver`: sweeps as a state the loop advances, the cache, joins, keys, status,
/// debug - and the frame interface (`wire::OP_NET_*`), over which the radio carries the stack's traffic
/// when the cable is out. Without a radio, every request but the hardware and use ops and the power ops
/// (`powercycle`, `off hard`) is answered "radio down", so a shell never has to tell a down radio from a
/// wedged one by waiting.
///
/// The loop never sits inside a sweep: `Station::scan_step` advances it one turn at a time and
/// `try_recv` answers requests between turns, which is what lets `q` stop a sweep
/// (`utilities/56_wifi.md` rule 11) and `wifi list` answer during one. It DOES sit inside a join (a few
/// seconds), and `nic-driver`'s bound on it is short for exactly that reason.
///
/// `crypto_ok` is the caller's `crypto::selftest`, run once before the first call.
///
/// `who` is the service's name, and every line this loop logs opens with it. It returns only when
/// `host` says the radio under it changed (`Notice::Changed`) - a USB dongle bound or removed. A radio
/// soldered to the board never does, and its service calls this again if it ever returns.
pub fn serve<'s>(
    ctx: &ServiceContext,
    who: &str,
    mut radio: Option<&mut (dyn Station + 's)>,
    host: &mut dyn Host,
    down_reason: u8,
    crypto_ok: bool,
) {
    // THE RADIO, AS A STATION. Everything the loop below asks of the chip goes through `radio`; the loop
    // itself is the policy every radio shares (`godspeed_wifi::station`). `host` is here for the power
    // operations, which are the host's and the kernel's, not the chip's, and for the notices that arrive
    // with no reply cap. The caller builds the station: the Broadcom's and the AIC8800's in `wifi-driver`,
    // the Realtek's in `wifi-usb`.
    // Bounded: 2 status bytes plus 32 records of 45 is 1442 for a list, and 2 plus 64 names of 33 is 2114 for
    // `stored` - both inside this fixed buffer, inside a 4 KiB message.
    let mut out = [0u8; 2560];

    // THE SWEEP IS A STATE, NOT A CALL. `wifi scan` starts one and returns; the loop below advances it one
    // frame per turn and answers requests between frames, so `wifi list`, `wifi status` and an abort are
    // heard while the radio sweeps (rule 11), and a sweep left running by `b` finishes on its own. A
    // finished sweep becomes THE CACHE - the one list `wifi list` ever prints - and only a finished one
    // does: an abort or a sweep that fell silent past the poll bound is discarded, never half-served.
    struct Sweep {
        scan: Scan,
        empty: u32,
    }
    struct Cache {
        scan: Scan,
        at_secs: i64,
    }
    let mut sweep: Option<Sweep> = None;
    let mut cache: Option<Cache> = None;
    // The last sweep died by the poll bound rather than the firmware's word. Pollers are told, not handed
    // whatever older cache exists as if it were the sweep they asked for.
    let mut sweep_failed = false;
    // `bring_up` ran the UP chain, so the radio starts on; `wifi radio off` is the only thing that turns
    // it off, and `wifi radio on` re-runs the same chain.
    let mut radio_on = true;
    // `wifi radio off hard` cut the chip's power and this instance is still here to say so. While it is
    // set, status, the radio op and the hardware and use ops are served and everything else is answered
    // RADIO_POWERED_OFF; `on`
    // restores the power and asks to be restarted, because a cold chip needs the boot's own path.
    let mut powered_off = false;
    // The network the firmware last reported JOINED, cleared by a disconnect or a power-off.
    let mut joined: Option<([u8; SSID_MAX], u8)> = None;
    // When the join was reported, and with what security - for `status`'s `joined N s ago` and `security`.
    let mut joined_at_secs: i64 = 0;
    let mut joined_security: u8 = sec::OPEN;
    // THE FRAME PATH (`wire::OP_NET_*`, with the shared queue `crate::rxq`): what `nic-driver` asks once
    // the cable is out. The queue is bounded and on this stack; the address is asked of the chip once; the
    // rekey count is for the log.
    let mut rxq = RxQueue::new();
    let mut link_mac: Option<[u8; 6]> = None;
    // The access point of the current join, for INFO: asked of the firmware once per join, keyed by
    // `joined_at_secs`. A join takes seconds, so two joins cannot share one. Zeros mean "not known",
    // which `net-stack` reads as no evidence either way.
    let mut link_peer: [u8; 6] = [0; 6];
    let mut link_peer_for: Option<i64> = None;
    // Pairwise rekeys that did not complete, for the log.
    let mut rekey_seen: u32 = 0;
    // When a frame op (NET_RX or NET_TX) last read the chip, and the frames the loop's own idle read threw
    // away because nobody took them (`IDLE_PULL_MS`). Discarded, not kept: a frame nobody asked for in a
    // quarter of a second has nobody waiting for it, and the queue holds eight.
    let mut last_frame_op = Since::now(ctx);
    let mut idle_discarded: u32 = 0;
    // The loop's own reads, said once a minute while joined: proof that they happen, which ten quiet
    // minutes of log (2026-10-05 10:23) could not give.
    let mut idle_reads: u32 = 0;
    let mut idle_said_at = Since::now(ctx);
    // The network last JOINED this boot - name, length, security - so `radio on` after `radio off` can
    // go back to it without being asked (`utilities/56_wifi.md` 2). Set on every successful join, cleared
    // by `wifi leave` (an explicit leave means "not this one") and by `forget` of that name.
    let mut last_joined: Option<([u8; SSID_MAX], u8, u8)> = None;
    /// The reply status for a join outcome - one table, used by `wifi join` and by the rejoin.
    fn reply_of(outcome: Outcome) -> u8 {
        match outcome {
            Outcome::Joined => wire::JOINED,
            Outcome::NotFound => wire::NOT_FOUND,
            Outcome::PassphraseRefused => wire::PASSPHRASE_REFUSED,
            Outcome::Failed => wire::JOIN_FAILED,
            Outcome::Timeout => wire::JOIN_TIMEOUT,
            Outcome::HandshakeUnimplemented => wire::HANDSHAKE_UNIMPLEMENTED,
        }
    }
    // THE KEY FILE (`keyfile.rs`, `/wifi.keys`). Loaded once the radio is up - retried a bounded number
    // of times while `fs` is still coming up, then given up with a line - and the most recent entry is
    // joined without being asked. Written after every change to the table.
    let mut keyfile_settled = false;
    let mut keyfile_tries: u32 = 0;
    // WHICH RADIO IS IN USE (`wifi hardware use`, `utilities/56_wifi.md` 11): `/wifi.radio`, written by the
    // shell, read here beside `/wifi.keys` and kept current by `wire::OP_USE`. A radio that is not the one
    // in use does not rejoin at start, and says so in its link status, so `nic-driver` follows the choice.
    let mut choice_use: u8 = wire::USE_DEFAULT;
    let mut choice_settled = false;
    // STANDING IN (`docs/wifi-usb.md` 49). Whether the OTHER radio is attached, as the supervisor says it
    // (`wire::NOTE_USB_RADIO`) - a fact, not a guess from timing: at boot the dongle's driver starts after
    // this one, so "not running yet" is not "absent". `None` until told. The dongle's own driver never
    // stands in for the onboard radio, which is part of the board, so for it the other is always there.
    let mut other_present: Option<bool> = if wire::radio_name(who) == "usb" { Some(true) } else { None };
    // Joined in the chosen radio's place, while it is absent; left when it arrives.
    let mut standing_in = false;
    // The network to stand in on - the auto-join this radio did not make because it was not chosen.
    let mut standby: Option<([u8; SSID_MAX], u8, u8)> = None;
    // The wait for that fact, said once.
    let mut said_waiting = false;
    const KEYFILE_TRIES: u32 = 15;
    let mut auto_join: Option<([u8; SSID_MAX], u8, u8)> = None;
    // Why `auto_join` is set, for its line: the boot rejoin from `/wifi.keys`, or the rejoin after the
    // access point dropped the link (`rejoin_after_drop`).
    let mut auto_join_why: &str = "from /wifi.keys";
    /// THE REJOIN AFTER A DROP. An access point drops a station for its own reasons - on 2026-10-08 a
    /// VisionFive idle for half an hour was disassociated for inactivity (reason 4) - and the network is
    /// still there and still the one chosen, so it is rejoined, ONCE per drop, through the same path as the
    /// boot rejoin (`auto_join`, which also honours `/wifi.radio`). Not when the link lasted under
    /// `REJOIN_MIN_SECS`: an access point that drops every join would otherwise be rejoined forever, and a
    /// drop that soon is an answer about this station, not an accident (Commandment VIII). Either way the
    /// line says what was done and why.
    fn rejoin_after_drop(
        ctx: &ServiceContext,
        who: &str,
        joined_at_secs: i64,
        last_joined: Option<([u8; SSID_MAX], u8, u8)>,
    ) -> Option<([u8; SSID_MAX], u8, u8)> {
        const REJOIN_MIN_SECS: i64 = 60;
        let lasted = (gs::task::epoch_secs_monotonic(ctx) - joined_at_secs).max(0);
        match last_joined {
            None => {
                say(ctx, who, "nothing to rejoin - `wifi join` returns");
                None
            }
            Some(_) if lasted < REJOIN_MIN_SECS => {
                say_fmt(ctx, who, format_args!(
                    "not rejoined: the link lasted {} s, under {} - an access point that drops a join that soon would be rejoined forever; `wifi join` returns",
                    lasted, REJOIN_MIN_SECS));
                None
            }
            some => some,
        }
    }
    /// Join a network this driver holds a key for - the rejoin after `radio on`, and the auto-join at boot
    /// from `/wifi.keys`. `None` when there is no key for a WPA2 name (nothing was attempted); otherwise
    /// the join's outcome, with the driver's memory of the link updated either way.
    fn join_known(
        session: &mut dyn Station,
        name: &[u8; SSID_MAX],
        len: u8,
        sec: u8,
        stored: &mut [Option<Stored>],
        joined: &mut Option<([u8; SSID_MAX], u8)>,
        joined_at_secs: &mut i64,
        joined_security: &mut u8,
        rxq: &mut RxQueue,
        ctx: &ServiceContext,
    ) -> Option<Outcome> {
        let ssid = &name[..len as usize];
        let mut pmk_buf = [0u8; crypto::PMK_LEN];
        let secret = if sec == sec::OPEN {
            Some(Secret::Open)
        } else {
            match slot_of(stored, ssid).and_then(|i| stored[i].as_ref()) {
                Some(st) => {
                    pmk_buf = st.pmk;
                    Some(Secret::Pmk(&pmk_buf))
                }
                None => None,
            }
        };
        let secret = secret?;
        let outcome = session.join(ssid, secret, ctx);
        if outcome == Outcome::Joined {
            *joined = Some((*name, len));
            *joined_at_secs = gs::task::epoch_secs_monotonic(ctx);
            *joined_security = sec;
            rxq.clear();
            if let Some(st) = slot_of(stored, ssid).and_then(|i| stored[i].as_mut()) {
                st.used_at = gs::task::epoch_secs_monotonic(ctx);
            }
        } else {
            *joined = None;
            let _ = session.disassoc(ctx);
        }
        pmk_buf.fill(0);
        Some(outcome)
    }
    /// Write the table to `/wifi.keys`, most recently used first. Called after any change to it.
    fn save_keys(ctx: &ServiceContext, who: &str, stored: &[Option<Stored>], forgotten: Option<&[u8]>) {
        let mut entries = [keyfile::Entry::EMPTY; keyfile::MAX_SAVED];
        let mut n = 0usize;
        // Selection by recency into a bounded array: the table is 64 slots and this is 48 picks of it.
        let mut taken = [false; CREDENTIAL_SLOTS];
        while n < keyfile::MAX_SAVED {
            let mut best: Option<usize> = None;
            for (i, s) in stored.iter().enumerate() {
                if taken[i] {
                    continue;
                }
                if let Some(st) = s {
                    if best.map_or(true, |b| stored[b].as_ref().map_or(true, |bs| st.used_at > bs.used_at)) {
                        best = Some(i);
                    }
                }
            }
            let Some(i) = best else { break };
            taken[i] = true;
            if let Some(st) = stored[i].as_ref() {
                entries[n] = keyfile::Entry { ssid: st.ssid, len: st.len, sec: sec::WPA2, pmk: st.pmk };
                n += 1;
            }
        }
        if keyfile::save(ctx, who, &entries[..n], forgotten) {
            say_fmt(ctx, who, format_args!("/wifi.keys written - {} network(s)", n));
        }
        for e in entries.iter_mut() {
            e.pmk.fill(0);
        }
    }
    // The keys a WPA2 join keeps for the rekeys to come live inside the station now
    // (`Station::forget_keys` drops them at every end of an association).
    /// What a pull saw, applied to the driver's memory of the join - here, so the frame module need not
    /// know what a join is.
    /// Returns whether the pull saw the access point drop a link this driver held (`rejoin_after_drop`).
    fn note_pull(
        session: &mut dyn Station,
        p: &Pulled,
        joined: &mut Option<([u8; SSID_MAX], u8)>,
        rekey_seen: &mut u32,
        who: &str,
        ctx: &ServiceContext,
    ) -> bool {
        let mut dropped = false;
        if let Some((event, reason)) = p.dropped_link {
            if joined.is_some() {
                dropped = true;
                say_fmt(ctx, who, format_args!(
                    "the access point dropped the link (event {} - {}, reason {}) - not joined",
                    event, session.event_name(event), reason
                ));
            }
            *joined = None;
            session.forget_keys();
        }
        if p.pairwise_failed > 0 {
            // The pull answered a restarted four-way handshake and it did not complete; the step is in
            // the log above. The access point decides what happens to the link next, and if it drops it
            // the pull sees that too.
            *rekey_seen = rekey_seen.wrapping_add(p.pairwise_failed);
            say(ctx, who, "a pairwise rekey did not complete - if the access point drops the link, the driver rejoins it once");
        }
        dropped
    }

    // THE CREDENTIAL SLOTS (`utilities/56_wifi.md` 6): a network name and the pairwise master key derived
    // from its passphrase, sixty-four of them. The passphrase itself is gone the moment the key exists. When
    // every slot is held, the one JOINED LONGEST AGO is replaced. This table is the WORKING SET; the 48 most
    // recent keys and their names are also on disk in `/wifi.keys` (`godspeed_wifi::keyfile`), loaded when
    // the radio comes up and rewritten after every change.
    // About 80 bytes each (88 as an `Option`), some 5.5 KiB in all, against a service limit of 8 MiB or more: the count is a BOUND (26.6), chosen
    // so that nobody reaches it, not a fit to the memory - a table that grew to fill what is available is
    // the elastic growth 26.6.1 says to resist.
    struct Stored {
        ssid: [u8; SSID_MAX],
        len: u8,
        pmk: [u8; crypto::PMK_LEN],
        /// When this key was last used to join (or was stored), monotonic seconds - the replacement order.
        used_at: i64,
    }
    const CREDENTIAL_SLOTS: usize = 64;
    let mut stored: [Option<Stored>; CREDENTIAL_SLOTS] = core::array::from_fn(|_| None);
    /// The slot holding this name, if any.
    fn slot_of(stored: &[Option<Stored>], ssid: &[u8]) -> Option<usize> {
        stored.iter().position(|s| {
            s.as_ref().map(|st| st.len as usize == ssid.len() && &st.ssid[..ssid.len()] == ssid).unwrap_or(false)
        })
    }
    /// The NOTE byte of one record: is a key held for this name, and is it the network joined. What a person
    /// picking from the list most needs to know, and what only this loop knows.
    fn note_for<'a>(
        stored: &'a [Option<Stored>],
        joined: &'a Option<([u8; SSID_MAX], u8)>,
    ) -> impl Fn(&Network) -> u8 + 'a {
        move |n: &Network| {
            let name = &n.ssid[..n.ssid_len as usize];
            let mut note = 0;
            if slot_of(stored, name).is_some() {
                note |= wire::NOTE_SAVED;
            }
            if let Some((j, jl)) = joined {
                if *jl as usize == name.len() && &j[..name.len()] == name {
                    note |= wire::NOTE_JOINED;
                }
            }
            note
        }
    }
    /// Where a new key goes: the slot already holding this name, else a free one, else the one used longest ago.
    fn slot_for(stored: &[Option<Stored>], ssid: &[u8]) -> usize {
        if let Some(i) = slot_of(stored, ssid) {
            return i;
        }
        if let Some(i) = stored.iter().position(|s| s.is_none()) {
            return i;
        }
        let mut oldest = 0;
        for (i, s) in stored.iter().enumerate() {
            if let (Some(a), Some(b)) = (s.as_ref(), stored[oldest].as_ref()) {
                if a.used_at < b.used_at {
                    oldest = i;
                }
            }
        }
        oldest
    }

    // `crypto_ok`: the primitives that turn a passphrase into a key, checked against their published
    // vectors (`crypto::selftest`) by the caller, once - not on every entry, since a USB radio's service
    // comes back here on every replug. A wrong hash would be refused by every access point in a way
    // indistinguishable from a wrong passphrase, so if it failed, passphrases are refused HERE, with the
    // reason, rather than there, without one.
    // Requests that could not be answered, and answers that could not be delivered - both loud.
    let mut capless: u32 = 0;
    let mut reply_failed: u32 = 0;

    loop {
        if !keyfile_settled && radio.is_some() {
            // The choice first, so the auto-join below knows whether it is this radio's to make.
            if !choice_settled {
                match keyfile::load_choice(ctx) {
                    keyfile::Choice::Named(name, len) => {
                        choice_settled = true;
                        let mine = wire::radio_name(who).as_bytes() == &name[..len];
                        choice_use = if mine { wire::USE_THIS } else { wire::USE_NOT };
                        say_fmt(ctx, who, format_args!("/wifi.radio names {} - {}",
                            core::str::from_utf8(&name[..len]).unwrap_or("?"),
                            if mine { "this radio is the one in use" } else { "another radio is in use; this one does not rejoin" }));
                    }
                    keyfile::Choice::None => {
                        choice_settled = true;
                        choice_use = wire::USE_DEFAULT;
                    }
                    keyfile::Choice::Unreachable => {}
                }
            }
            let mut entries = [keyfile::Entry::EMPTY; keyfile::MAX_SAVED];
            match keyfile::load(ctx, who, &mut entries) {
                keyfile::Load::Loaded(n) => {
                    keyfile_settled = true;
                    let now = gs::task::epoch_secs_monotonic(ctx);
                    for (i, e) in entries.iter().take(n).enumerate() {
                        let ssid = &e.ssid[..e.len as usize];
                        let slot = slot_for(&stored, ssid);
                        // Most recent first in the file, so the first keeps the highest `used_at`.
                        stored[slot] = Some(Stored { ssid: e.ssid, len: e.len, pmk: e.pmk, used_at: now - i as i64 });
                    }
                    say_fmt(ctx, who, format_args!("/wifi.keys loaded - {} network(s) known", n));
                    if n > 0 {
                        auto_join = Some((entries[0].ssid, entries[0].len, entries[0].sec));
                        auto_join_why = "from /wifi.keys";
                    }
                }
                keyfile::Load::NoFile => {
                    keyfile_settled = true;
                    say(ctx, who, "no /wifi.keys - nothing to rejoin; the first join writes it");
                }
                keyfile::Load::Unreachable => {
                    keyfile_tries += 1;
                    if keyfile_tries >= KEYFILE_TRIES {
                        keyfile_settled = true;
                        say_fmt(ctx, who, format_args!(
                            "fs could not give /wifi.keys in {} tries (no answer, or its storage unavailable) - running on the table in memory alone this boot",
                            keyfile_tries
                        ));
                    }
                }
            }
            for e in entries.iter_mut() {
                e.pmk.fill(0);
            }
        }
        if let (Some((name, len, sec)), Some(session)) = (auto_join.take(), radio.as_deref_mut()) {
            let mut go = choice_use != wire::USE_NOT;
            if !go {
                standby = Some((name, len, sec));
                match other_present {
                    Some(false) => {
                        say(ctx, who, "the radio /wifi.radio chooses is not attached - this one rejoins in its place, and leaves when it arrives");
                        standing_in = true;
                        go = true;
                    }
                    Some(true) => say(ctx, who, "not rejoining the network last joined - another radio is the one in use (/wifi.radio)"),
                    None => if !said_waiting {
                        said_waiting = true;
                        say(ctx, who, "not rejoining yet - /wifi.radio chooses another radio, and whether it is attached is the supervisor's to say");
                    },
                }
            }
            if go && radio_on && sweep.is_none() && joined.is_none() {
                say_fmt(ctx, who, format_args!("joining the network last joined, {}", auto_join_why));
                match join_known(session, &name, len, sec, &mut stored, &mut joined,
                                 &mut joined_at_secs, &mut joined_security, &mut rxq, ctx) {
                    Some(Outcome::Joined) => {
                        last_joined = Some((name, len, sec));
                        save_keys(ctx, who, &stored, None);
                    }
                    Some(_) => say(ctx, who, "the network last joined did not take us back - not joined; `wifi join` when it is in range"),
                    None => {}
                }
            }
        }
        // ---- 1. One frame of the running sweep, if there is one. ----
        if let (Some(s), Some(session)) = (sweep.as_mut(), radio.as_deref_mut()) {
            match session.scan_step(&mut s.scan, ctx) {
                ScanStep::Frame => {}
                ScanStep::Empty => {
                    s.empty += 1;
                    gs::task::sleep_ms(ctx, 1);
                    if s.empty >= session.scan_empty_bound() {
                        say_fmt(ctx, who, format_args!(
                            "the sweep fell silent for {} empty polls without the firmware saying it \
                             was over - discarded ({} heard); the last complete scan stands",
                            s.empty,
                            s.scan.count()
                        ));
                        sweep = None;
                        sweep_failed = true;
                    }
                }
                ScanStep::Ended(why) => {
                    let done = sweep.take().unwrap_or(Sweep { scan: Scan::new(), empty: 0 });
                    say_fmt(ctx, who, format_args!(
                        "sweep complete - {} network(s), ended by {}",
                        done.scan.count(),
                        why
                    ));
                    cache = Some(Cache { scan: done.scan, at_secs: gs::task::epoch_secs_monotonic(ctx) });
                    sweep_failed = false;
                }
            }
        }

        // ---- 2. A request. Blocking when idle - there is nothing else to do - and a look when sweeping. ----
        let req = if sweep.is_some() {
            match gs::ipc::try_recv(ctx) {
                Some(m) => m,
                None => continue,
            }
        } else if !keyfile_settled {
            // `fs` may still be mounting when the radio comes up: wait for a request, but not forever, so
            // the load above gets its next try.
            match gs::ipc::recv_within_ms(ctx, 2_000) {
                Some(m) => m,
                None => continue,
            }
        } else if let (true, true, Some(session)) = (joined.is_some(), radio_on, radio.as_deref_mut()) {
            // Joined: wait, but not forever, so a radio nobody reads is read here (`IDLE_PULL_MS`).
            match gs::ipc::recv_within_ms(ctx, IDLE_PULL_MS) {
                Some(m) => m,
                None => {
                    if last_frame_op.passed(ctx, Budget::ms(IDLE_PULL_MS)) {
                        rxq.clear();
                        let p = session.pull(&mut rxq, ctx);
                        if note_pull(session, &p, &mut joined, &mut rekey_seen, who, ctx) {
                            auto_join = rejoin_after_drop(ctx, who, joined_at_secs, last_joined);
                            auto_join_why = "after the access point dropped it";
                        }
                        rxq.clear();
                        if p.data > 0 && idle_discarded == 0 {
                            say(ctx, who, "nothing is taking received frames - the loop reads the radio itself every 250 ms and discards them, so the chip never fills and group rekeys are answered");
                        }
                        idle_discarded = idle_discarded.saturating_add(p.data);
                        idle_reads = idle_reads.saturating_add(1);
                        if idle_said_at.passed(ctx, Budget::ms(60_000)) {
                            idle_said_at = Since::now(ctx);
                            say_fmt(ctx, who, format_args!(
                                "still joined - the loop read the radio {} time(s) this minute; {} frame(s) discarded unread this boot",
                                idle_reads, idle_discarded));
                            idle_reads = 0;
                        }
                        if p.rekeyed > 0 {
                            say_fmt(ctx, who, format_args!(
                                "group rekey answered by the loop's own read ({} frame(s) discarded unread so far)",
                                idle_discarded));
                        }
                    }
                    continue;
                }
            }
        } else {
            gs::ipc::recv(ctx)
        };
        let reply = match gs::ipc::take_sent_cap(ctx) {
            Some(r) => r,
            // A message with no reply cap is the HOST's: a USB host telling `wifi-usb` that a received
            // transfer is waiting, or that the dongle was bound or removed. The host takes it, and says
            // whether the radio under this loop changed - which ends it, so the caller can bring the new
            // one up. A radio on the board has no such notices and says `Ignored` to all of them.
            // The supervisor saying whether the USB dongle is attached (`wire::NOTE_USB_RADIO`): the fact a
            // radio the choice does not name stands in, or stands down, on.
            None if matches!(req.payload_bytes(), [wire::NOTE_USB_RADIO, _]) => {
                let attached = req.payload_bytes()[1] != 0;
                if other_present != Some(attached) {
                    say(ctx, who, if attached { "the USB radio is attached (the supervisor)" } else { "the USB radio is not attached (the supervisor)" });
                }
                other_present = Some(attached);
                if attached && standing_in {
                    // STAND DOWN: the chosen radio is here, and it rejoins on its own.
                    standing_in = false;
                    if let Some(session) = radio.as_deref_mut() {
                        if joined.is_some() {
                            let _ = session.disassoc(ctx);
                            joined = None;
                            session.forget_keys();
                            rxq.clear();
                        }
                    }
                    say(ctx, who, "the radio /wifi.radio chooses is attached - this one leaves the network it held in its place");
                } else if !attached && choice_use == wire::USE_NOT && !standing_in && joined.is_none() {
                    // STAND IN: the chosen radio is absent, so the rejoin this radio held back is made now.
                    auto_join = standby;
                }
                continue;
            }
            None => match host.notice(req.payload_bytes(), sweep.as_mut().map(|s| &mut s.scan), ctx) {
                Notice::Taken => continue,
                Notice::Changed => return,
                Notice::Ignored => {
                // No cap to answer on. Once, this was the WHOLE failure of the first frame-path boot:
                // reply caps were never reclaimed (below), the 64-slot table filled after some fifty
                // requests, the kernel could install no more, and every request after that landed here
                // and was dropped without a word - `net-stack` saw a radio that "stopped responding
                // after a while", `observe` saw this driver idle with an empty queue. Counted and said.
                capless = capless.saturating_add(1);
                if capless == 1 || capless % 64 == 0 {
                    say_fmt(ctx, who, format_args!(
                        "a request arrived with no reply cap - dropped (x{}); if this repeats, the cap table is full and every answer is being lost",
                        capless
                    ));
                }
                continue;
                }
            },
        };
        let (tag, payload) = untag(req.payload_bytes());
        let op = payload.first().copied().unwrap_or(0);
        // How long this request takes to serve, so a slow one is named from THIS side too: the shell and
        // `nic-driver` both bound their waits, and a driver that quietly took three seconds over a sweep
        // start (boot 2026-09-30 14:51) left neither of them able to say where the time went.
        let served_t0 = Since::now(ctx);
        let n = match (op, radio.as_deref_mut()) {
            // What this radio is, before every other arm: true down, powered off or up (`wifi hardware`).
            (wire::OP_HARDWARE, _) => {
                let hw = host.hardware();
                let mut at = 1;
                out[0] = wire::OK;
                for text in [hw.chip, hw.bus] {
                    let b = text.as_bytes();
                    let len = b.len().min(wire::HW_TEXT_MAX);
                    out[at] = len as u8;
                    out[at + 1..at + 1 + len].copy_from_slice(&b[..len]);
                    at += 1 + len;
                }
                at
            }
            // The radio the operator chose (`wire::OP_USE`): asked, or told. Answered in any state, since a
            // radio that is down must still know it is not the one to rejoin.
            (wire::OP_USE, _) => {
                if let Some(&len) = payload.get(1) {
                    // A name over 16 bytes is no radio's, as `keyfile` reads `/wifi.radio`: it was cut
                    // to 16 and compared, so a long name that began like this radio's chose it
                    // (`backlog/80` S8).
                    let long = len as usize > 16;
                    let len = (len as usize).min(16);
                    let name = payload.get(2..2 + len).unwrap_or(&[]);
                    choice_use = if long {
                        wire::USE_NOT
                    } else if name.is_empty() {
                        wire::USE_DEFAULT
                    } else if name == wire::radio_name(who).as_bytes() {
                        wire::USE_THIS
                    } else {
                        wire::USE_NOT
                    };
                    choice_settled = true;
                    // Chosen now, or the default: no longer anyone's stand-in.
                    if choice_use != wire::USE_NOT {
                        standing_in = false;
                    }
                }
                out[0] = wire::OK;
                out[1] = if standing_in { wire::USE_STANDIN } else { choice_use };
                2
            }
            // One radio in full (`wifi hardware <radio>`): the host's facts, then the station's, and for a
            // radio with no station the reason its readings are missing. The station may ask the chip only
            // when `OP_DEBUG` may: not mid-sweep, not with the radio off, not powered down.
            (wire::OP_HARDWARE_DETAIL, r) => {
                let live = sweep.is_none() && radio_on && !powered_off;
                out[0] = wire::OK;
                let (count, len) = {
                    let mut d = Details::new(&mut out[2..]);
                    host.details(&mut d);
                    match r {
                        Some(station) => station.details(&mut d, live, ctx),
                        None => d.add("firmware", format_args!("not running - the radio is down")),
                    }
                    if powered_off && !d.has("firmware") {
                        d.add("firmware", format_args!("not running - the chip is powered down"));
                    }
                    d.done()
                };
                out[1] = count;
                2 + len
            }
            // ---- Powered down (`wifi radio off hard`): these three arms come before every arm that talks
            // to the chip, so nothing below talks to a chip that has no power. ----
            (wire::OP_STATUS, _) if powered_off => {
                // The status shape the shell knows, all zero: radio off, not associated, no cache claimed,
                // and the trailing power byte at 0 - the one fact that tells this off from the soft one.
                out[..30 + SSID_MAX].fill(0);
                out[0] = wire::OK;
                30 + SSID_MAX
            }
            (wire::OP_RADIO, _) if powered_off => {
                let mode = payload.get(1).copied().unwrap_or(1);
                out[1] = 0;
                out[3] = 0;
                out[4] = 0;
                if mode == 0 || mode == wire::RADIO_HARD_OFF {
                    // Already as asked.
                    out[0] = wire::OK;
                    out[2] = 0;
                } else if host.restore_power(ctx) {
                    powered_off = false;
                    say(ctx, who, "`wifi radio on` on a powered-down chip - the power is restored; this instance has no firmware to serve and expects to be restarted onto the cold chip");
                    out[0] = wire::OK;
                    out[2] = 1;
                    out[3] = wire::COLD_START;
                } else {
                    say(ctx, who, "`wifi radio on` on a powered-down chip, and the kernel refused to restore the power");
                    out[0] = wire::NO_POWER_CONTROL;
                    out[2] = 0;
                }
                5
            }
            (_, Some(_)) if powered_off => {
                out[0] = wire::RADIO_POWERED_OFF;
                1
            }
            (wire::OP_RADIO, Some(_)) if payload.get(1).copied() == Some(wire::RADIO_HARD_OFF) && !host.can_cut_power() => {
                // No power to cut here: say so and change NOTHING - in particular, do not leave the network.
                say(ctx, who, "`wifi radio off hard` asked, and this radio's power is not this driver's to cut - nothing changed, still on the network");
                out[0] = wire::NO_POWER_CONTROL;
                out[1] = 0;
                out[2] = 0;
                out[3] = 0;
                out[4] = 0;
                5
            }
            (wire::OP_RADIO, Some(session)) if payload.get(1).copied() == Some(wire::RADIO_HARD_OFF) => {
                // `wifi radio off hard`: leave the network politely while the firmware can still send,
                // then cut the chip's power and stay that way. The keys held in memory go with the
                // association; `/wifi.keys` stays, and the cold start after `on` rejoins from it.
                let was_joined = joined.is_some();
                if let Some(s) = sweep.take() {
                    let _ = session.scan_abort(ctx);
                    say_fmt(ctx, who, format_args!(
                        "hard off mid-sweep - the sweep is stopped ({} heard, not kept)",
                        s.scan.count()));
                }
                if was_joined {
                    let _ = session.disassoc(ctx);
                }
                joined = None;
                session.forget_keys();
                out[1] = was_joined as u8;
                out[3] = 0;
                out[4] = 0;
                if host.cut_power(ctx) {
                    // Quiet for as long as the power stays off - and while it is off nothing in this loop
                    // touches the bus after the one check below that the cut took (`Host::verify_off`):
                    // auto-join needs `radio_on`, the sweep was stopped above, and every request is
                    // answered from the powered-off arms (docs/wifi.md 48).
                    powered_off = true;
                    radio_on = false;
                    say(ctx, who, "`wifi radio off hard` - the chip's power is cut and stays cut until `wifi radio on`");
                    out[0] = wire::OK;
                    out[2] = 1;
                    out[3] = host.verify_off(ctx);
                } else {
                    say(ctx, who, "`wifi radio off hard` asked, and this machine has no control over the radio's power - the radio stays as it was");
                    out[0] = wire::NO_POWER_CONTROL;
                    out[2] = 0;
                }
                5
            }
            // THE RADIO IS DOWN (no session) AND THE CHIP'S POWER IS STILL THIS DRIVER'S TO COMMAND. The
            // power ops are served here, because they are the way out of exactly this state: a firmware
            // that trapped at start leaves the radio down, and a `powercycle` or `off hard` typed then
            // used to be answered RADIO_DOWN - which the shell read, wrongly, as "no control over the
            // radio's power" (boot 2026-10-01 09:48). Everything else below stays "radio down".
            (wire::OP_RADIO, None) if payload.get(1).copied() == Some(wire::RADIO_POWERCYCLE) => {
                let cycled = host.power_cycle(ctx, payload.get(2).copied().unwrap_or(0));
                say(ctx, who, if cycled {
                    "`wifi radio powercycle` on a radio that is down - the chip's power was cut and restored; this instance expects to be restarted onto the cold chip"
                } else {
                    "`wifi radio powercycle` on a radio that is down, and this machine has no control over the radio's power - nothing changed"
                });
                out[0] = if cycled { wire::OK } else { wire::NO_POWER_CONTROL };
                out[1] = 0;
                out[2] = cycled as u8;
                out[3] = 0;
                out[4] = 0;
                5
            }
            (wire::OP_RADIO, None) if payload.get(1).copied() == Some(wire::RADIO_HARD_OFF) => {
                if host.cut_power(ctx) {
                    powered_off = true;
                    radio_on = false;
                    say(ctx, who, "`wifi radio off hard` on a radio that is down - the chip's power is cut and stays cut until `wifi radio on`");
                    out[0] = wire::OK;
                    out[2] = 1;
                    out[3] = host.verify_off(ctx);
                } else {
                    out[0] = wire::NO_POWER_CONTROL;
                    out[2] = 0;
                    out[3] = 0;
                }
                out[1] = 0;
                out[4] = 0;
                5
            }
            (_, None) => {
                // Byte 1 says WHY (wire::DOWN_*), so `wifi status` can tell a firmware that
                // trapped from a bring-up that stopped, instead of guessing.
                out[0] = wire::RADIO_DOWN;
                out[1] = down_reason;
                2
            }
            (wire::OP_LIST, Some(_)) => match (&sweep, &cache) {
                (Some(s), _) => {
                    out[0] = wire::SCANNING;
                    out[1] = s.scan.count() as u8;
                    2
                }
                (None, Some(c)) => {
                    let note = note_for(&stored, &joined);
                    write_reply(&c.scan, &note, &mut out)
                }
                (None, None) => {
                    out[0] = wire::NO_SCAN_YET;
                    1
                }
            },
            // A powered-off radio cannot sweep or join; the cache and the status are still served.
            (wire::OP_SCAN_START, Some(_)) | (wire::OP_CONNECT, Some(_)) if !radio_on => {
                out[0] = wire::RADIO_OFF;
                1
            }
            (wire::OP_DISCONNECT, Some(session)) => {
                if let Some(s) = sweep.take() {
                    let _ = session.scan_abort(ctx);
                    say_fmt(ctx, who, format_args!(
                        "a disconnect was asked for mid-sweep - the sweep is stopped ({} heard, not kept)",
                        s.scan.count()
                    ));
                }
                let left = joined;
                // Sent whether or not this driver believes it is associated: the firmware's state is the truth,
                // and a stale belief here must not stop the operator leaving a network.
                let _ = session.disassoc(ctx);
                joined = None;
                last_joined = None;
                session.forget_keys();
                rxq.clear();
                // `[OK, was_joined, len, name[32]]` - the name of what was left, so the shell can say it.
                out[0] = wire::OK;
                out[1] = left.is_some() as u8;
                out[2] = 0;
                out[3..3 + SSID_MAX].fill(0);
                if let Some((name, len)) = left {
                    out[2] = len;
                    out[3..3 + SSID_MAX].copy_from_slice(&name);
                }
                3 + SSID_MAX
            }
            // `wifi radio powercycle`: the CHIP's power, not the firmware's radio switch. The operator's form of
            // the recovery this driver does for itself when it finds a firmware it cannot adopt
            // (docs/wifi.md 47). The power is cut and restored by the host (`Host::power_cycle`: the
            // kernel's `DevicePower` on the Pi 4 and the VisionFive, a register power-down on the USB
            // dongle); what follows is the SHELL's, because it holds restart authority: it kills this
            // instance, and the respawn finds a cold chip - the boot's own path.
            // The reply goes out before the kill arrives, so the operator is told the cycle happened (or
            // that this machine cannot do it) rather than left with a prompt that went quiet.
            (wire::OP_RADIO, Some(session)) if payload.get(1).copied() == Some(wire::RADIO_POWERCYCLE) => {
                let was_joined = joined.is_some();
                let cycled = host.power_cycle(ctx, payload.get(2).copied().unwrap_or(0));
                if cycled {
                    say(ctx, who, "`wifi radio powercycle` - the chip's power was cut and restored; this instance has no firmware to serve and expects to be restarted onto the cold chip");
                    joined = None;
                    last_joined = None;
                    session.forget_keys();
                    radio_on = false;
                } else {
                    say(ctx, who, "`wifi radio powercycle` asked, and this machine has no control over the radio's power - nothing changed");
                }
                out[0] = if cycled { wire::OK } else { wire::NO_POWER_CONTROL };
                out[1] = was_joined as u8;
                out[2] = cycled as u8;
                out[3] = 0;
                out[4] = 0;
                5
            }
            (wire::OP_RADIO, Some(session)) => {
                // Reply: `[status, was_joined, changed, verdict_or_rejoin, len, name[32]]` (`wire::OP_RADIO`).
                // `changed` is 0 when the radio was already in the
                // state asked for - and then NOTHING is sent to the firmware, because a DOWN to a radio that
                // is down is not a no-op on every firmware and an UP chain re-run resets a live interface.
                let on = payload.get(1).copied().unwrap_or(1) != 0;
                let was_joined = joined.is_some();
                out[2] = (on != radio_on) as u8;
                // Bytes 0-4 are written on EVERY path; the name bytes after them are written only by a
                // rejoin, and otherwise `len` (byte 4) is 0 so none is read. `out` is one buffer reused for
                // every request, and the "already on" branch left byte 3 - the rejoin status - holding
                // whatever the previous reply put there. The shell read it as a rejoin outcome it had no
                // words for. Boot 2026-09-30 15:17: `radio already on` followed by "a reply this shell
                // does not understand".
                out[3] = 0;
                out[4] = 0;
                if on {
                    if !radio_on {
                        if !session.radio_up(ctx) {
                            say(ctx, who, "the radio would not come back up - it stays off");
                            out[0] = wire::JOIN_FAILED;
                            out[1] = 0;
                            // The status byte names a refused command; the shell says so in its own words.
                        } else {
                            radio_on = true;
                            out[0] = wire::OK;
                            out[1] = 0;
                            // BACK ON THE NETWORK IT WAS ON. Reply bytes 3.. carry the rejoin: `[status,
                            // ssid_len, ssid[32]]`, status 0 when there was nothing to rejoin. The key
                            // is the held one - the passphrase is never asked for here - and an open
                            // network is rejoined open. A `forget` of the name leaves nothing to rejoin
                            // with, and the reply says so by attempting nothing.
                            if let Some((name, len, sec)) = last_joined {
                                say(ctx, who, "radio back on - rejoining the network last joined");
                                if let Some(outcome) = join_known(session, &name, len, sec, &mut stored,
                                                                  &mut joined, &mut joined_at_secs, &mut joined_security,
                                                                  &mut rxq, ctx) {
                                    out[3] = reply_of(outcome);
                                    out[4] = len;
                                    out[5..5 + SSID_MAX].copy_from_slice(&name);
                                    if outcome == Outcome::Joined {
                                        save_keys(ctx, who, &stored, None);
                                    }
                                }
                            }
                        }
                    } else {
                        out[0] = wire::OK;
                        out[1] = 0;
                    }
                } else if !radio_on {
                    // Already off: nothing to stop, nothing to send.
                    out[0] = wire::OK;
                    out[1] = 0;
                } else {
                    // `off` disconnects first (`utilities/56_wifi.md` 2), then takes the interface down.
                    if let Some(s) = sweep.take() {
                        let _ = session.scan_abort(ctx);
                        say_fmt(ctx, who, format_args!(
                            "radio off mid-sweep - the sweep is stopped ({} heard, not kept)",
                            s.scan.count()
                        ));
                    }
                    if was_joined {
                        let _ = session.disassoc(ctx);
                    }
                    joined = None;
                    session.forget_keys();
                    if session.radio_down(ctx) {
                        radio_on = false;
                        out[0] = wire::OK;
                        // VERIFIED, as `on` is: ask the firmware whether it is still up.
                        out[3] = match session.is_up(ctx) {
                            Some(false) => wire::OFF_VERIFIED,
                            Some(true) => {
                                say(ctx, who, "`wifi radio off` - DOWN was accepted but the firmware still says it is up");
                                wire::OFF_CONTRADICTED
                            }
                            None => {
                                say(ctx, who, "`wifi radio off` - the firmware did not answer whether it is up, so the off is unverified");
                                wire::OFF_UNVERIFIED
                            }
                        };
                    } else {
                        say(ctx, who, "the firmware refused DOWN - the radio stays on");
                        out[0] = wire::JOIN_FAILED;
                    }
                    out[1] = was_joined as u8;
                }
                5 + SSID_MAX
            }
            (wire::OP_SCAN_START, Some(session)) => match &sweep {
                Some(s) => {
                    // Already sweeping: the caller attaches to it rather than starting a second - the radio
                    // has one sweep in it at a time.
                    out[0] = wire::SCANNING;
                    out[1] = s.scan.count() as u8;
                    2
                }
                None => {
                    if session.scan_start(ctx) {
                        sweep = Some(Sweep { scan: Scan::new(), empty: 0 });
                        sweep_failed = false;
                        out[0] = wire::OK;
                        out[1] = 0;
                        2
                    } else {
                        out[0] = wire::SCAN_FAILED;
                        1
                    }
                }
            },
            (wire::OP_SCAN_POLL, Some(_)) => {
                let from = payload.get(1).copied().unwrap_or(0) as usize;
                match (&sweep, &cache) {
                    (Some(s), _) => {
                        let note = note_for(&stored, &joined);
                        write_records(&s.scan, from, wire::SCANNING, &note, &mut out)
                    }
                    (None, _) if sweep_failed => {
                        out[0] = wire::SCAN_FAILED;
                        1
                    }
                    (None, Some(c)) => {
                        let note = note_for(&stored, &joined);
                        write_records(&c.scan, from, wire::SCAN_DONE, &note, &mut out)
                    }
                    (None, None) => {
                        out[0] = wire::NO_SCAN_YET;
                        1
                    }
                }
            }
            (wire::OP_SCAN_ABORT, Some(session)) => {
                let heard = match sweep.take() {
                    Some(s) => {
                        if !session.scan_abort(ctx) {
                            say(ctx, who, "the firmware did not take the abort - the sweep's events will be drained and discarded as they arrive");
                        }
                        say_fmt(ctx, who, format_args!(
                            "sweep stopped by request - {} heard, not kept",
                            s.scan.count()
                        ));
                        s.scan.count()
                    }
                    None => 0,
                };
                out[0] = wire::OK;
                out[1] = heard as u8;
                2
            }
            (wire::OP_STATUS, Some(session)) => {
                // Reply: `[OK, sweeping, heard, has_cache, cache_count, age u32, radio_on, associated,
                //          bssid[6], rssi i32, chanspec u16, security, joined_secs u32, ssid_len, ssid[32]]`.
                out[0] = wire::OK;
                out[1] = sweep.is_some() as u8;
                out[2] = sweep.as_ref().map(|s| s.scan.count()).unwrap_or(0) as u8;
                out[3] = cache.is_some() as u8;
                out[4] = cache.as_ref().map(|c| c.scan.count()).unwrap_or(0) as u8;
                let age = cache
                    .as_ref()
                    .map(|c| (gs::task::epoch_secs_monotonic(ctx) - c.at_secs).max(0) as u32)
                    .unwrap_or(u32::MAX);
                out[5..9].copy_from_slice(&age.to_le_bytes());
                out[9] = radio_on as u8;

                // THE LINK IS READ, NOT REMEMBERED - but not while a sweep runs: a control exchange reads
                // frames off the bus and skips the ones that are not its reply, which mid-sweep would be the
                // scan's own results. During a sweep the status is the driver's memory, and says the sweep is
                // running, which is the fact that matters then.
                let link = if radio_on && sweep.is_none() { session.link(ctx) } else { None };
                let (assoc, bssid, rssi, chanspec) = match &link {
                    Some(l) if l.associated() => (true, l.bssid, l.rssi, l.chanspec),
                    Some(_) => {
                        if joined.is_some() {
                            say(ctx, who, "the firmware reports no association - the remembered join is dropped");
                            joined = None;
                            session.forget_keys();
                        }
                        (false, [0u8; 6], 0, 0)
                    }
                    None => (joined.is_some(), [0u8; 6], 0, 0),
                };
                out[10] = assoc as u8;
                out[11..17].copy_from_slice(&bssid);
                out[17..21].copy_from_slice(&rssi.to_le_bytes());
                out[21..23].copy_from_slice(&chanspec.to_le_bytes());
                out[23] = joined_security;
                let since = if assoc { (gs::task::epoch_secs_monotonic(ctx) - joined_at_secs).max(0) as u32 } else { 0 };
                out[24..28].copy_from_slice(&since.to_le_bytes());
                match &joined {
                    Some((ssid, len)) => {
                        out[28] = *len;
                        out[29..29 + SSID_MAX].copy_from_slice(ssid);
                    }
                    None => {
                        out[28] = 0;
                        out[29..29 + SSID_MAX].fill(0);
                    }
                }
                // Trailing power byte: 1 here, because this arm is only reached with the chip powered. The
                // powered-down status arm above writes 0. The shell reads it to say which off this is.
                out[29 + SSID_MAX] = 1;
                30 + SSID_MAX
            }
            (wire::OP_CONNECT, Some(session)) => {
                // A join and a sweep cannot share the radio. The sweep goes, and says so; the cache stays.
                if let Some(s) = sweep.take() {
                    let _ = session.scan_abort(ctx);
                    say_fmt(ctx, who, format_args!(
                        "a join was asked for mid-sweep - the sweep is stopped ({} heard, not kept)",
                        s.scan.count()
                    ));
                }
                // `[op, ssid_len, ssid[32], pass_len, pass[64]]`. Lengths are checked against the fixed
                // fields, and the passphrase bytes are used from the request buffer and never copied
                // anywhere that outlives this arm.
                const SSID_LEN_AT: usize = 1;
                const SSID_AT: usize = 2;
                const PASS_LEN_AT: usize = 2 + SSID_MAX;
                const PASS_AT: usize = PASS_LEN_AT + 1;
                const TOTAL: usize = PASS_AT + PASS_MAX;
                if payload.len() < TOTAL {
                    out[0] = wire::JOIN_FAILED;
                    1
                } else {
                    let ssid_len = core::cmp::min(payload[SSID_LEN_AT] as usize, SSID_MAX);
                    let pass_len = core::cmp::min(payload[PASS_LEN_AT] as usize, PASS_MAX);
                    let ssid = &payload[SSID_AT..SSID_AT + ssid_len];
                    let pass = &payload[PASS_AT..PASS_AT + pass_len];

                    // WHAT TO JOIN WITH, decided in this order and never guessed:
                    //  1. a passphrase in the request: derive the key, keep it in the slot (replacing whatever
                    //     was there), and join with it. The passphrase bytes live in `req`, which the next
                    //     `recv` overwrites; nothing copies them.
                    //  2. no passphrase, and the slot holds this name: join with the stored key.
                    //  3. no passphrase, and the last sweep heard this name as OPEN: join open.
                    //  4. otherwise: NEEDS_PASSPHRASE - the shell asks and sends again.
                    let name_of = |ssid: &[u8]| {
                        let mut name = [0u8; SSID_MAX];
                        name[..ssid.len()].copy_from_slice(ssid);
                        name
                    };
                    // ALREADY ON IT? Asked of the firmware, not remembered: `joined` names the network and
                    // `Station::link` (the firmware's or the station's own reading) says whether the link is
                    // still up. A join of the network we are on sends
                    // nothing and says so; a stale memory of one is cleared and the join proceeds.
                    let on_this = joined
                        .as_ref()
                        .map(|(j, jl)| *jl as usize == ssid_len && &j[..ssid_len] == ssid)
                        .unwrap_or(false);
                    let already = on_this
                        && matches!(session.link(ctx), Some(l) if l.associated());
                    if on_this && !already {
                        say(ctx, who, "the remembered join is not on the air any more - joining afresh");
                        joined = None;
                    }
                    let now = gs::task::epoch_secs_monotonic(ctx);
                    // A KEY IS KEPT ONLY ONCE IT HAS JOINED. A passphrase typed now is derived into `pmk_buf`
                    // and enters the table after `JOINED`, not on arrival. The first hardware run of the
                    // handshake kept it on arrival, so a join that failed left behind a key nothing had
                    // proved, and the next `wifi join` used it without asking - the operator had to `forget`
                    // between every attempt. A key already in the table has joined before; it is used from
                    // there, and dropped only when the network refuses it as incorrect (its passphrase changed).
                    let mut pmk_buf = [0u8; crypto::PMK_LEN];
                    let mut fresh = false;
                    let mut use_slot: Option<usize> = None;
                    if !already && pass_len > 0 && crypto_ok {
                        pmk_buf = crypto::psk(pass, ssid);
                        fresh = true;
                        say(ctx, who, "pairwise master key derived from the passphrase - kept once it has joined");
                    } else if pass_len == 0 {
                        if let Some(i) = slot_of(&stored, ssid) {
                            say_fmt(ctx, who, format_args!("joining with the key in credential slot {}", i));
                            // A match rather than an unwrap: a service never halts (Commandment V).
                            if let Some(st) = stored[i].as_mut() {
                                pmk_buf = st.pmk;
                                st.used_at = now;
                            }
                            use_slot = Some(i);
                        }
                    }
                    let secret = if pass_len > 0 {
                        if fresh {
                            Some(Secret::Pmk(&pmk_buf))
                        } else {
                            say(ctx, who, "a passphrase arrived and the key derivation failed its self-test at boot - refused");
                            None
                        }
                    } else if use_slot.is_some() {
                        Some(Secret::Pmk(&pmk_buf))
                    } else if cache
                        .as_ref()
                        .and_then(|c| c.scan.find(ssid))
                        .map(|n| n.security == sec::OPEN)
                        .unwrap_or(false)
                    {
                        say(ctx, who, "the last sweep heard this network as open - joining without a key");
                        Some(Secret::Open)
                    } else {
                        None
                    };

                    if already {
                        out[0] = wire::ALREADY_JOINED;
                        1
                    } else {
                    match secret {
                        None if pass_len > 0 => {
                            out[0] = wire::JOIN_FAILED;
                            1
                        }
                        None => {
                            out[0] = wire::NEEDS_PASSPHRASE;
                            1
                        }
                        Some(secret) => {
                            let outcome = session.join(ssid, secret, ctx);
                            if outcome == Outcome::Joined {
                                joined = Some((name_of(ssid), ssid_len as u8));
                                joined_at_secs = gs::task::epoch_secs_monotonic(ctx);
                                rxq.clear();
                                last_joined = Some((name_of(ssid), ssid_len as u8, if matches!(secret, Secret::Open) { sec::OPEN } else { sec::WPA2 }));
                                joined_security = if matches!(secret, Secret::Open) {
                                    sec::OPEN
                                } else {
                                    sec::WPA2
                                };
                                if fresh {
                                    let i = slot_for(&stored, ssid);
                                    let replaced = stored[i].is_some() && slot_of(&stored, ssid) != Some(i);
                                    stored[i] = Some(Stored { ssid: name_of(ssid), len: ssid_len as u8, pmk: pmk_buf, used_at: now });
                                    say_fmt(ctx, who, format_args!(
                                        "joined - the key is kept in credential slot {}{}",
                                        i,
                                        if replaced { " (replacing the one used longest ago)" } else { "" }
                                    ));
                                }
                                if !matches!(secret, Secret::Open) {
                                    save_keys(ctx, who, &stored, None);
                                }
                            } else {
                                joined = None;
                                if fresh {
                                    say(ctx, who, "the key from that passphrase is NOT kept - it did not join, so the next `wifi join` asks again");
                                } else if let Some(i) = use_slot {
                                    if matches!(outcome, Outcome::PassphraseRefused) {
                                        if let Some(st) = stored[i].as_mut() {
                                            st.pmk.fill(0);
                                            st.ssid.fill(0);
                                        }
                                        stored[i] = None;
                                        say_fmt(ctx, who, format_args!(
                                            "the key in credential slot {} was refused as incorrect - dropped; the next `wifi join` asks again",
                                            i
                                        ));
                                    }
                                }
                                // A join that failed AFTER association leaves the firmware on the network with
                                // no keys - and `Station::link` would then report it as joined, which the first
                                // hardware run of the handshake showed as `wifi status` saying `joined 5 min
                                // ago` to a `(hidden)` network after `wsec_key` was refused. The firmware's
                                // state is made to match this driver's answer. Harmless when there was no
                                // association to leave, exactly as `leave` is.
                                let _ = session.disassoc(ctx);
                            }
                            out[0] = reply_of(outcome);
                            // The working copy of the key does not outlive the join it was for.
                            pmk_buf.fill(0);
                            1
                        }
                    }
                    }
                }
            }
            (wire::OP_DEBUG, Some(session)) => {
                let sub = payload.get(1).copied().unwrap_or(wire::dbg::STATS);
                // Not mid-sweep and not with the radio off, for the reason `OP_STATUS` gives.
                session.debug(sub, sweep.is_none() && radio_on, &mut out, ctx)
            }
            (wire::OP_STORED, Some(_)) => {
                // `[OK, count, (len, ssid[32]) * count]` - names only, in slot order.
                out[0] = wire::OK;
                let mut count = 0u8;
                let mut at = 2;
                for st in stored.iter().flatten() {
                    out[at] = st.len;
                    out[at + 1..at + 1 + SSID_MAX].copy_from_slice(&st.ssid);
                    at += 1 + SSID_MAX;
                    count += 1;
                }
                out[1] = count;
                at
            }
            (wire::OP_FORGET, Some(_)) => {
                // `[10, len, ssid[32]]`.
                let len = core::cmp::min(payload.get(1).copied().unwrap_or(0) as usize, SSID_MAX);
                let dropped = match payload.get(2..2 + len).and_then(|ssid| slot_of(&stored, ssid)) {
                    Some(i) => {
                        // Zeroed, not just forgotten: the key must not linger in memory nothing points at.
                        if let Some(st) = stored[i].as_mut() {
                            st.pmk.fill(0);
                            st.ssid.fill(0);
                            st.len = 0;
                        }
                        stored[i] = None;
                        true
                    }
                    None => false,
                };
                // Saved whether or not THIS table held it: the file is shared with the other radio, which may
                // be the one that added it, and the merge would otherwise keep it (`keyfile::save`).
                save_keys(ctx, who, &stored, payload.get(2..2 + len));
                out[0] = wire::OK;
                out[1] = dropped as u8;
                2
            }
            // ---- THE FRAME INTERFACE (`wire::OP_NET_*`), served to `nic-driver` alongside the `wifi` ops. The
            // op numbers start at 0x10 for the reason `dwc2`'s do: they share an endpoint with another
            // protocol. Every reply is tagged with its op, because the caller bounds its wait and a late
            // answer must not be read as the next one. ----
            (wire::OP_NET_INFO, Some(session)) => {
                // `[op, ok, mac(6), link, peer(6), use]`. The address is the chip's, asked once; the link is
                // this driver's memory of the join, which every pull keeps honest. Not asked mid-sweep: a
                // control exchange would eat the sweep's frames, and a sweep is a moment of no link.
                //
                // `peer` is the access point the join reached. Our own address does not change when the
                // radio rejoins somewhere else, so without it `net-stack` cannot tell a rejoin to the same
                // access point (the lease still holds) from one to another, where it may not
                // (`docs/wifi.md` 60).
                out[0] = wire::OP_NET_INFO;
                if link_mac.is_none() && sweep.is_none() {
                    if let Some(mac) = session.mac(ctx) {
                        link_mac = Some(mac);
                    }
                }
                match link_mac {
                    Some(mac) => {
                        out[1] = 1;
                        out[2..8].copy_from_slice(&mac);
                    }
                    None => {
                        out[1] = 0;
                        out[2..8].fill(0);
                    }
                }
                out[8] = (radio_on && joined.is_some() && sweep.is_none()) as u8;
                if joined.is_none() {
                    link_peer = [0; 6];
                    link_peer_for = None;
                } else if link_peer_for != Some(joined_at_secs) && radio_on && sweep.is_none() {
                    if let Some(l) = session.link(ctx) {
                        if l.associated() {
                            link_peer = l.bssid;
                            link_peer_for = Some(joined_at_secs);
                        }
                    }
                }
                if link_peer_for == Some(joined_at_secs) {
                    out[9..15].copy_from_slice(&link_peer);
                } else {
                    out[9..15].fill(0);
                }
                // Whether this is the radio in use (`wire::USE_*`), for `nic-driver`'s bridge - a stand-in
                // answers `USE_STANDIN`, and the bridge stays on it as on the radio in use.
                out[15] = if standing_in { wire::USE_STANDIN } else { choice_use };
                16
            }
            (wire::OP_NET_TX, Some(session)) => {
                last_frame_op = Since::now(ctx);
                // `[op, sent]`. Refused, not queued, when there is no link to send on: the stack retries
                // on its own pace and a refusal is a fact it can act on.
                out[0] = wire::OP_NET_TX;
                let eth = &payload[1..];
                let mut sent = false;
                if radio_on && joined.is_some() && sweep.is_none() && eth.len() >= ETH_HEADER {
                    if !session.tx_ok() {
                        // Credit comes back on received frames; a stack that only sends runs dry.
                        let p = session.pull(&mut rxq, ctx);
                        if note_pull(session, &p, &mut joined, &mut rekey_seen, who, ctx) {
                            auto_join = rejoin_after_drop(ctx, who, joined_at_secs, last_joined);
                            auto_join_why = "after the access point dropped it";
                        }
                    }
                    if joined.is_some() {
                        sent = session.send(eth, ctx);
                    }
                }
                out[1] = sent as u8;
                2
            }
            (wire::OP_NET_RX, Some(session)) => {
                last_frame_op = Since::now(ctx);
                // `[op, len_lo, len_hi, frame...]`, oldest first; a length of 0 is "nothing waiting".
                // The chip is read only when the queue is empty and the radio has a link to read.
                out[0] = wire::OP_NET_RX;
                if rxq.is_empty() && radio_on && joined.is_some() && sweep.is_none() {
                    let p = session.pull(&mut rxq, ctx);
                    if note_pull(session, &p, &mut joined, &mut rekey_seen, who, ctx) {
                        auto_join = rejoin_after_drop(ctx, who, joined_at_secs, last_joined);
                        auto_join_why = "after the access point dropped it";
                    }
                }
                let n = rxq.pop(&mut out[3..]);
                out[1..3].copy_from_slice(&(n as u16).to_le_bytes());
                3 + n
            }
            _ => {
                out[0] = wire::UNKNOWN_OP;
                1
            }
        };
        // `try_send`, never `send`: the shell may have given up on this reply (`q`, or its own deadline),
        // and a blocking send toward a peer that is not receiving is the mutual-blocking anti-pattern §8.9
        // names. A failed reply usually means nobody was waiting - but it can also mean the caller's
        // queue is FULL while it waits (`nic-driver` blocked on this very answer with sixteen stale
        // requests behind it), and that one is worth seeing, so it is counted and reported sparingly.
        let served_ms = served_t0.elapsed_us(ctx) / 1000;
        if served_ms >= 500 {
            say_fmt(ctx, who, format_args!("op {:#04x} took {} ms to serve", op, served_ms));
        }
        // `gs::ipc::reply` sends with `try_send` and then RECLAIMS THE REPLY CAP, both halves every time. The
        // cap is one-shot and holds a slot in this task's table until it is removed, and a task holds 64:
        // the reclaim was missing once, and the driver went deaf after its first fifty requests (the
        // capless arm above).
        if gs::ipc::reply(ctx, reply, &tagged_reply(tag, &out[..n])).is_err() {
            reply_failed = reply_failed.saturating_add(1);
            if reply_failed == 1 || reply_failed % 64 == 0 {
                say_fmt(ctx, who, format_args!(
                    "an answer (op {:#04x}) could not be delivered (x{}) - the caller gave up, or its queue is full",
                    op, reply_failed
                ));
            }
        }
    }
}