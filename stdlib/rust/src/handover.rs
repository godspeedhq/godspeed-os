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
//! 2. It tells the running one who its successor is. The running one finishes what is already in its queue, sends its
//!    state to the successor, and ends itself.
//! 3. The successor takes the state and carries on. Its `code_change` is yours: it reads the old version's state and
//!    turns it into its own.
//!
//! A service opts in by declaring `handover = true` in its contract and doing both halves:
//!
//! ```ignore
//! // At start: a successor takes the state; anyone else starts fresh.
//! let mut buf = [0u8; gs::handover::STATE_MAX];
//! let mut count = 0u64;
//! if gs::handover::is_successor(&ctx) {
//!     if let Some(t) = gs::handover::take_over(&ctx, &mut buf) {
//!         count = decode(t.state);              // the old version's state, in its format
//!         for m in t.early.iter().flatten() { handle(m); }
//!     }
//! }
//! loop {
//!     let msg = gs::ipc::recv(&ctx);
//!     if gs::handover::is_offer(&msg) {
//!         gs::handover::hand_over(&ctx, &encode(count), |m| handle(&m));   // does not return
//!     }
//!     handle(&msg);
//! }
//! ```
//!
//! **What it does not do, said here rather than discovered:** capabilities do not travel - the successor holds what its
//! own spawn request gave it, as a restart would. A request a client sent to the old version after it drained its queue
//! and before it ended is lost with it, and that client's `gs::call` reports it (`OutcomeUnknown`), exactly as for a
//! death. State is one message, [`STATE_MAX`] bytes; a service that needs more keeps the rest on disk and hands over
//! where it is.

use crate::ipc::{self, Message};
use crate::ServiceContext;

/// The spawn mode a successor is started with (`SpawnRequest::probe_mode`), so it knows to wait for its state.
pub const MODE: u32 = 0x4844_4f56;
/// Supervisor -> running version: "your successor's endpoint comes with this message; hand over to it".
pub const OFFER: &[u8; 8] = b"GSHOVER1";
/// Running version -> successor: the state follows these eight bytes.
pub const STATE: &[u8; 8] = b"GSHOSTA1";
/// The most state one handover carries.
pub const STATE_MAX: usize = 4096 - 8;
/// How long a successor waits for its state before it starts fresh, saying so.
pub const TAKE_SECS: i64 = 10;
/// How many ordinary messages a successor keeps that arrive before the state does.
pub const EARLY_MAX: usize = 2;

/// Was this program started to take over from a running version of itself?
pub fn is_successor(ctx: &ServiceContext) -> bool { ctx.probe_mode() == MODE }

/// Is `msg` the supervisor telling this version to hand over?
pub fn is_offer(msg: &Message) -> bool { msg.payload_bytes() == OFFER }

/// What a successor was handed: the state, and any ordinary messages that arrived before it, in order.
pub struct TakeOver<'b> {
    pub state: &'b [u8],
    pub early: [Option<Message>; EARLY_MAX],
}

/// Hand over and end: every message already queued goes to `drain` first (they were sent to this version, so this
/// version answers them), then the state goes to the successor, then this program ends with status 0. A successor that
/// cannot be reached is said, and this version ends all the same: the successor already holds the name.
pub fn hand_over(ctx: &ServiceContext, state: &[u8], mut drain: impl FnMut(Message)) -> ! {
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
    out[8..8 + n].copy_from_slice(&state[..n]);
    match successor {
        Some(c) => {
            if ipc::send_to(ctx, c, &Message::from_bytes(&out[..8 + n])).is_err() {
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

/// Take over: wait up to [`TAKE_SECS`] for the running version's state, keeping up to [`EARLY_MAX`] ordinary messages
/// that arrive first. `None` when none came in time, which is said: the service starts fresh, as a restart would.
pub fn take_over<'b>(ctx: &ServiceContext, buf: &'b mut [u8; STATE_MAX]) -> Option<TakeOver<'b>> {
    let mut early: [Option<Message>; EARLY_MAX] = core::array::from_fn(|_| None);
    let mut ne = 0usize;
    let start = crate::task::uptime_secs(ctx);
    loop {
        let left = TAKE_SECS - (crate::task::uptime_secs(ctx) - start);
        if left <= 0 { break; }
        let Some(m) = ipc::recv_within(ctx, left) else { break };
        let p = m.payload_bytes();
        if p.len() >= 8 && &p[..8] == STATE {
            let n = (p.len() - 8).min(buf.len());
            buf[..n].copy_from_slice(&p[8..8 + n]);
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
