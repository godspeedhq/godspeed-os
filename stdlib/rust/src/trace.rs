// SPDX-License-Identifier: Apache-2.0
//! What this service is doing, and what every service has done: metrics, traces and the log tail.
//!
//! # Two halves, and only one of them was reachable
//!
//! **Publishing** already worked from `gs`: [`metric`] and [`as_name`] wrap methods the context has
//! always carried, and the SDK instruments every request and reply on its own. What had no route was
//! **reading any of it back**. The query side is a wire protocol spoken to the `events` service - an
//! opcode byte out, a packed reply in - and a program that wanted the numbers had to import
//! `godspeed_sdk`, hand-assemble the request and decode fixed-width fields itself. That is exactly
//! the kind of protocol this library exists to hide, the same way [`crate::fs`] hides the filesystem's.
//!
//! # Everything here answers with a [`Table`]
//!
//! Not a printed grid and not JSON. The table is the truth and a rendering is a view of it
//! (Commandment III), so a caller sorts, filters or aggregates first and renders once:
//!
//! ```ignore
//! let mut t = gs::trace::metrics(&ctx)?;
//! t.filter("owner", "=", "fs");
//! t.sort("value", true);
//! t.to_grid(&mut sink);
//! ```
//!
//! # The log tail here is a CONVENIENCE, not the log
//!
//! Every line [`logs`] returns also went to serial and the kernel ring the moment it was written, by
//! syscall, before `events` ever saw a copy (CLAUDE.md 11.4). Serial is complete; this window is not.
//! It cannot show anything printed before `events` started, nor a line dropped when the sink's queue
//! was briefly full. Reach for it to ask a question at a prompt, never to prove something did not
//! happen.
//!
//! # Bounded, like every other wait in this library
//!
//! Each query is one [`crate::call::request_within`] with a short deadline: a `CallDeadline`, so a
//! service that also SERVES clients can call these without a plain receive swallowing an unrelated
//! request (CLAUDE.md 8.2). The sink holds its ring in memory, so a healthy answer is immediate and a
//! slow one means the sink is in trouble - which is usually the reason you are asking.

use godspeed_sdk::ipc::Message;
use godspeed_sdk::service_context::ServiceContext;
use godspeed_sdk::trace::{MET_LEN, MET_NAME_LEN, PEER_LEN};

use crate::error::Error;
use crate::record::{Table, Value};

/// The service that holds the rings: `"events"`.
///
/// Named so a program can report which service failed to answer, rather than printing a name it
/// hard-coded separately and would not update if this one moved.
pub const SINK: &str = godspeed_sdk::trace::SINK_NAME;

/// How long to wait for the sink. The rings are in memory, so a healthy reply is immediate.
const ASK_SECS: i64 = 3;

/// Declare the name this service publishes under.
///
/// **Call this once at startup if you publish anything at all.** Metrics are keyed by
/// `(owner, name)`, so every service that never declares one publishes under an empty owner and they
/// all collide into a single row with their counters interleaving. [`metrics`] renders that owner as
/// `?` rather than blank, because a blank cell reads as a formatting quirk and this is a wrong number.
pub fn as_name(ctx: &ServiceContext, name: &str) {
    ctx.trace_as(name);
}

/// Publish the current value of a named metric.
///
/// A metric is a **set, not an increment**: the sink keeps the last value published under
/// `(owner, name)`, and it keeps it after the publisher dies - a sample outliving its emitter is the
/// one useful thing left to learn from a service that is gone.
///
/// The name is truncated past [`MET_NAME_LEN`](godspeed_sdk::trace::MET_NAME_LEN) bytes, and a
/// truncated name is a DIFFERENT name that can silently merge two metrics into one row. The SDK says
/// so once per service rather than once per sample.
///
/// This cannot fail in a way a caller can act on, so it returns nothing: the sink is best-effort by
/// design, and a dropped sample is retried on the next interval rather than failing the work that
/// produced it.
pub fn metric(ctx: &ServiceContext, name: &str, value: u64) {
    ctx.metric(name, value);
}

/// Every metric the sink currently holds: `owner`, `metric`, `value`, `age_s`.
///
/// `age_s` is seconds since that sample was published, not since it was read. Without it a number
/// frozen at the moment of a crash is indistinguishable from one being maintained right now, and an
/// instrument that cannot tell those apart is worse than none.
///
/// # Errors
///
/// [`Error::Unreachable`] if the sink could not be reached, [`Error::OutcomeUnknown`] if it did not
/// answer in time, [`Error::Failed`] if it answered with something this cannot parse.
pub fn metrics(ctx: &ServiceContext) -> Result<Table, Error> {
    let reply = ask(ctx, godspeed_sdk::trace::TRACE_OP_METRICS)?;
    let b = reply.payload_bytes();
    if b.is_empty() {
        return Err(Error::Failed);
    }
    let n = b[0] as usize;
    let stride = MET_LEN + 4;            // the record, plus the u32 timestamp the sink appends
    if b.len() < 1 + n * stride {
        return Err(Error::Failed);
    }

    let now = ctx.epoch_secs_monotonic() as u32;
    let mut t = Table::new(&["owner", "metric", "value", "age_s"]);
    for i in 0..n {
        let o = 1 + i * stride;
        let owner = trim(&b[o..o + PEER_LEN]);
        let name = trim(&b[o + PEER_LEN..o + PEER_LEN + MET_NAME_LEN]);
        let v = o + PEER_LEN + MET_NAME_LEN;
        let value = u64::from_le_bytes([
            b[v], b[v + 1], b[v + 2], b[v + 3], b[v + 4], b[v + 5], b[v + 6], b[v + 7],
        ]);
        let at = u32::from_le_bytes([
            b[o + MET_LEN], b[o + MET_LEN + 1], b[o + MET_LEN + 2], b[o + MET_LEN + 3],
        ]);
        // An undeclared publisher reads `?`, never blank - see [`as_name`].
        let ov = if owner.is_empty() { t.intern(b"?") } else { t.intern(owner) };
        let nv = t.intern(name);
        t.add_row(&[ov, nv, Value::Int(value), Value::Int(now.saturating_sub(at) as u64)]);
    }
    Ok(t)
}

/// The tail of what services printed: `owner`, `text`.
///
/// Read the module header before relying on this - it is a window, not the log.
///
/// The owner is a field, so the service's own `name: ` prefix is stripped from the text; keeping it
/// would state the same fact twice in one row.
///
/// # Errors
///
/// As [`metrics`].
pub fn logs(ctx: &ServiceContext) -> Result<Table, Error> {
    let reply = ask(ctx, godspeed_sdk::trace::TRACE_OP_LOGS)?;
    let b = reply.payload_bytes();
    // A 25-byte header carries the cursor fields a repeated drainer needs (which lines are new, and
    // whether the window outran it). A one-shot reader wants only the text that follows.
    const HEADER: usize = 25;
    if b.len() < HEADER {
        return Err(Error::Failed);
    }

    let body = &b[HEADER..];
    let mut t = Table::new(&["owner", "text"]);
    let mut start = 0usize;
    for i in 0..=body.len() {
        let end = if i == body.len() {
            body.len()
        } else if body[i] == b'\n' {
            i
        } else {
            continue;
        };
        if end > start {
            let line = &body[start..end];
            // The sink separates owner from text with 0x1f (unit separator).
            let cut = line.iter().position(|&c| c == 0x1f).unwrap_or(line.len());
            let owner = &line[..cut];
            let mut text = if cut < line.len() { &line[cut + 1..] } else { &line[..0] };
            // Drop the self-prefix: `fs` + `fs: disk capacity = ...` is one fact twice.
            if !owner.is_empty() && text.starts_with(owner) {
                let rest = &text[owner.len()..];
                if rest.starts_with(b": ") {
                    text = &rest[2..];
                }
            }
            let ov = if owner.is_empty() { t.intern(b"?") } else { t.intern(owner) };
            let tv = t.intern(text);
            t.add_row(&[ov, tv]);
        }
        start = end + 1;
    }
    Ok(t)
}

/// How the event ring is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Status {
    /// How many events the ring can hold.
    pub capacity: u64,
    /// How many have been recorded since boot.
    pub recorded: u64,
    /// How many were **overwritten before anyone read them**. Non-zero means this instrument is
    /// lossy right now, and anything read from it is a sample rather than a record.
    pub dropped: u64,
}

/// Capacity, total recorded, and how many were dropped unread.
///
/// The ring lives in the `events` service; the kernel records nothing (CLAUDE.md 11.4). So this
/// answers for a restartable service, and its counters start again when it restarts.
///
/// # Errors
///
/// As [`metrics`].
pub fn status(ctx: &ServiceContext) -> Result<Status, Error> {
    let reply = ask(ctx, godspeed_sdk::trace::TRACE_OP_STATUS)?;
    let b = reply.payload_bytes();
    if b.len() < 24 {
        return Err(Error::Failed);
    }
    let at = |i: usize| -> u64 {
        u64::from_le_bytes([
            b[i], b[i + 1], b[i + 2], b[i + 3], b[i + 4], b[i + 5], b[i + 6], b[i + 7],
        ])
    };
    Ok(Status { capacity: at(0), recorded: at(8), dropped: at(16) })
}

/// How an exchange ENDED. One row per exchange, so this is its whole story.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// A request went out.
    Request,
    /// A reply came back. The exchange closed normally.
    Reply,
    /// No reply before the deadline. **The outcome is unknown** - it may still have happened.
    Timeout,
    /// The peer died while the exchange was open.
    PeerLost,
    /// The peer's queue was full, so the request never left. Nothing happened.
    QueueFull,
    /// The user changed their mind. Not a failure.
    Aborted,
    /// A kind this library does not know. Newer sink than stdlib, or a corrupt row.
    Unknown,
}

impl Outcome {
    /// Did the exchange fail to get where it was going?
    ///
    /// True for [`Outcome::Timeout`], [`Outcome::PeerLost`] and [`Outcome::QueueFull`]. **False for
    /// [`Outcome::Aborted`]**, which is a person deciding, not a fault - counting it as one turns
    /// every impatient keypress into a reliability statistic.
    pub fn is_failure(self) -> bool {
        matches!(self, Outcome::Timeout | Outcome::PeerLost | Outcome::QueueFull)
    }

    fn from_kind(k: u8) -> Outcome {
        use godspeed_sdk::trace as t;
        match k {
            t::KIND_REQUEST => Outcome::Request,
            t::KIND_REPLY => Outcome::Reply,
            t::KIND_TIMEOUT => Outcome::Timeout,
            t::KIND_PEER_LOST => Outcome::PeerLost,
            t::KIND_QUEUE_FULL => Outcome::QueueFull,
            t::KIND_ABORTED => Outcome::Aborted,
            _ => Outcome::Unknown,
        }
    }

    fn name(self) -> &'static [u8] {
        match self {
            Outcome::Request => b"REQUEST",
            Outcome::Reply => b"REPLY",
            Outcome::Timeout => b"TIMEOUT",
            Outcome::PeerLost => b"PEER_LOST",
            Outcome::QueueFull => b"QUEUE_FULL",
            Outcome::Aborted => b"ABORTED",
            Outcome::Unknown => b"?",
        }
    }
}

/// The newest `max` IPC exchanges: `seq`, `sec`, `caller`, `peer`, `op`, `outcome`.
///
/// Rows are in ring order, oldest first. What each column is worth knowing about:
///
/// - **`seq`** is the EMITTER'S own event number, not a global one, so a dump mixing several services
///   interleaves several sequences and can look unsorted. It is not. A GAP in one service's numbering
///   is that service's dropped events.
/// - **`sec`** is seconds since the oldest row shown. The stored value is an epoch second, which says
///   nothing alone; the GAP between rows is what a stall looks like.
/// - **`caller`** is who made the call, as that service DECLARED itself ([`as_name`]). A service
///   cannot be asked its own name - identity is not ambient - so an undeclared one reads `?`.
/// - **`op`** is the opcode as a NUMBER. Naming it needs knowledge of the protocol being spoken, and
///   only the service speaking it has that. See this function's source for why guessing is wrong.
///
/// `max` is clamped to what one reply can carry. Ask [`status`] how much history exists.
///
/// # Errors
///
/// As [`metrics`].
pub fn events(ctx: &ServiceContext, max: usize) -> Result<Table, Error> {
    let want = max.min(u8::MAX as usize) as u8;
    let reply = crate::call::request_within(
        ctx,
        SINK,
        &Message::from_bytes(&[godspeed_sdk::trace::TRACE_OP_DUMP, want]),
        ASK_SECS,
    )?;
    let b = reply.payload_bytes();
    if b.is_empty() {
        return Err(Error::Failed);
    }
    let n = b[0] as usize;
    let ev = godspeed_sdk::trace::EV_LEN;

    // Every `sec` is relative to the oldest row, so the column reads as elapsed time rather than as
    // an epoch second nobody can subtract in their head.
    let base = if n > 0 && b.len() >= 9 {
        u32::from_le_bytes([b[5], b[6], b[7], b[8]])
    } else {
        0
    };

    let mut t = Table::new(&["seq", "sec", "caller", "peer", "op", "outcome"]);
    for i in 0..n {
        let o = 1 + i * ev;
        if o + ev > b.len() {
            break;                       // a short reply is what we got; report what parsed
        }
        let seq = u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let at = u32::from_le_bytes([b[o + 4], b[o + 5], b[o + 6], b[o + 7]]);
        let caller = trim(&b[o + 8..o + 8 + PEER_LEN]);
        let peer = trim(&b[o + 8 + PEER_LEN..o + 8 + 2 * PEER_LEN]);
        let op = b[o + 8 + 2 * PEER_LEN];
        let outcome = Outcome::from_kind(b[o + 9 + 2 * PEER_LEN]);

        let cv = if caller.is_empty() { t.intern(b"?") } else { t.intern(caller) };
        let pv = t.intern(peer);
        let ov = t.intern(outcome.name());
        t.add_row(&[
            Value::Int(seq as u64),
            Value::Int(at.saturating_sub(base) as u64),
            cv,
            pv,
            Value::Int(op as u64),
            ov,
        ]);
    }
    Ok(t)
}

/// One bounded question to the sink.
fn ask(ctx: &ServiceContext, op: u8) -> Result<Message, Error> {
    crate::call::request_within(ctx, SINK, &Message::from_bytes(&[op]), ASK_SECS)
}

/// A fixed-width field is NUL-padded; the name ends at the first NUL.
fn trim(field: &[u8]) -> &[u8] {
    let n = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    &field[..n]
}
