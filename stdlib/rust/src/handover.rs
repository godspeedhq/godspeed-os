// SPDX-License-Identifier: Apache-2.0
//! A LIVE HANDOVER: a running service replaced by a new version without stopping - its state carried across, its
//! name never unanswered.
//!
//! Erlang does this by loading new code into a running process. GodspeedOS will not (CLAUDE.md 2.5: the bytes that
//! run are the bytes that were signed and checked), so it does the same thing with two isolated programs and an
//! explicit handover:
//!
//! 1. The supervisor checks the new version from the disk and starts it BESIDE the running one, under the same name;
//!    the name now answers to the new one.
//! 2. It sends the running one an OFFER naming its successor. The running one finishes what is already in its queue,
//!    sends its state to the successor, and ends itself.
//! 3. The successor takes the state and carries on. What Erlang calls code change is yours: it reads the old version's state and
//!    turns it into its own.
//!
//! A service opts in by declaring `handover = true` in its contract and doing both halves:
//!
//! ```ignore
//! let mut h = gs::handover::Handover::new(&ctx);
//! let mut buf = [0u8; gs::handover::STATE_MAX];
//! let mut count = 0u64;
//! if let Some(t) = h.take_over(&mut buf) {                // a successor: the running version's state
//!     count = decode(t.state);
//!     for m in t.early.iter().flatten() { handle(m); }
//! }
//! loop {
//!     let msg = gs::ipc::recv(&ctx);
//!     if let Some(offer) = h.offer(&msg) {                 // the supervisor's offer, checked
//!         h.hand_over(offer, &encode(count), |m| handle(&m));   // does not return
//!     }
//!     handle(&msg);
//! }
//! ```
//!
//! **Why an offer is checked, and how.** The offer arrives on the service's public endpoint, which every client can
//! send to, and the kernel does not say who sent a message. Anyone able to send an unchecked offer could make a service
//! hand its state to them and end. So the supervisor gives every handover-capable start a KEY in its spawn mode
//! (`SpawnRequest::probe_mode`, which only the supervisor can set, since only it calls `SpawnImage`), and a successor a
//! key of its own. An offer must carry the running version's key and names the successor's; the state must carry the
//! successor's. Each side refuses after [`WRONG_MAX`] wrong ones and stops listening for more, saying so - so a key
//! cannot be guessed, at the cost that someone who keeps sending wrong offers can make a later upgrade fall back to a
//! stop and start. That is loud, and no worse than an upgrade was before handovers.
//!
//! **What it does not do, said here rather than discovered:** capabilities do not travel - the successor holds what its
//! own spawn request gave it, as a restart would. A request a client sent to the old version after it drained its queue
//! and before it ended is lost with it, and that client's `gs::call` reports it (`OutcomeUnknown`), exactly as for a
//! death. State is one message, [`STATE_MAX`] bytes; a service that needs more keeps the rest on disk.

use crate::ipc::{self, Message};
use crate::ServiceContext;

/// Spawn-mode bit: this start carries a handover key (the low 30 bits).
pub const KEYED: u32 = 1 << 30;
/// Spawn-mode bit: this start is a SUCCESSOR, to wait for a running version's state.
pub const SUCCESSOR: u32 = 1 << 31;
/// The key's bits.
pub const KEY_MASK: u32 = KEYED - 1;
/// Supervisor -> running version: the offer, then the running version's key and the successor's (u32 LE each).
pub const OFFER: &[u8; 8] = b"GSHOVER2";
/// Running version -> successor: the successor's key (u32 LE), then the state.
pub const STATE: &[u8; 8] = b"GSHOSTA2";
/// The most state one handover carries.
pub const STATE_MAX: usize = 4096 - 12;
/// How long a successor waits for its state before it starts fresh, saying so.
pub const TAKE_SECS: i64 = 10;
/// How many ordinary messages a successor keeps that arrive before the state does.
pub const EARLY_MAX: usize = 2;
/// Wrong offers (or wrong states) taken before this side stops listening for any.
pub const WRONG_MAX: u32 = 3;

/// The spawn mode the supervisor gives a handover-capable start with `key`.
pub const fn mode(key: u32, successor: bool) -> u32 {
    KEYED | (key & KEY_MASK) | if successor { SUCCESSOR } else { 0 }
}

/// An offer that checked: the successor's key, which the state must carry.
#[derive(Clone, Copy)]
pub struct Offer { successor_key: u32 }

/// What a successor was handed: the state, and any ordinary messages that arrived before it, in order.
pub struct TakeOver<'b> {
    pub state: &'b [u8],
    pub early: [Option<Message>; EARLY_MAX],
}

/// One service's side of handovers: its key, and how many wrong offers it has seen.
pub struct Handover<'c> {
    ctx: &'c ServiceContext,
    mode: u32,
    wrong: u32,
}

impl<'c> Handover<'c> {
    pub fn new(ctx: &'c ServiceContext) -> Handover<'c> {
        Handover { ctx, mode: ctx.probe_mode(), wrong: 0 }
    }

    /// Was this program started to take over from a running version of itself?
    pub fn is_successor(&self) -> bool { self.mode & (KEYED | SUCCESSOR) == KEYED | SUCCESSOR }

    fn key(&self) -> u32 { self.mode & KEY_MASK }

    /// Is `msg` the supervisor's offer for THIS version? `None` for an ordinary message, and for an offer that does not
    /// carry this version's key, which is counted: after [`WRONG_MAX`] of them no offer is taken at all.
    pub fn offer(&mut self, msg: &Message) -> Option<Offer> {
        let p = msg.payload_bytes();
        if p.len() != 16 || &p[..8] != OFFER { return None; }
        if self.mode & KEYED == 0 || self.wrong >= WRONG_MAX {
            return None;
        }
        let mine = u32::from_le_bytes([p[8], p[9], p[10], p[11]]);
        let next = u32::from_le_bytes([p[12], p[13], p[14], p[15]]);
        if mine != self.key() {
            self.wrong += 1;
            // The capability that came with it is not ours to keep.
            if let Some(c) = ipc::take_sent_cap(self.ctx) { crate::cap::remove(self.ctx, c); }
            self.ctx.log_fmt(format_args!("handover: an offer with the wrong key - refused ({} of {}){}", self.wrong, WRONG_MAX,
                if self.wrong >= WRONG_MAX { "; no more offers are taken, so an upgrade restarts this instead" } else { "" }));
            return None;
        }
        Some(Offer { successor_key: next & KEY_MASK })
    }

    /// Hand over and end: every message already queued goes to `drain` first (they were sent to this version, so this
    /// version answers them), then the state goes to the successor with its key, then this program ends with status 0.
    /// A successor that cannot be reached is said, and this version ends all the same: the successor holds the name.
    pub fn hand_over(&self, offer: Offer, state: &[u8], mut drain: impl FnMut(Message)) -> ! {
        let ctx = self.ctx;
        let successor = ipc::take_sent_cap(ctx);
        // Bounded by the queue: an endpoint holds sixteen, and nothing new comes here once the name is the successor's.
        for _ in 0..64 {
            match ipc::try_recv(ctx) {
                Some(m) => drain(m),
                None => break,
            }
        }
        let mut out = [0u8; 4096];
        let n = state.len().min(STATE_MAX);
        out[..8].copy_from_slice(STATE);
        out[8..12].copy_from_slice(&offer.successor_key.to_le_bytes());
        out[12..12 + n].copy_from_slice(&state[..n]);
        match successor {
            Some(c) => {
                if ipc::send_to(ctx, c, &Message::from_bytes(&out[..12 + n])).is_err() {
                    ctx.log("handover: the successor could not be reached - its state is lost; it starts fresh");
                }
                crate::cap::remove(ctx, c);
            }
            None => ctx.log("handover: offered with no successor attached - nothing to hand to"),
        }
        if state.len() > STATE_MAX {
            ctx.log_fmt(format_args!("handover: state cut to {} of {} bytes", STATE_MAX, state.len()));
        }
        crate::program::exit(ctx, 0)
    }

    /// Take over, if this is a successor: wait up to [`TAKE_SECS`] for the running version's state - carrying this
    /// successor's key - keeping up to [`EARLY_MAX`] ordinary messages that arrive first. `None` for a program that is
    /// not a successor, and for one whose state did not come in time, which is said: it starts fresh, as a restart
    /// would. A state with the wrong key is refused, and after [`WRONG_MAX`] of them none is taken.
    pub fn take_over<'b>(&mut self, buf: &'b mut [u8; STATE_MAX]) -> Option<TakeOver<'b>> {
        if !self.is_successor() { return None; }
        let ctx = self.ctx;
        let mut early: [Option<Message>; EARLY_MAX] = core::array::from_fn(|_| None);
        let mut ne = 0usize;
        let start = crate::task::uptime_secs(ctx);
        loop {
            let left = TAKE_SECS - (crate::task::uptime_secs(ctx) - start);
            if left <= 0 || self.wrong >= WRONG_MAX { break; }
            let Some(m) = ipc::recv_within(ctx, left) else { break };
            let p = m.payload_bytes();
            if p.len() >= 12 && &p[..8] == STATE {
                let key = u32::from_le_bytes([p[8], p[9], p[10], p[11]]);
                if key != self.key() {
                    self.wrong += 1;
                    ctx.log_fmt(format_args!("handover: a state with the wrong key - refused ({} of {})", self.wrong, WRONG_MAX));
                    continue;
                }
                let n = (p.len() - 12).min(buf.len());
                buf[..n].copy_from_slice(&p[12..12 + n]);
                return Some(TakeOver { state: &buf[..n], early });
            }
            if ne < EARLY_MAX {
                early[ne] = Some(m);
                ne += 1;
            } else {
                ctx.log("handover: a message came before the state with no room to keep it - dropped");
            }
        }
        ctx.log("handover: no state came from the running version - starting fresh");
        None
    }
}
