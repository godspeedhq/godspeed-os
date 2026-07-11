// SPDX-License-Identifier: GPL-2.0-only
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
//! behind. You watch the per-service kill/flood counts climb and the shell's `gsh>` blink out and back.
//!
//! `max-carnage` takes an explicit TARGET: `all` aims kill/flood at random live services, a service
//! name aims them at THAT service every round. Attacks split two ways: AIMED (kill-storm, flood-storm)
//! are directed at a victim and shown as per-service table rows; SYSTEM-WIDE (mem-pressure = chaos's own
//! alloc_mem, spawn-storm = mem-pressure tasks) pressure the shared allocator + task pool, so they cannot be aimed
//! at one service and show as a single `system:` footer line. It does NOT track "recovered": that would
//! mean asking the (also pummeled) supervisor for ground truth mid-storm. The one truth it fetches is the
//! live victim list (for "all-services"). Each round it logs a one-line trail to the serial: the panel
//! overwrites itself, so the SERIAL log is the scrolling history for troubleshooting. `q` to abort must
//! come from the SERIAL console - chaos kills the USB keyboard drivers, so the kernel-owned UART is the
//! only surviving input (a loud `[y/N]` warning precedes the run).

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, CapHandle, Message, IpcError};
use core::fmt::Write as _;

// Tuning - mirrors the shell's former max-carnage, bounded (§26.6).
const RECOVER_SECS: i64 = 8;        // wall-clock bound (RTC, portable) for the handoff's shell-wait
const POLL_EVERY: u32 = 64;         // yields between clock polls in the handoff wait
const MAX_CAND: usize = 32;         // bounded snapshot of live killable tasks per round
const MAX_SVC: usize = 16;          // distinct services in the aggregate tally (~6-8 real)
const SHELL_SETTLE_YIELDS: u32 = 4000; // let a freshly-respawned shell settle before we hand back
const PACE_YIELDS: u32 = 3000;      // a beat between rounds so the panel/log stay readable + `q` lands
const MEMP_CHUNK: usize = 64 * 1024; // one mem-pressure round allocs this (held; chaos's limit bounds it)
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]; // matches the `date` utility

// Chaos DISCOVERS its victims by scanning the live task table each round - it never hardcodes a service
// list. A hardcoded set goes stale the moment the running set changes (a driver the hardware lacks, like
// `ehci` on a box with no EHCI controller; or a newly added service), producing phantom "kills" of a
// service that was never alive. The only names the scan skips are chaos's OWN untouchables: `chaos`
// itself (a self-kill would end the run) and the transient tasks it spawns as attacks (`mem-pressure`,
// including the spawn-storm) or that are ephemeral (`observe-*`). Everything else that is live is fair
// game - and NOTHING is protected-last: the supervisor is a normal victim at any point, because the
// fixed-point robustness must recover from ANY kill order (Test 15 / Cmd II - let Chaos try its
// hardest); random destruction is the honest stress.
fn is_transient(name: &str) -> bool {
    name == "chaos" || name == "mem-pressure" || name.starts_with("observe")
}

// A tiny xorshift64 PRNG - there is no std rng in no_std. Seeded from the RTC start time (varies run to
// run; the TSC is broken on the T630 so we do NOT seed from it) and advanced every round, so the random
// subset differs both run-to-run and round-to-round. Bounded, no heap.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self { Rng(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed }) }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        self.0 = x; x
    }
}

fn str_of(b: &[u8]) -> &str { core::str::from_utf8(b).unwrap_or("?") }

/// Slot of the live task named `name`, or None.
fn slot_of(ctx: &ServiceContext, name: &str) -> Option<u32> {
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if st.valid && st.state != 4 /* Dead */ && st.name_str() == name { return Some(slot); }
    }
    None
}

/// A bounded frame buffer. The whole panel is built into one of these, then flushed in a couple of
/// `console_write`s (not one per line), in <=240-byte chunks broken AT A NEWLINE: `console_write` caps at
/// 256 bytes (BOTH the SDK wrapper and the kernel syscall), and a chunk must never split a CSI escape /
/// line across two writes. NOTE: do NOT collapse this to one whole-panel write to "stop flicker" - the
/// per-round redraw showing the counters change IS the intended feedback, not a rendering bug, and a
/// >256B write is silently dropped by the SDK cap so the panel vanishes (tried 2026-06-27, reverted).
struct FrameBuf { buf: [u8; 2048], len: usize }
impl FrameBuf {
    fn new() -> Self { Self { buf: [0; 2048], len: 0 } }
    fn flush(&self, ctx: &ServiceContext) {
        let mut s = 0;
        while s < self.len {
            let mut e = (s + 240).min(self.len);
            if e < self.len {
                let mut b = e;
                while b > s && self.buf[b - 1] != b'\n' { b -= 1; }
                if b > s { e = b; }
            }
            if let Ok(st) = core::str::from_utf8(&self.buf[s..e]) { ctx.console_write(st); }
            s = e;
        }
    }
}
impl core::fmt::Write for FrameBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() { if self.len < self.buf.len() { self.buf[self.len] = b; self.len += 1; } }
        Ok(())
    }
}

/// Write a compact, bounded duration ("45s", "1m23s", "2h05m", "3d04h") into the frame: a chaos run can
/// span days, so cascade d/h/m/s and show the two most-significant units. Seconds come from the RTC
/// (year-guarded), so the value is plausible by construction.
fn write_dur(f: &mut FrameBuf, secs: u64) {
    if secs >= 86400 { let _ = write!(f, "{}d{:02}h", secs / 86400, (secs % 86400) / 3600); }
    else if secs >= 3600 { let _ = write!(f, "{}h{:02}m", secs / 3600, (secs % 3600) / 60); }
    else if secs >= 60 { let _ = write!(f, "{}m{:02}s", secs / 60, secs % 60); }
    else { let _ = write!(f, "{}s", secs); }
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
    // The shell launcher sends the round count (always > 0 - the shell requires an explicit count) right
    // after spawning us. Wait briefly (RTC-bounded) for it BEFORE claiming the foreground - the shell is
    // still live to send it. If it never arrives (e.g. the launcher's send failed) rounds stays 0 and the
    // run is a safe no-op (there is no uncapped default), so we never storm unconfigured.
    let mut rounds: u64 = 0;
    let mut tbuf = [0u8; 128];   // target string; may be a comma-separated list, so sized for a bounded list
    let mut tlen = 0usize;
    {
        let t0 = ctx.datetime().epoch_secs();
        loop {
            if let Some(msg) = ctx.try_recv() {
                let b = msg.payload_bytes();
                if b.len() >= 4 { rounds = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64; }
                if b.len() > 4 { let n = (b.len() - 4).min(128); tbuf[..n].copy_from_slice(&b[4..4 + n]); tlen = n; }
                break;
            }
            if ctx.datetime().epoch_secs() - t0 >= 2 { break; }
            ctx.yield_cpu();
        }
    }
    // The TARGET: DEFAULT (no target) = "random" - a RANDOM subset of the restartable set each round (the
    // honest chaos-monkey storm; supervisor is a normal victim, nothing protected-last). "all-services" =
    // a full even sweep of every live service each round; a service name = aim every round at THAT one; a
    // comma-list = kill every listed one each round. mem-pressure + spawn-storm are system-wide in all modes.
    let target: &str = if tlen == 0 { "random" } else { str_of(&tbuf[..tlen]) };
    // all-services = the RANDOM whole-set storm (a random subset each round). The shell now REQUIRES a target
    // (a bare max-carnage is refused there), so tlen==0 should not occur; keep it -> random defensively.
    let target_random = tlen == 0 || target == "all-services";
    let target_all = target == "all-services";     // kept for the serial-only abort warning messages
    // A comma-separated target ("nic-driver,net-stack") is a MULTI-TARGET run: EVERY listed service is
    // killed each round (semantics B - the cascade stress). Parse it once into a bounded fixed array.
    const MAX_TLIST: usize = 8;
    let mut tlist: [[u8; 24]; MAX_TLIST] = [[0u8; 24]; MAX_TLIST];
    let mut tlist_len: [usize; MAX_TLIST] = [0usize; MAX_TLIST];
    let mut ntlist = 0usize;
    let target_list = target.contains(',');
    if target_list {
        for seg in target.split(',') {
            if seg.is_empty() || ntlist >= MAX_TLIST { continue; }
            let sb = seg.as_bytes(); let l = sb.len().min(24);
            tlist[ntlist][..l].copy_from_slice(&sb[..l]); tlist_len[ntlist] = l; ntlist += 1;
        }
    }
    // Three keyboard-abort cases. all-services storms EVERY driver, so the keyboard dies for sure (serial
    // only). A single USB host driver (xhci/ehci) kills the keyboard ONLY if it is the controller yours is
    // on - we cannot know which (two controllers + hot-plug make detection unreliable), so we state the
    // proviso honestly rather than guess. Anything else leaves the keyboard alive (plain keyboard `q`).
    let target_usb = target.split(',').any(|s| s == "xhci" || s == "ehci");

    // Take the keyboard so a resurrected shell cannot steal our `q`. This is the moment the shell goes
    // "muted" for the duration of the run (unclaimed is the normal state, so this changes nothing else).
    ctx.claim_console_foreground();
    // chaos owns the screen now: the foreground gate sends every backgrounded task's console output to
    // serial only, so the muted shell can no longer smear our display. Clear it for a fresh canvas.
    ctx.console_write("\x1b[2J\x1b[H");

    // Per-service aggregate tally (bounded; constant memory regardless of round count).
    let mut sv_name:    [[u8; 24]; MAX_SVC] = [[0u8; 24]; MAX_SVC];
    let mut sv_nlen:    [usize;    MAX_SVC] = [0usize;    MAX_SVC];
    let mut sv_killed:  [u64;      MAX_SVC] = [0u64;      MAX_SVC]; // AIMED, per-service
    let mut sv_flooded: [u64;      MAX_SVC] = [0u64;      MAX_SVC]; // AIMED, per-service
    let mut sv_floodcap:[Option<CapHandle>; MAX_SVC] = [None; MAX_SVC];
    // Why a service's flood column is N/A rather than a misleading 0: 0 = floodable (show the count),
    // 1 = reply-style (we kill it instead - flooding corrupts its reply stream), 2 = no acquirable send
    // endpoint (acquire_send_cap returned None). Discovered at runtime, not hardcoded.
    let mut sv_flood_na: [u8; MAX_SVC] = [0u8; MAX_SVC];
    let mut nsv = 0usize;

    let (mut round, mut killed, mut flooded, mut mempr, mut spawns) = (0u64, 0u64, 0u64, 0u64, 0u64);
    // Wall-clock start (RTC, year-guarded): the datetime for the "started HH:MM:SS" readout, and its epoch
    // for elapsed + the linear ETA (a pure extrapolation of elapsed over round progress, no outside truth).
    let start_dt = ctx.datetime();
    let start_epoch = start_dt.epoch_secs();
    // Seed the random-storm PRNG from the RTC start time (varies per run); advanced each round so the
    // subset differs round-to-round. Only read in the `target_random` branch.
    let mut rng = Rng::new((start_epoch as u64)
        ^ ((start_dt.minute as u64) << 24) ^ ((start_dt.second as u64) << 40));

    'carnage: loop {
        // `q` aborts (round boundary; also polled between each kill/flood in the sweep below, so one q
        // press aborts within a sub-step rather than lagging a whole round). The kernel buffers the
        // keypress across input-driver churn, so it is caught here.
        if let Some(b) = ctx.try_console_read() { if b == b'q' || b == b'Q' { break; } }
        if round >= rounds { break; } // the bounded run is complete (rounds is always > 0 - the shell requires it)

        round += 1;

        // ONE ROUND = ONE SWEEP over EVERY live service, not one random victim. So `all-services N` hits
        // every service N times over, evenly, instead of N scattered pokes landing on a random few (the old
        // behaviour: at N=10 only ~2 services were ever touched). Snapshot the live set: for "all-services"
        // every live task except chaos, the mem-pressure tasks it spawns, and transient observe-*; for a
        // specific target, just that one service.
        let mut cand: [([u8; 24], usize); MAX_CAND] = [([0u8; 24], 0usize); MAX_CAND];
        let mut ncand = 0usize;
        if target_random {
            // DYNAMIC victim set: SCAN the live task table (no hardcoded service list - a driver the
            // hardware lacks simply isn't found, so it can't be phantom-killed) and take a RANDOM subset
            // this round - vary WHICH and HOW MANY, each live non-transient service in with ~50%
            // probability (a fresh PRNG draw). Supervisor included at any point - no protected-last.
            for slot in 0..256u32 {
                if ncand >= MAX_CAND { break; }
                let st = ctx.task_stat(slot);
                if !st.valid || st.state == 4 /* Dead */ { continue; }
                let nm = st.name_str();
                if is_transient(nm) { continue; }
                if (rng.next() & 1) == 1 {
                    let b = nm.as_bytes(); let l = b.len().min(24);
                    cand[ncand].0[..l].copy_from_slice(&b[..l]); cand[ncand].1 = l; ncand += 1;
                }
            }
            // If the coin flips picked nothing, take the first live victim so a round is never a no-op.
            if ncand == 0 {
                for slot in 0..256u32 {
                    let st = ctx.task_stat(slot);
                    if !st.valid || st.state == 4 { continue; }
                    let nm = st.name_str();
                    if is_transient(nm) { continue; }
                    let b = nm.as_bytes(); let l = b.len().min(24);
                    cand[0].0[..l].copy_from_slice(&b[..l]); cand[0].1 = l; ncand = 1;
                    break;
                }
            }
        } else if target_list {
            // Multi-target: every listed service is a candidate this round (all killed below).
            for t in 0..ntlist {
                if ncand < MAX_CAND {
                    let l = tlist_len[t];
                    cand[ncand].0[..l].copy_from_slice(&tlist[t][..l]); cand[ncand].1 = l; ncand += 1;
                }
            }
        } else {
            let tb = target.as_bytes(); let l = tb.len().min(24);
            cand[0].0[..l].copy_from_slice(&tb[..l]); cand[0].1 = l; ncand = 1;
        }

        // Per sweep: FLOOD every floodable service and KILL every reply-style one (shell/fs can't be flooded
        // - it corrupts their reply stream - so a kill is how they get hit). PLUS kill ONE rotating floodable
        // service, walking the set across rounds, so the floodable ones are RESTART-tested too (the other
        // resilience axis), not only drain-tested. cand order can shift as services respawn into new slots,
        // but the rotor still spreads those kills across the floodable set over the run.
        let kill_pick = if ncand > 0 { ((round - 1) % ncand as u64) as usize } else { usize::MAX };
        let (mut sweep_killed, mut sweep_flooded) = (0u64, 0u64);
        for c in 0..ncand {
            // Poll q between each kill/flood: a round can take seconds (services respawn), so checking
            // only at the round top made one q press lag a whole round. Now one press aborts within a
            // sub-step. (`break 'carnage` exits the round loop; the kernel buffers the key across churn.)
            if let Some(b) = ctx.try_console_read() { if b == b'q' || b == b'Q' { break 'carnage; } }
            let nl = cand[c].1;
            let mut nbuf = [0u8; 24]; nbuf[..nl].copy_from_slice(&cand[c].0[..nl]);
            let name = str_of(&nbuf[..nl]);

            // Find-or-add this service's tally slot (its kill + flood counts + cached flood cap share it).
            let mut idx = None;
            for s in 0..nsv { if sv_name[s][..sv_nlen[s]] == nbuf[..nl] { idx = Some(s); break; } }
            if idx.is_none() && nsv < MAX_SVC {
                sv_name[nsv][..nl].copy_from_slice(&nbuf[..nl]); sv_nlen[nsv] = nl;
                idx = Some(nsv); nsv += 1;
            }
            let s = match idx { Some(s) => s, None => continue }; // tally full (won't happen at ~8 services)

            if name == "shell" || name == "fs" {
                // Reply-style: KILL every sweep. The kill bumps the endpoint generation, so reclaim any
                // cached flood cap (RECLAIM, don't drop) or a long run fills the 64-slot cap table.
                let _ = ctx.kill(name);
                killed += 1; sv_killed[s] += 1; sweep_killed += 1;
                sv_flood_na[s] = 1;
                if let Some(h) = sv_floodcap[s].take() { ctx.remove_cap(h); }
            } else {
                // Floodable: FLOOD every sweep...
                match flood(&ctx, name, &mut sv_floodcap[s]) {
                    // Reset na to 0 on success: a floodable service the rotor just killed can be mid-restart
                    // on the next sweep and briefly fail acquire_send_cap (na=2 below); without this reset the
                    // column would stay "N/A (no-ep)" forever while the count keeps climbing - a lying cell.
                    Some(_) => { flooded += 1; sv_flooded[s] += 1; sweep_flooded += 1; sv_flood_na[s] = 0; }
                    None    => { if sv_flood_na[s] == 0 { sv_flood_na[s] = 2; } } // no endpoint right now
                }
                // ...and KILL it: when the rotor lands here (all-services / single target), OR always in a
                // multi-target list run (semantics B) or the RANDOM storm (every service the round PICKED
                // goes down this round).
                if c == kill_pick || target_list || target_random {
                    let _ = ctx.kill(name);
                    killed += 1; sv_killed[s] += 1; sweep_killed += 1;
                    if let Some(h) = sv_floodcap[s].take() { ctx.remove_cap(h); }
                }
            }
        }

        // SYSTEM-WIDE, once per sweep: chaos allocs a chunk of its OWN memory and fires one mem-pressure
        // spawn. Count what chaos FIRED (one of each, every sweep), CONSISTENT with the kill-storm/flood-
        // storm columns above (also fired-counts) - so all four climb with the rounds. The system's RESPONSE
        // bounds the effect, NOT the counter: chaos's own alloc plateaus at its contract memory limit (later
        // allocs return AllocDenied), and only the FIRST mem-pressure TASK survives before memory is
        // exhausted (later spawns are refused). The system holding IS the point; the offense is what we count.
        // (Reclaimed at cleanup below.)
        let _ = ctx.alloc_mem(MEMP_CHUNK); mempr += 1;
        let _ = ctx.spawn("mem-pressure"); spawns += 1;

        // Redraw the per-service TABLE in place. We build the whole frame into one buffer and flush it in
        // a couple of writes (not ~one per line), so the framebuffer redraws without flicker. Home the
        // cursor (NOT `\x1b[J`, which blanks the screen and flickers); each line erases to its end
        // (`\x1b[K`) so a shorter value overwrites cleanly. The table only GROWS (a service appends the
        // first time chaos hits it), so the in-place redraw never leaves a stale row below it. The table
        // shows only what chaos FIRED - the kills + floods it itself counted - plus the global spawn count.
        let mut f = FrameBuf::new();
        let _ = write!(f, "\x1b[H");
        let _ = write!(f, "  C H A O S   max-carnage    target: {}    ({})\x1b[K\r\n",
            target, if target_all || target_random { "'q' to quit via SERIAL" } else { "'q' to quit" });
        // rounds is always > 0 - the shell requires an explicit count; there is no uncapped run.
        let pct = round * 100 / rounds; // round is 1..=rounds, so pct is 1..=100
        let _ = write!(f, "  round {} / {} ({}%)\x1b[K\r\n", round, rounds, pct);
        // Wall-clock status line: when it began, how long it has run, and a linear ETA (no outside truth -
        // a pure extrapolation of elapsed over round progress). until-q has no total, so remains is n/a.
        let elapsed = (ctx.datetime().epoch_secs() - start_epoch).max(0) as u64;
        let _ = write!(f, "  started {} {:04}-{:02}-{:02} {:02}:{:02}:{:02}  |  elapsed ",
            WEEKDAYS[(start_dt.weekday() as usize) % 7], start_dt.year, start_dt.month, start_dt.day,
            start_dt.hour, start_dt.minute, start_dt.second);
        write_dur(&mut f, elapsed);
        if rounds > 0 && round > 0 {
            let _ = write!(f, "  |  remains ~");
            write_dur(&mut f, elapsed * rounds.saturating_sub(round) / round);
        } else {
            let _ = write!(f, "  |  remains n/a");
        }
        let _ = write!(f, "\x1b[K\r\n");
        let _ = write!(f, "  ----------------------------------------------------\x1b[K\r\n");
        let _ = write!(f, "  {:<16} {:>10} {:>11}\x1b[K\r\n", "service", "kill-storm", "flood-storm");
        for s in 0..nsv {
            let nm = str_of(&sv_name[s][..sv_nlen[s]]);
            // flood cell: a LOUD N/A (with the reason) where flooding does not apply, else the count. A
            // bare 0 reads as "tried, got nothing"; this says "not applicable, and why".
            match sv_flood_na[s] {
                1 => { let _ = write!(f, "  {:<16} {:>10} {:>11}\x1b[K\r\n", nm, sv_killed[s], "N/A (reply)"); }
                2 => { let _ = write!(f, "  {:<16} {:>10} {:>11}\x1b[K\r\n", nm, sv_killed[s], "N/A (no-ep)"); }
                _ => { let _ = write!(f, "  {:<16} {:>10} {:>11}\x1b[K\r\n", nm, sv_killed[s], sv_flooded[s]); }
            }
        }
        let _ = write!(f, "  ----------------------------------------------------\x1b[K\r\n");
        let _ = write!(f, "  flood N/A: reply = killed instead (reply-style); no-ep = no send endpoint\x1b[K\r\n");
        let _ = write!(f, "  system:  mem-pressure {}   spawn-storm {}\x1b[K\r\n", mempr, spawns);
        if target_all || target_random {
            let _ = write!(f, "  kernel: ALIVE   abort: 'q' in the SERIAL console (keyboard dead)\x1b[K\r\n");
        } else if target_usb {
            let _ = write!(f, "  kernel: ALIVE   abort: 'q' on the keyboard, or SERIAL if it's on {}\x1b[K\r\n", target);
        } else {
            let _ = write!(f, "  kernel: ALIVE   abort: 'q' (keyboard alive)\x1b[K\r\n");
        }
        f.flush(&ctx);
        // Per-round line to the kernel LOG (serial + ring buffer, NOT the in-place framebuffer panel), so
        // the SERIAL LOG keeps a scrolling history of the storm for troubleshooting. The CSI panel above
        // overwrites itself in place each round; without this the serial would show only the latest frame.
        // This records the per-round ATTACK + victim - the trail the operator relies on.
        ctx.log_fmt(format_args!(
            "chaos round {}: swept {} svc, {} flooded, {} killed, +mem +spawn", round, ncand, sweep_flooded, sweep_killed));

        // Pace the round. With the recovery wait gone the loop would otherwise spin in milliseconds,
        // flooding the serial log and outrunning the eye. Yield a modest beat, still polling `q` (from the
        // SERIAL console - the keyboard is a chaos target) so an abort lands promptly.
        for _ in 0..PACE_YIELDS {
            if let Some(b) = ctx.try_console_read() { if b == b'q' || b == b'Q' { break 'carnage; } }
            ctx.yield_cpu();
        }
    }

    // The live per-service TABLE above IS the report - the user wanted the end to LOOK like the table, not
    // switch to a separate screen. So leave the final frame in place (no `\x1b[2J`) and just append one
    // summary line below it (which also carries the substrings the shell test greps for).
    ctx.console_writeln("=== chaos max-carnage: report ===");
    ctx.console_writeln_fmt(format_args!(
        "total: {} rounds, {} kills, {} flooded, {} mem-pressure, {} spawns. kernel: alive (this command returned).",
        round, killed, flooded, mempr, spawns));

    // Reclaim any flood caps still cached, so the run leaves the cap table as it found it.
    for c in sv_floodcap.iter_mut() { if let Some(h) = c.take() { ctx.remove_cap(h); } }
    // The sweep spawned one mem-pressure task per sweep (the spawn-storm dimension), so reclaim them ALL:
    // kill one at a time until none remain, not just one, else a long run leaks the parked tasks + their
    // held memory. Bounded so a kill racing a respawn cannot spin forever. This runs BEFORE the shell-wait
    // below, so freeing the task pool lets a killed shell respawn into a slot.
    let mut g = 0u32;
    while slot_of(&ctx, "mem-pressure").is_some() && g < 512 {
        let _ = ctx.kill("mem-pressure"); g += 1;
    }

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
    // Cosmetic hand-off pacing, NOT a completion wait (Commandment VIII): the live-shell TRUTH is
    // already established by the bounded slot_of loop above. This fixed pad only smooths the console
    // hand-back so our "done" line and the shell's redrawn prompt do not interleave; skipping it risks
    // at worst a momentary cosmetic glitch, never incorrectness. Deliberately time-based, not a truth-wait.
    for _ in 0..SHELL_SETTLE_YIELDS { ctx.yield_cpu(); }
    // Print our last line FIRST, then release. The muted shell only draws its prompt once it regains the
    // foreground, so releasing BEFORE this printed "done" made the shell's `gsh>` land before the text
    // (the "switches screen, press Enter to see the prompt" glitch). done -> release -> the shell draws a
    // clean prompt right below.
    ctx.console_writeln("chaos: done - foreground returned to the shell");
    ctx.release_console_foreground();

    // Self-terminate so chaos does not linger in the task list (`observe` showed a parked chaos long
    // after a run). The kernel's kill syscall now switches away on a self-kill (handle_kill ->
    // current_task_is_dead -> yield_current), so this never returns - no "no running task" panic (the
    // bug that forced the earlier park-and-reap). The park is an unreachable safety net for `-> !`.
    let _ = ctx.kill("chaos");
    ctx.park();
}
