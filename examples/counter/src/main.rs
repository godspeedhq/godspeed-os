//! counter - a STATEFUL service that survives its own restart by persisting
//! state externally (§14 restart, §15 state/persistence).
//!
//! Every other example is stateless - it restarts trivially because it owns
//! nothing to lose. `counter` owns a running count, which is exactly the thing a
//! restart would erase if it lived only in RAM. The teaching point is the
//! LIFECYCLE that makes a stateful service restartable anyway:
//!
//!   load-on-spawn  - reconstruct the count from a file in `fs` (the durable copy)
//!   save-on-change - write the new count back to `fs` after every increment
//!
//! With both halves in place, a kill + respawn RECOVERS the count: the kernel
//! notifies the supervisor of the death, the supervisor respawns `counter`, and
//! the fresh instance loads the persisted value and continues from there. State
//! belongs to the service, not the kernel (§15); a restartable service that holds
//! state must externalize it (Commandment IX). `fs` owns the one durable copy
//! (Commandment III); `counter` reconstructs FROM it, never the other way round.
//!
//! `counter` is no more special than any other service (Commandment V): its death
//! is a supervisor restart, not a reboot. It degrades gracefully when `fs` is
//! unreachable - it still RUNS, the count just lives in RAM and will not survive a
//! restart, and it says so loudly (§2.4 loud failures, never a silent fallback).

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message};

// ── fs file-API wire protocol (client <-> fs) ───────────────────────────────────
// MUST match `services/fs` and the shell's fs helpers (services/shell/src/main.rs).
// A request is `[op, path_len:u8, path[path_len], data…]`; the reply's first byte
// is a status code. We only need whole-file read and write here.
const OP_WRITE_FILE: u8 = 10; // [op, plen, path, data]            -> [FS_OK]
const OP_READ_FILE:  u8 = 11; // [op, plen, path]                  -> [FS_OK, n:u32, bytes]
const FS_OK: u8 = 0; // operation succeeded (any other status -> no durable value to load)

/// The durable copy of our state. `fs` owns it; we only read and overwrite it.
const COUNTER_PATH: &[u8] = b"/counter.dat";

/// Approximate pacing between ticks. Sleeping conserves CPU; it does NOT determine
/// correctness (Commandment VIII - we never rely on timing for the count itself,
/// only to avoid spinning). Cycle granularity is one scheduler quantum (~10 ms);
/// the exact value is not portable across hosts and does not need to be.
const TICK_CYCLES: u64 = 2_000_000_000; // ~1 s at ~2 GHz

// ── fs round-trips, modelled on the shell's `fs_request` ────────────────────────

/// Send one fs file-API request `[op, path_len, path, data]` and return the reply.
///
/// Uses `request_with_reply`, which embeds a per-request reply cap (a SEND|GRANT
/// copy of our own endpoint cap) so `fs` can answer us - the same mechanism the
/// shell uses. On a miss (usually `fs` restarted and our cached cap is now
/// `EndpointDead`, §14.3) we reacquire a fresh `fs` cap by NAME via the kernel
/// directory and retry once. Reacquire-and-retry IS the recovery contract: a
/// client whose dependency restarts reacquires and retries, it does not crash
/// (Commandment IX, §14.3).
fn fs_request(ctx: &ServiceContext, op: u8, path: &[u8], data: &[u8]) -> Option<Message> {
    let pl = path.len().min(255);
    let mut req = [0u8; 64];
    req[0] = op;
    req[1] = pl as u8;
    req[2..2 + pl].copy_from_slice(&path[..pl]);
    let dn = data.len().min(req.len() - 2 - pl);
    req[2 + pl..2 + pl + dn].copy_from_slice(&data[..dn]);
    let msg = Message::from_bytes(&req[..2 + pl + dn]);

    if let Some(r) = ctx.request_with_reply("fs", &msg) {
        return Some(r);
    }
    if ctx.reacquire_by_name("fs") {
        return ctx.request_with_reply("fs", &msg);
    }
    None
}

/// LOAD-ON-SPAWN: read the persisted count from `fs` and reconstruct it.
///
/// Returns:
///   - `Some(n)` - the saved count, parsed from the file's 8 little-endian bytes;
///   - `Some(0)` - the file exists but is empty/short (treat as a fresh start);
///   - `None`    - `fs` is unreachable or has no filesystem; the caller degrades.
///
/// We wait for the persisted TRUTH and parse the real value - we do not assume a
/// number (Commandment VIII). The count is the source of truth in `fs`; this is
/// the only place we decide what it is on startup.
fn load_count(ctx: &ServiceContext) -> Option<u64> {
    let reply = fs_request(ctx, OP_READ_FILE, COUNTER_PATH, &[])?;
    let p = reply.payload_bytes();
    match p.first() {
        Some(&FS_OK) if p.len() >= 5 => {
            // Reply layout: [FS_OK, n:u32, bytes…]; `bytes` holds our 8-byte LE u64.
            let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
            let body = &p[5..(5 + n).min(p.len())];
            if body.len() >= 8 {
                Some(u64::from_le_bytes([
                    body[0], body[1], body[2], body[3],
                    body[4], body[5], body[6], body[7],
                ]))
            } else {
                // File present but not a full count yet - start fresh, still persisting.
                Some(0)
            }
        }
        // FS_NOFS or any non-OK status (e.g. file absent): no durable value to load.
        _ => None,
    }
}

/// SAVE-ON-CHANGE: overwrite `/counter.dat` with the new count (8 LE bytes).
///
/// `true` if `fs` acknowledged the write. A successful write means the value is
/// committed to the durable copy `fs` owns, so the next spawn can recover it.
fn save_count(ctx: &ServiceContext, count: u64) -> bool {
    matches!(
        fs_request(ctx, OP_WRITE_FILE, COUNTER_PATH, &count.to_le_bytes()),
        Some(r) if r.payload_bytes().first() == Some(&FS_OK)
    )
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("counter: ready");

    // Probe whether `fs` is reachable. `acquire_send_cap` resolves "fs" by name via
    // the kernel directory and installs a SEND cap; `None` means `fs` has not come
    // up (or we hold no authority to send to it). Either way we keep running - the
    // count just will not persist (graceful, loud degrade, never a silent fallback).
    let mut persist = ctx.acquire_send_cap("fs").is_some();

    // LOAD-ON-SPAWN. After a restart this is what makes the count survive: the fresh
    // instance reconstructs its state from the durable copy instead of starting over.
    let mut count: u64 = if persist {
        match load_count(&ctx) {
            Some(saved) => {
                ctx.log_fmt(format_args!("counter: recovered count={} from /counter.dat", saved));
                saved
            }
            None => {
                // fs is up but the file is absent (first ever boot) or unreadable.
                ctx.log("counter: no saved count yet - starting at 0");
                0
            }
        }
    } else {
        ctx.log("counter: fs unavailable - starting at 0 (state will NOT persist)");
        0
    };

    // SAVE-ON-CHANGE loop. Each tick: increment, then commit to fs. The increment is
    // pure (no timing dependence); the sleep only paces CPU (Commandment VIII).
    loop {
        count = count.wrapping_add(1);

        if persist {
            if save_count(&ctx, count) {
                ctx.log_fmt(format_args!("counter: count={} saved", count));
            } else {
                // The save failed - most likely `fs` is mid-restart. Drop to in-RAM
                // mode and try to reacquire `fs` for the next tick (reacquire + retry,
                // §14.3). The count keeps advancing; it just isn't durable right now.
                ctx.log_fmt(format_args!("counter: count={} (save failed - fs degraded)", count));
                persist = ctx.acquire_send_cap("fs").is_some();
            }
        } else {
            ctx.log_fmt(format_args!("counter: count={} (in-RAM only)", count));
            // Keep trying to bring fs back; once it answers, future ticks persist again.
            persist = ctx.acquire_send_cap("fs").is_some();
        }

        // Pace the loop. `sleep` lets the core halt instead of busy-yielding.
        ctx.sleep(TICK_CYCLES);
    }
}
