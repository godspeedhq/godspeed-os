// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! `chaos` - the system-stress orchestrator, spawned on demand by the shell's `chaos max-carnage`
//! command. It exists so the SHELL can be a chaos target: the loop that kills and resurrects services
//! cannot live inside the shell (a shell killing itself dies on round one), so it lives here, in a
//! separate task. `chaos` is the one program a run never kills - it excludes ITSELF from its victim
//! pool the way the loop used to exclude "shell". The two untouchables during a run are `chaos` and
//! the kernel; everything else, the shell included, is fair game and recovers.
//!
//! It claims exclusive console input (the foreground primitive, syscall 40) so a resurrected shell
//! polling the keyboard cannot swallow its `q`-to-quit, runs the carnage loop, and on `q` ensures a
//! live shell exists, releases the foreground, and self-terminates so a finished run leaves nothing
//! behind. You watch the kills/recoveries climb and the shell's `gsh>` blink out and back.
//!
//! Phase 2b (this commit): the kill / flood / kill-then-flood loop with recovery confirmation, the
//! per-service tally + report, and the handoff. Run-until-`q`. Deferred, clearly scoped follow-ups:
//! the mem-pressure + spawn-burst dimensions, a round-count (an IPC handshake from the shell), and
//! the nicer 20-line scrolling TUI ring (the heartbeat + report is the functional first display).

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, CapHandle, Message, IpcError};

// Tuning - mirrors the shell's former max-carnage, bounded (§26.6).
const RESTARTABLE: [&str; 7] = ["supervisor", "block-driver", "fs", "xhci", "ehci", "logger", "shell"];
const RECOVER_SECS: i64 = 8;        // wall-clock bound (RTC, portable) to confirm a victim respawned
const POLL_EVERY: u32 = 64;         // yields between recovery/clock polls
const MAX_CAND: usize = 32;         // bounded snapshot of live killable tasks per round
const MAX_SVC: usize = 16;          // distinct services in the aggregate tally (~6-8 real)
const SHELL_SETTLE_YIELDS: u32 = 4000; // let a freshly-respawned shell settle before we hand back

fn xorshift64(mut x: u64) -> u64 { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x }

fn str_of(b: &[u8]) -> &str { core::str::from_utf8(b).unwrap_or("?") }

/// Slot of the live task named `name`, or None.
fn slot_of(ctx: &ServiceContext, name: &str) -> Option<u32> {
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if st.valid && st.state != 4 /* Dead */ && st.name_str() == name { return Some(slot); }
    }
    None
}

/// Restart count of the live task named `name` - the recovery signal: a value different from a pre-kill
/// reading proves a NEW instance came up (§7.5). None if not running.
fn restart_of(ctx: &ServiceContext, name: &str) -> Option<u32> {
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if st.valid && st.state != 4 && st.name_str() == name { return Some(st.restart_count as u32); }
    }
    None
}

/// Wait (wall-clock bounded, RTC - portable across QEMU/hardware) for `name` to reach a restart count
/// different from `og`, proving a fresh instance came up. Yields cooperatively so the recoverer (which
/// shares core 0) runs. Returns true on recovery, false on timeout.
fn wait_recovery(ctx: &ServiceContext, name: &str, og: u32) -> bool {
    let t0 = ctx.datetime().epoch_secs();
    let mut k = 0u32;
    loop {
        ctx.yield_cpu();
        k += 1;
        if k % POLL_EVERY == 0 {
            if let Some(g) = restart_of(ctx, name) { if g != og { return true; } }
            if ctx.datetime().epoch_secs() - t0 >= RECOVER_SECS { return false; }
        }
    }
}

/// One flood pass: get-or-reuse a cached SEND cap to `name`, then burst `try_send` (never blocking
/// `send`, §8.9) until the queue saturates, the service dies, or we hit the burst cap. Returns
/// `(sent, saturated, died)`, or None if unreachable. Reclaims the dead cap on `EndpointDead` BEFORE
/// clearing the cache, else a long run leaks a slot per flood-death and fills the 64-slot cap table.
fn flood(ctx: &ServiceContext, name: &str, cache: &mut Option<CapHandle>) -> Option<(u32, bool, bool)> {
    const BURST: u32 = 64; // > queue depth (16) so saturation shows
    let h = match *cache {
        Some(h) => h,
        None => match ctx.acquire_send_cap(name) { Some(h) => { *cache = Some(h); h } None => return None },
    };
    let msg = Message::from_bytes(&[0x01]); // minimal benign payload; the target drains + drops it
    let (mut sent, mut sat, mut died) = (0u32, false, false);
    while sent < BURST {
        match ctx.try_send_by_handle(h, &msg) {
            Ok(())                      => sent += 1,
            Err(IpcError::QueueFull)    => { sat = true; break; }
            Err(IpcError::EndpointDead) => { died = true; ctx.remove_cap(h); *cache = None; break; }
            Err(_)                      => break,
        }
    }
    Some((sent, sat, died))
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Take the keyboard so a resurrected shell cannot steal our `q`. This is the moment the shell goes
    // "muted" for the duration of the run (unclaimed is the normal state, so this changes nothing else).
    ctx.claim_console_foreground();
    ctx.console_writeln(
        "chaos max-carnage: kill / FLOOD / kill-then-flood a random LIVE service each round - the SHELL included now. Press q to quit.");

    // RNG seed: the TSC mixed with the wall clock; never zero.
    let mut rng = ctx.read_tsc()
        ^ (ctx.datetime().epoch_secs() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    if rng == 0 { rng = 0xDEAD_BEEF_CAFE_F00D; }

    // Per-service aggregate tally (bounded; constant memory regardless of round count).
    let mut sv_name:    [[u8; 24]; MAX_SVC] = [[0u8; 24]; MAX_SVC];
    let mut sv_nlen:    [usize;    MAX_SVC] = [0usize;    MAX_SVC];
    let mut sv_killed:  [u64;      MAX_SVC] = [0u64;      MAX_SVC];
    let mut sv_recov:   [u64;      MAX_SVC] = [0u64;      MAX_SVC];
    let mut sv_flooded: [u64;      MAX_SVC] = [0u64;      MAX_SVC];
    let mut sv_floodcap:[Option<CapHandle>; MAX_SVC] = [None; MAX_SVC];
    let mut nsv = 0usize;

    let (mut round, mut killed, mut recovered, mut flooded) = (0u64, 0u64, 0u64, 0u64);

    loop {
        // `q` aborts. The kernel buffers the keypress across input-driver churn, so it is caught here.
        if let Some(b) = ctx.try_console_read() { if b == b'q' || b == b'Q' { break; } }

        // Snapshot the live, killable set: valid, not Dead, named, and NOT chaos itself (the one program
        // a run never kills - it is the thing doing the killing).
        let mut cand: [([u8; 24], usize, u32); MAX_CAND] = [([0u8; 24], 0usize, 0u32); MAX_CAND];
        let mut ncand = 0usize;
        for slot in 0..256u32 {
            let st = ctx.task_stat(slot);
            if !st.valid || st.state == 4 { continue; }
            let nm = st.name_str();
            if nm.is_empty() || nm == "chaos" { continue; }
            if ncand < MAX_CAND {
                let b = nm.as_bytes(); let l = b.len().min(24);
                cand[ncand].0[..l].copy_from_slice(&b[..l]);
                cand[ncand].1 = l;
                cand[ncand].2 = st.restart_count as u32;
                ncand += 1;
            }
        }
        if ncand == 0 { ctx.yield_cpu(); continue; } // nothing but us
        round += 1;

        rng = xorshift64(rng);
        let pick = (rng % ncand as u64) as usize;
        let nl = cand[pick].1;
        let og = cand[pick].2;
        let mut nbuf = [0u8; 24];
        nbuf[..nl].copy_from_slice(&cand[pick].0[..nl]);
        let name = str_of(&nbuf[..nl]);

        // Find-or-add the per-service tally slot (so kill + flood tally to one slot and share its cap).
        let mut idx = None;
        for s in 0..nsv { if sv_name[s][..sv_nlen[s]] == nbuf[..nl] { idx = Some(s); break; } }
        let idx = match idx {
            Some(s) => Some(s),
            None if nsv < MAX_SVC => {
                sv_name[nsv][..nl].copy_from_slice(&nbuf[..nl]); sv_nlen[nsv] = nl;
                let s = nsv; nsv += 1; Some(s)
            }
            None => None,
        };

        // Roll the action: 0 = kill, 1 = flood, 2 = kill-then-flood.
        rng = xorshift64(rng);
        let action = rng % 3;
        if let Some(s) = idx {
            if action == 1 || action == 2 {
                if flood(&ctx, name, &mut sv_floodcap[s]).is_some() { flooded += 1; sv_flooded[s] += 1; }
            }
            if action == 0 || action == 2 {
                let _ = ctx.kill(name);
                killed += 1; sv_killed[s] += 1;
                // The kill bumped the endpoint generation; the cached flood cap is now stale. RECLAIM it
                // (don't just drop the handle) so a long run does not fill the 64-slot cap table.
                if let Some(h) = sv_floodcap[s].take() { ctx.remove_cap(h); }
                // Confirm recovery for the restartable set (shell included - it respawns a fresh prompt).
                if RESTARTABLE.contains(&name) && wait_recovery(&ctx, name, og) {
                    recovered += 1; sv_recov[s] += 1;
                }
            }
        }

        // In-place heartbeat. The console is the kernel's framebuffer/serial (not a service), so it
        // survives the carnage; `\r` rewrites the line in place, trailing spaces clear the previous one.
        if round % 16 == 0 {
            ctx.console_write_fmt(format_args!(
                "\rmax-carnage: {} rounds - {} kills, {} recovered, {} floods - kernel alive - q to quit    ",
                round, killed, recovered, flooded));
        }
    }

    ctx.console_writeln(""); // end the in-place heartbeat line before the report
    ctx.console_writeln("=== chaos max-carnage: report ===");
    for s in 0..nsv {
        let nm = str_of(&sv_name[s][..sv_nlen[s]]);
        ctx.console_writeln_fmt(format_args!(
            "  {:<14} killed {:>5}  recovered {:>5}  flooded {:>5}", nm, sv_killed[s], sv_recov[s], sv_flooded[s]));
    }
    ctx.console_writeln_fmt(format_args!(
        "total: {} rounds, {} kills, {} recovered, {} floods. kernel: alive (this command returned).",
        round, killed, recovered, flooded));

    // Reclaim any flood caps still cached, so the run leaves the cap table as it found it.
    for c in sv_floodcap.iter_mut() { if let Some(h) = c.take() { ctx.remove_cap(h); } }

    // HANDOFF - the order is load-bearing. Our last kill may have just taken the shell, so FIRST wait
    // (bounded) for a live shell to hand the keyboard back to, THEN release the foreground so that shell
    // resumes reading, THEN self-terminate so a finished run leaves no chaos task behind. Releasing
    // before a live shell exists would leave a window with no keyboard owner.
    let t0 = ctx.datetime().epoch_secs();
    let mut k = 0u32;
    while slot_of(&ctx, "shell").is_none() {
        ctx.yield_cpu(); k += 1;
        if k % POLL_EVERY == 0 && ctx.datetime().epoch_secs() - t0 >= RECOVER_SECS { break; }
    }
    for _ in 0..SHELL_SETTLE_YIELDS { ctx.yield_cpu(); } // let the fresh shell settle
    ctx.release_console_foreground();
    ctx.console_writeln("chaos: done - foreground returned to the shell");

    // Self-terminate (we hold SERVICE_CONTROL). chaos is not in any auto-restart set, so it stays dead
    // until the next `chaos max-carnage` spawns a fresh one. The park() is an unreachable safety net for
    // the `-> !` return type (a successful self-kill never returns here).
    let _ = ctx.kill("chaos");
    ctx.park();
}
