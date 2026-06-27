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

/// A bounded frame buffer. The whole panel is built into one of these, then flushed in a couple of
/// `console_write`s (not one per line), so the framebuffer redraws without flicker. `console_write` now
/// caps at 4096 bytes (one page), so the whole <=2048-byte panel flushes in ONE write and the framebuffer
/// repaints in a single pass instead of ~4 bursts - that single pass is what removes the flicker. The
/// newline-break below still applies if a frame ever exceeds the cap, so a CSI escape is never split.
struct FrameBuf { buf: [u8; 2048], len: usize }
impl FrameBuf {
    fn new() -> Self { Self { buf: [0; 2048], len: 0 } }
    fn flush(&self, ctx: &ServiceContext) {
        let mut s = 0;
        while s < self.len {
            let mut e = (s + self.buf.len()).min(self.len);
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
    // The shell launcher sends the round count right after spawning us (0 = run until q). Wait briefly
    // (RTC-bounded) for it BEFORE claiming the foreground - the shell is still live to send it. Default
    // to run-until-q if it never arrives (e.g. the launcher's send failed), so we never block forever.
    let mut rounds: u64 = 0;
    let mut tbuf = [0u8; 24];
    let mut tlen = 0usize;
    {
        let t0 = ctx.datetime().epoch_secs();
        loop {
            if let Some(msg) = ctx.try_recv() {
                let b = msg.payload_bytes();
                if b.len() >= 4 { rounds = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64; }
                if b.len() > 4 { let n = (b.len() - 4).min(24); tbuf[..n].copy_from_slice(&b[4..4 + n]); tlen = n; }
                break;
            }
            if ctx.datetime().epoch_secs() - t0 >= 2 { break; }
            ctx.yield_cpu();
        }
    }
    // The TARGET: "all-services" = aim kill/flood at random live services; a service name = aim them at
    // THAT service every round. mem-pressure + spawn-storm are system-wide in both modes. Default
    // "all-services" (named so it can never clash with a real service that someone calls "all").
    let target: &str = if tlen == 0 { "all-services" } else { str_of(&tbuf[..tlen]) };
    let target_all = target == "all-services";

    // Take the keyboard so a resurrected shell cannot steal our `q`. This is the moment the shell goes
    // "muted" for the duration of the run (unclaimed is the normal state, so this changes nothing else).
    ctx.claim_console_foreground();
    // chaos owns the screen now: the foreground gate sends every backgrounded task's console output to
    // serial only, so the muted shell can no longer smear our display. Clear it for a fresh canvas.
    ctx.console_write("\x1b[2J\x1b[H");

    // RNG seed: the TSC mixed with the wall clock; never zero.
    let mut rng = ctx.read_tsc()
        ^ (ctx.datetime().epoch_secs() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    if rng == 0 { rng = 0xDEAD_BEEF_CAFE_F00D; }

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

    'carnage: loop {
        // `q` aborts. The kernel buffers the keypress across input-driver churn, so it is caught here.
        if let Some(b) = ctx.try_console_read() { if b == b'q' || b == b'Q' { break; } }
        if rounds > 0 && round >= rounds { break; } // a bounded `max-carnage N` run is complete

        round += 1;

        // Roll ONE attack. KILL/FLOOD are AIMED at a victim (per-service tally); MEM-PRESSURE + SPAWN-STORM
        // are SYSTEM-WIDE (they pressure the shared frame allocator + task pool, which cannot be aimed at one
        // service), so they are global counters only. Pick the aimed victim: for "all-services" a random live
        // service (the ONLY truth chaos fetches), excluding chaos itself, the mem-pressure it spawns, and
        // transient observe-* tools; for a specific target, THAT service every round.
        rng = xorshift64(rng);
        let action = rng % 4;
        let mut nbuf = [0u8; 24];
        let mut nl = 0usize;
        if action == 0 || action == 1 {
            if target_all {
                let mut cand: [([u8; 24], usize); MAX_CAND] = [([0u8; 24], 0usize); MAX_CAND];
                let mut ncand = 0usize;
                for slot in 0..256u32 {
                    let st = ctx.task_stat(slot);
                    if !st.valid || st.state == 4 { continue; }
                    let nm = st.name_str();
                    if nm.is_empty() || nm == "chaos" || nm == "mem-pressure" || nm.starts_with("observe") { continue; }
                    if ncand < MAX_CAND {
                        let b = nm.as_bytes(); let l = b.len().min(24);
                        cand[ncand].0[..l].copy_from_slice(&b[..l]); cand[ncand].1 = l; ncand += 1;
                    }
                }
                if ncand > 0 {
                    rng = xorshift64(rng);
                    let pick = (rng % ncand as u64) as usize;
                    nl = cand[pick].1; nbuf[..nl].copy_from_slice(&cand[pick].0[..nl]);
                }
            } else {
                let tb = target.as_bytes(); nl = tb.len().min(24); nbuf[..nl].copy_from_slice(&tb[..nl]);
            }
        }
        let name = str_of(&nbuf[..nl]);

        // Find-or-add the per-service tally slot for an aimed victim (kill + flood share its slot + cap).
        let mut idx = None;
        if nl > 0 {
            for s in 0..nsv { if sv_name[s][..sv_nlen[s]] == nbuf[..nl] { idx = Some(s); break; } }
            if idx.is_none() && nsv < MAX_SVC {
                sv_name[nsv][..nl].copy_from_slice(&nbuf[..nl]); sv_nlen[nsv] = nl;
                idx = Some(nsv); nsv += 1;
            }
        }

        let mut attack_name = match action { 0 => "kill-storm", 1 => "flood-storm", 2 => "mem-pressure", _ => "spawn-storm" };
        let mut victim_label: &str = if nl > 0 { name } else { "(none alive)" };
        match action {
            // KILL-STORM (aimed). Reclaim the cached flood cap (the kill bumped the endpoint generation, so
            // it is now stale): RECLAIM, don't drop, else a long run fills the 64-slot cap table.
            0 => {
                if let Some(s) = idx {
                    let _ = ctx.kill(name);
                    killed += 1; sv_killed[s] += 1;
                    if let Some(h) = sv_floodcap[s].take() { ctx.remove_cap(h); }
                }
            }
            // FLOOD-STORM (aimed), DRAIN-style only. shell + fs are reply-style: flooding corrupts their
            // reply stream (the shell's 16/16 clog; fs reading junk-as-superblock), so KILL them instead
            // (still a hit, counted as kill-storm).
            1 => {
                if let Some(s) = idx {
                    if name == "shell" || name == "fs" {
                        let _ = ctx.kill(name);
                        killed += 1; sv_killed[s] += 1;
                        if let Some(h) = sv_floodcap[s].take() { ctx.remove_cap(h); }
                        attack_name = "kill-storm";
                        sv_flood_na[s] = 1; // reply-style: flood is N/A, we killed it instead
                    } else {
                        match flood(&ctx, name, &mut sv_floodcap[s]) {
                            Some(_) => { flooded += 1; sv_flooded[s] += 1; } // floodable: na stays 0
                            None    => { sv_flood_na[s] = 2; } // acquire_send_cap failed: no floodable endpoint
                        }
                    }
                }
            }
            // MEM-PRESSURE (system-wide): chaos allocates its OWN memory. Held (no free syscall); chaos's
            // contract limit bounds it (later allocs return AllocDenied - still count the attempt) and the
            // footprint is reclaimed when chaos self-kills. A global counter only.
            2 => { let _ = ctx.alloc_mem(MEMP_CHUNK); mempr += 1; victim_label = "system"; }
            // SPAWN-STORM (system-wide): spawn a mem-pressure (a no-op if one already runs; reclaimed at end).
            _ => { let _ = ctx.spawn("mem-pressure"); spawns += 1; victim_label = "system"; }
        }

        // Redraw the per-service TABLE in place. We build the whole frame into one buffer and flush it in
        // a couple of writes (not ~one per line), so the framebuffer redraws without flicker. Home the
        // cursor (NOT `\x1b[J`, which blanks the screen and flickers); each line erases to its end
        // (`\x1b[K`) so a shorter value overwrites cleanly. The table only GROWS (a service appends the
        // first time chaos hits it), so the in-place redraw never leaves a stale row below it. The table
        // shows only what chaos FIRED - the kills + floods it itself counted - plus the global spawn count.
        let mut f = FrameBuf::new();
        let _ = write!(f, "\x1b[H");
        let _ = write!(f, "  C H A O S   max-carnage    target: {}    ('q' to quit via SERIAL)\x1b[K\r\n", target);
        if rounds > 0 {
            let pct = round * 100 / rounds; // round <= rounds, so 1..=100; rounds>0 guards the divide
            let _ = write!(f, "  round {} / {} ({}%)\x1b[K\r\n", round, rounds, pct);
        } else {
            let _ = write!(f, "  round {}  (until q)\x1b[K\r\n", round);
        }
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
        let _ = write!(f, "  kernel: ALIVE   abort: 'q' in the SERIAL console (keyboard dead)\x1b[K\r\n");
        f.flush(&ctx);
        // Per-round line to the kernel LOG (serial + ring buffer, NOT the in-place framebuffer panel), so
        // the SERIAL LOG keeps a scrolling history of the storm for troubleshooting. The CSI panel above
        // overwrites itself in place each round; without this the serial would show only the latest frame.
        // This records the per-round ATTACK + victim - the trail the operator relies on.
        ctx.log_fmt(format_args!("chaos round {}: {} -> {}", round, attack_name, victim_label));

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
    // Reclaim the last spawn-storm mem-pressure (one runs at a time), so the run leaves memory as it found it.
    let _ = ctx.kill("mem-pressure");

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
