// SPDX-License-Identifier: GPL-2.0-only
//! `events` - the diagnostic sink (§11.4). Restartable.
//!
//! Two jobs, both "somewhere to put diagnostic data that someone reads later":
//!
//! 1. **Drain its endpoint.** The endpoint EXISTS, so anything sent here must be consumed or the
//!    16-deep queue sits full forever (a stub that only parks never recv's, and a `chaos flood-storm`
//!    then wedges it permanently).
//! 2. **Hold the IPC trace ring** (`utilities/46_trace.md` mechanism B) - a bounded history of
//!    request/reply events emitted by services that hold a send cap to this one, read back by the
//!    `trace` utility.
//!
//! # Why the ring is HERE and not in the kernel, and not in a service of its own
//!
//! The requirement asked for a bounded ring recording IPC, and the obvious home was the kernel: it is
//! already at the routing point holding sender, endpoint and generation. That reading answers "where
//! is the data?" instead of "whose responsibility is this?". A kernel ring would have added storage
//! with a lifecycle, a retention policy (what to discard is a JUDGEMENT, and judgement is policy -
//! §26.10), a message-identity scheme, a control syscall, an authority decision, and a write on the
//! hottest path in the system.
//!
//! The first fix was a `tracer` service. The enforcement layer refused it, and was right to: the
//! kernel holds a `service_config` for every service, and that catalogue is pinned as DEBT THAT MAY
//! ONLY SHRINK - so even a userspace ring would have cost ring 0 three lines (the config, the
//! death-notification set, the restart counter). "The kernel gains nothing" was *almost* true, and the
//! almost was the whole point.
//!
//! `events` already exists in all three lists, is already managed and watched, and its entire purpose
//! is diagnostic data. Putting the ring here costs the kernel **exactly nothing**.
//!
//! It is worth being precise about one tension: `docs/logging.md` calls this service "a stateless
//! broker, not a store", and that is about PERSISTENCE - an `events` that writes through `fs` makes
//! observing a storage failure depend on storage. A fixed in-memory ring is not persistence. It
//! survives nothing: a restarted events starts empty, and that is correct, because the ring is
//! history and nothing depends on it.
//!
//! # Bounded, and loud about loss (§26.6, invariant 12)
//!
//! Fixed ring, fixed events, no heap. Full = overwrite the oldest and COUNT it; `trace status` reports
//! the count. A ring that discards silently is the bug this project just fixed in the x86 keyboard
//! path - an instrument that loses data without saying so is worse than no instrument.

#![no_std]
#![no_main]

use godspeed_sdk::trace::{
    EV_LEN, MET_LEN, MET_NAME_LEN, PEER_LEN, TRACE_OP_LOG, TRACE_OP_LOGS, TRACE_OP_DUMP, TRACE_OP_EVENT, TRACE_OP_METRIC, TRACE_OP_METRICS,
    TRACE_OP_STATUS,
};
use godspeed_sdk::{Message, ServiceContext};

/// Events retained. 192 x 34 B is about 6.5 KiB, inside this service's existing footprint.
///
/// Sized for "what just happened", which is the question a stalled chain asks. Deliberately NOT sized
/// for "what happened a minute ago": under load that needs either a much larger ring or filtering at
/// the emitter, and filtering-in-the-middle is the first step toward putting a programmable VM
/// somewhere it does not belong. If longer history is ever wanted, the honest answer is a bigger arena
/// HERE, where it costs one service more memory and costs the kernel nothing.
const RING: usize = 192;

/// Distinct metric samples retained: 64 x 36 B is about 2.3 KiB, and the whole table fits in one 4 KiB
/// reply message with room to spare.
///
/// A FIXED table, not a map that grows with distinct names, because a counter keyed by arbitrary
/// strings is unbounded state wearing a small hat (§26.6.1). 64 is a bound a reader can read off this
/// line. When it is full the sink says so, loudly and once - a 65th metric that vanished silently
/// would make every number here suspect, since nothing would tell you which ones were missing.
const METRICS: usize = 64;

/// Bytes of queryable log scrollback. A BYTE ring, not a line array, so one long line costs its own
/// length instead of a whole fixed slot - `fs`'s durability warning is 280 bytes and the journal
/// refusal 356, and both are lines the constitution leans on.
///
/// 8 KiB is deliberately modest. This is a CONVENIENCE over a floor that outlives it: every line here
/// also went to serial and the kernel ring, unconditionally, before this service ever saw a copy. The
/// value of more scrollback is bounded by that, whereas the cost of a bigger arena is paid always.
const LOG_BYTES: usize = 8192;

/// Most text returned by one `events log`, inside a single 4 KiB message.
const LOG_REPLY_MAX: usize = 3072;

/// Wire length of one metric in a DUMP reply: the emitted record plus the stamp the sink adds.
const MET_OUT: usize = MET_LEN + 4;

/// One published sample. Keyed by (owner, name): two services may both publish `requests` and they are
/// different numbers, so the owner is part of the identity rather than a label beside it.
#[derive(Clone, Copy, Default)]
struct Met {
    owner: [u8; PEER_LEN],
    name: [u8; MET_NAME_LEN],
    /// The LAST value published. A metric is a set, not an increment (`sdk::trace::TRACE_OP_METRIC`) -
    /// the owning service holds the counter and publishes what it reads.
    value: u64,
    /// When the sink accepted it. Stamped HERE for the same reason an event is: the publisher must not
    /// pay a clock read.
    at_s: u32,
}

/// One recorded event - mirrors the wire format in `sdk::trace`.
#[derive(Clone, Copy, Default)]
struct Ev {
    seq: u32,
    at_s: u32,
    /// Who made the call, as that service declared itself (`ServiceContext::trace_as`). A service
    /// cannot ask what it is called - identity is not ambient - so a traced one says. Exactly as
    /// trustworthy as the two fields below, because the whole event is the emitter's testimony.
    caller: [u8; PEER_LEN],
    /// The PEER'S NAME, as the emitter knew it. Not an endpoint and not a cap slot: a slot is local to
    /// the emitter and means nothing here, and a name is what a reader actually wants.
    peer: [u8; PEER_LEN],
    op: u8,
    kind: u8,
}

/// Answer a query over the reply cap the request carried, NON-BLOCKING (§8.9).
///
/// A reader that has gone away must never block `events`: this service sits at the end of every
/// emitter's `try_send`, so a blocking reply here would let one stalled reader stall the sink for the
/// whole system. Dropped on failure - the caller retries, and a lost answer costs nothing.
fn reply(ctx: &ServiceContext, out: &[u8]) {
    if let Some(cap) = ctx.take_pending_cap() {
        let _ = ctx.try_send_by_handle(cap, &Message::from_bytes(out));
        // RECLAIM IT. A reply capability is a one-shot return address handed to us inside the request;
        // sending on it does not consume it, so leaving it behind burns a cap-table slot per reply
        // until the table is full. `block-driver`, `console` and `fs` all do this - this service was
        // the one that did not.
        //
        // It was visible before it was fatal: `trace deps fs` drew `events -> shell`, because a
        // retained return address is indistinguishable from a wired peer (both SEND|GRANT to a live
        // task's endpoint). A leak that shows up as a wrong arrow in a diagram is a lucky leak.
        ctx.remove_cap(cap);
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    let mut ring = [Ev::default(); RING];
    let mut next = 0usize; // write cursor
    let mut total = 0u64; // events ever accepted
    let mut dropped = 0u64; // events overwritten before being read
    let mut mets = [Met::default(); METRICS];
    let mut nmets = 0usize; // occupied slots
    let mut mets_full = 0u64; // samples refused because the table is full
    let mut mets_full_said = false; // the refusal is reported ONCE, not once per sample
    let mut logbuf = [0u8; LOG_BYTES];
    let mut loghead = 0usize; // write cursor
    let mut logwrapped = false; // older lines have been overwritten
    let mut loglines = 0u64; // lines accepted, ever

    // The event clock, CACHED. `epoch_secs_monotonic` is a CMOS RTC read on x86 - `wait_update_clear`
    // can spin ~1 ms before seven port-I/O reads - so calling it per event would cap this sink at
    // roughly a thousand events a second and drop the rest under a storm. The cycle counter is one
    // instruction, so read THAT every time and refresh the seconds only when a second has actually
    // passed. Events within the same second share a stamp, which is exactly the resolution the field
    // has anyway.
    let per_sec = ctx.duration_cycles(1000);
    let mut at_s = ctx.epoch_secs_monotonic() as u32;
    let mut at_tsc = ctx.read_tsc();

    ctx.trace_as("events");
    ctx.log("events: ready (drains its endpoint; holds the IPC trace ring)");

    loop {
        let msg = ctx.recv();
        let b = msg.payload_bytes();
        if b.is_empty() {
            continue;
        }
        match b[0] {
            // An emitted trace event. Fire-and-forget: the sender used `try_send` and did not wait, so
            // a full queue costs the emitter nothing and loses one event - the correct trade for an
            // observability path, and the opposite of the one made on a correctness path.
            TRACE_OP_EVENT if b.len() >= 1 + EV_LEN => {
                let e = &b[1..1 + EV_LEN];
                let tsc = ctx.read_tsc();
                if tsc.wrapping_sub(at_tsc) >= per_sec {
                    at_s = ctx.epoch_secs_monotonic() as u32;
                    at_tsc = tsc;
                }
                if next == RING {
                    next = 0;
                }
                // Overwriting a slot that was never read IS a loss. Counted, and reported.
                if total >= RING as u64 {
                    dropped += 1;
                }
                let mut caller = [0u8; PEER_LEN];
                caller.copy_from_slice(&e[8..8 + PEER_LEN]);
                let mut peer = [0u8; PEER_LEN];
                peer.copy_from_slice(&e[8 + PEER_LEN..8 + 2 * PEER_LEN]);
                ring[next] = Ev {
                    seq: u32::from_le_bytes([e[0], e[1], e[2], e[3]]),
                    // STAMPED HERE, not by the emitter: putting a clock read on every service's
                    // request path made the shell drop keystrokes. This service has to wake to
                    // receive the event anyway, so the cost lands where the job is - and it is the
                    // cached clock above, not a per-event RTC read.
                    at_s,
                    caller,
                    peer,
                    op: e[8 + 2 * PEER_LEN],
                    kind: e[9 + 2 * PEER_LEN],
                };
                next += 1;
                total += 1;
            }
            // `trace ipc` / `trace failures` - the most recent events, oldest of the tail first.
            TRACE_OP_DUMP => {
                let want = if b.len() >= 2 { b[1] as usize } else { 110 };
                let have = (total as usize).min(RING);
                let n = want.min(have).min(110); // 110 x 34 = 3740 B, inside one 4 KiB message
                let mut out = [0u8; 1 + 110 * EV_LEN];
                out[0] = n as u8;
                for i in 0..n {
                    let idx = (next + RING - n + i) % RING;
                    let e = &ring[idx];
                    let o = 1 + i * EV_LEN;
                    out[o..o + 4].copy_from_slice(&e.seq.to_le_bytes());
                    out[o + 4..o + 8].copy_from_slice(&e.at_s.to_le_bytes());
                    out[o + 8..o + 8 + PEER_LEN].copy_from_slice(&e.caller);
                    out[o + 8 + PEER_LEN..o + 8 + 2 * PEER_LEN].copy_from_slice(&e.peer);
                    out[o + 8 + 2 * PEER_LEN] = e.op;
                    out[o + 9 + 2 * PEER_LEN] = e.kind;
                }
                reply(&ctx, &out[..1 + n * EV_LEN]);
            }
            // `trace status` - capacity / recorded / dropped.
            TRACE_OP_STATUS => {
                let mut out = [0u8; 24];
                out[0..8].copy_from_slice(&(RING as u64).to_le_bytes());
                out[8..16].copy_from_slice(&total.to_le_bytes());
                out[16..24].copy_from_slice(&dropped.to_le_bytes());
                reply(&ctx, &out);
            }
            // A published metric sample. Same fire-and-forget contract as an event: the publisher
            // used `try_send` and did not wait, so this must never be the reason a service is slow.
            TRACE_OP_METRIC if b.len() >= 1 + MET_LEN => {
                let m = &b[1..1 + MET_LEN];
                let tsc = ctx.read_tsc();
                if tsc.wrapping_sub(at_tsc) >= per_sec {
                    at_s = ctx.epoch_secs_monotonic() as u32;
                    at_tsc = tsc;
                }
                let mut owner = [0u8; PEER_LEN];
                owner.copy_from_slice(&m[0..PEER_LEN]);
                let mut name = [0u8; MET_NAME_LEN];
                name.copy_from_slice(&m[PEER_LEN..PEER_LEN + MET_NAME_LEN]);
                let v = PEER_LEN + MET_NAME_LEN;
                let value = u64::from_le_bytes([
                    m[v], m[v + 1], m[v + 2], m[v + 3], m[v + 4], m[v + 5], m[v + 6], m[v + 7],
                ]);
                match mets[..nmets].iter().position(|e| e.owner == owner && e.name == name) {
                    Some(i) => {
                        mets[i].value = value;
                        mets[i].at_s = at_s;
                    }
                    None if nmets < METRICS => {
                        mets[nmets] = Met { owner, name, value, at_s };
                        nmets += 1;
                    }
                    // FULL. Refused, counted, and said ONCE - saying it per sample would flood the
                    // very channel an operator needs to read the warning on. The count is served in
                    // the dump, so the number of refusals is recoverable even after the one line
                    // has scrolled away.
                    None => {
                        mets_full += 1;
                        if !mets_full_said {
                            mets_full_said = true;
                            ctx.log("events: metric table FULL (64) - further samples with new names are refused, not dropped silently; `trace metrics` reports the count");
                        }
                    }
                }
            }
            // `trace metrics` - the whole table.
            TRACE_OP_METRICS => {
                // SELF-OBSERVATION IS A LOCAL WRITE, NEVER A MESSAGE. This service cannot publish its
                // own numbers the way every other service does: `ctx.metric()` sends, a send is a
                // reportable event, and a sink that reported its own reporting would fill with itself.
                // So it writes its rows straight into the table it already owns, here, at read time -
                // no timer, no hop, and no recursion possible (`docs/observability.md` 9).
                //
                // Its own DEATH is still not in here, and cannot be: the supervisor's death
                // notification and the kernel's unconditional serial write are what report that, and
                // both sit beneath this service rather than inside it.
                // `metrics.held` is written LAST, after the others are in, because it counts the
                // table it is itself a row of. Read before the insertions it reported 2 while the
                // table held 6 - an instrument off by its own contribution, which is worse than no
                // instrument because it looks authoritative.
                let mine: [(&str, u64); 3] = [
                    ("ring.recorded", total),
                    ("ring.dropped", dropped),
                    ("metrics.refused", mets_full),
                ];
                for (n, v) in mine {
                    let mut name = [0u8; MET_NAME_LEN];
                    let k = n.len().min(MET_NAME_LEN);
                    name[..k].copy_from_slice(&n.as_bytes()[..k]);
                    let mut owner = [0u8; PEER_LEN];
                    owner[.."events".len()].copy_from_slice(b"events");
                    match mets[..nmets].iter().position(|e| e.owner == owner && e.name == name) {
                        Some(i) => {
                            mets[i].value = v;
                            mets[i].at_s = at_s;
                        }
                        None if nmets < METRICS => {
                            mets[nmets] = Met { owner, name, value: v, at_s };
                            nmets += 1;
                        }
                        None => {}
                    }
                }

                // Now the self-count, with every other row already present - including the three
                // just added, and including this one once it exists.
                {
                    let mut name = [0u8; MET_NAME_LEN];
                    name[.."metrics.held".len()].copy_from_slice(b"metrics.held");
                    let mut owner = [0u8; PEER_LEN];
                    owner[.."events".len()].copy_from_slice(b"events");
                    match mets[..nmets].iter().position(|e| e.owner == owner && e.name == name) {
                        Some(i) => {
                            mets[i].value = nmets as u64;
                            mets[i].at_s = at_s;
                        }
                        None if nmets < METRICS => {
                            // +1 counts the row being added by this very statement.
                            mets[nmets] = Met { owner, name, value: nmets as u64 + 1, at_s };
                            nmets += 1;
                        }
                        None => {}
                    }
                }

                // The whole table, always: METRICS is 64 and 64 x 44 + 1 = 2817 B, comfortably inside
                // one 4 KiB message. No second cap to keep in step with the first (§26.4).
                let n = nmets;
                let mut out = [0u8; 1 + METRICS * MET_OUT];
                out[0] = n as u8;
                for i in 0..n {
                    let e = &mets[i];
                    let o = 1 + i * MET_OUT;
                    out[o..o + PEER_LEN].copy_from_slice(&e.owner);
                    out[o + PEER_LEN..o + PEER_LEN + MET_NAME_LEN].copy_from_slice(&e.name);
                    out[o + PEER_LEN + MET_NAME_LEN..o + MET_LEN].copy_from_slice(&e.value.to_le_bytes());
                    out[o + MET_LEN..o + MET_LEN + 4].copy_from_slice(&e.at_s.to_le_bytes());
                }
                reply(&ctx, &out[..1 + n * MET_OUT]);
            }
            // A LOG COPY. The line already reached serial and the kernel ring via syscall 5 before
            // this arrived - this is the queryable duplicate, and losing it costs scrollback and
            // nothing else.
            TRACE_OP_LOG if b.len() > 1 + PEER_LEN => {
                let owner = &b[1..1 + PEER_LEN];
                let text = &b[1 + PEER_LEN..];
                let olen = owner.iter().position(|&c| c == 0).unwrap_or(PEER_LEN);
                let mut push = |byte: u8, head: &mut usize, wrapped: &mut bool| {
                    logbuf[*head] = byte;
                    *head += 1;
                    if *head == LOG_BYTES {
                        *head = 0;
                        *wrapped = true;
                    }
                };
                // STORED AS A RECORD: `owner US text NL`, where US is 0x1F.
                //
                // The owner is a FIELD, not a prefix glued onto the text. That is what lets the reader
                // hand this to the record pipeline - `events log | where owner=fs` uses the same `where`
                // every other view uses, instead of a filter hand-rolled in the shell for this one view.
                // The first attempt did glue them together, and the bespoke filter that had to follow
                // was both duplicated machinery and wrong.
                //
                // It also removes a problem that only shows up on real services: `dwc2` logs its lines
                // as `dwc2-svc: ...`, so any "does the text already name its owner" test has to guess
                // where the name ends. With a separate field there is nothing to guess.
                for &c in &owner[..olen] {
                    push(c, &mut loghead, &mut logwrapped);
                }
                push(0x1f, &mut loghead, &mut logwrapped);
                for &c in text {
                    // A newline inside the text would split one line into two on read-back, so it is
                    // rendered as a space. The line is one line because one `ctx.log` call made it.
                    push(if c == b'\n' { b' ' } else { c }, &mut loghead, &mut logwrapped);
                }
                push(b'\n', &mut loghead, &mut logwrapped);
                loglines += 1;
            }
            // `events log` - the tail of the scrollback.
            // `events log [since]` - the window, or only what is new since a cursor.
            //
            // THE CURSOR IS WHAT MAKES REPEATED DRAINING POSSIBLE. Without it every read returns the
            // whole 8 KiB window, so anything appending in a loop re-writes lines it already wrote.
            // A `recorder` draining every few seconds needs to ask "what is new", and to be TOLD when
            // the answer is incomplete.
            //
            // No per-line sequence is stored. Lines are appended in order and the ring holds the most
            // recent, so the sequences present are contiguous and end at `loglines` - counting the
            // lines currently held gives the oldest one still here. That costs a walk of at most 8 KiB
            // on a query, which is rare, and saves eight bytes on every line, which is not.
            TRACE_OP_LOGS => {
                let since = if b.len() >= 9 {
                    u64::from_le_bytes([b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8]])
                } else {
                    0
                };
                let have = if logwrapped { LOG_BYTES } else { loghead };
                let n = have.min(LOG_REPLY_MAX);
                let mut buf = [0u8; LOG_REPLY_MAX];
                let start = (loghead + LOG_BYTES - n) % LOG_BYTES;
                for i in 0..n {
                    buf[i] = logbuf[(start + i) % LOG_BYTES];
                }
                // DROP A PARTIAL FIRST LINE. Starting mid-sentence reads as corruption rather than as
                // a window, and a reader cannot tell the two apart.
                let mut skip = 0usize;
                if n < have || logwrapped {
                    while skip < n && buf[skip] != b'\n' {
                        skip += 1;
                    }
                    if skip < n {
                        skip += 1;
                    }
                }
                let view = &buf[skip..n];
                let held = view.iter().filter(|&&c| c == b'\n').count() as u64;
                let oldest = loglines.saturating_sub(held) + 1;

                // Skip the lines this caller has already seen. A caller whose cursor is BEHIND
                // `oldest` lost lines to the wrap; it learns that from `oldest` in the reply rather
                // than by noticing a gap it cannot see (invariant 12).
                let already = if since >= oldest { since - oldest + 1 } else { 0 };
                let mut from = 0usize;
                let mut passed = 0u64;
                if already > 0 {
                    for (i, &c) in view.iter().enumerate() {
                        if c == b'\n' {
                            passed += 1;
                            if passed >= already {
                                from = i + 1;
                                break;
                            }
                        }
                    }
                    if passed < already {
                        from = view.len();
                    }
                }
                let body = &view[from..];

                let mut out = [0u8; 25 + LOG_REPLY_MAX];
                out[0..8].copy_from_slice(&loglines.to_le_bytes());   // next cursor
                out[8..16].copy_from_slice(&oldest.to_le_bytes());    // oldest still held
                out[16..24].copy_from_slice(&held.to_le_bytes());     // lines in the window
                out[24] = logwrapped as u8;
                out[25..25 + body.len()].copy_from_slice(body);
                reply(&ctx, &out[..25 + body.len()]);
            }
            // Anything else is drained and dropped, which is the job this service already had: an
            // unconsumed endpoint fills at 16 and never empties.
            _ => {}
        }
    }
}
