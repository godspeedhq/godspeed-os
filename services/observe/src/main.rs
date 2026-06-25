// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! `observe` - live task introspection (Appendix C §C.1).
//!
//! Polls all 224 scheduler slots every ~500 yields and logs a summary of
//! every live task: name, core, state, memory, restart count, queue depth,
//! and CPU usage percentage.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

const MAX_SLOTS:      u32 = 224;
const YIELD_INTERVAL: u32 = 500;
const FRAME_SIZE:     u64 = 4096;
const QUEUE_MAX:      u8  = 16;
const MAX_CORES:      u32 = 16;

// Mode passed by the kernel at spawn (ServiceConfig.probe_mode).
const MODE_LIVE:    u32 = 0; // `observe`      - refresh forever (full-build streaming)
const MODE_NOW:     u32 = 1; // `observe now`  - one static frame, then park
const MODE_LIVE_FG: u32 = 2; // `observe` live - full-screen foreground view

/// Repaint cadence for the live view, in TSC cycles (~0.5 s at 2 GHz on the
/// T630). `q` is polled every loop iteration regardless, so quit stays snappy.
const FRAME_CYCLES: u64 = 1_000_000_000;

/// Per-iteration sleep for the live loop, in TSC cycles (~30 ms at 2 GHz). The loop SLEEPS this
/// long between `q`-polls/repaints instead of busy-`yield`ing, so the core halts in between and
/// `observe` itself does not peg its core (which would make every task on that core read as
/// ~100% busy - the very thing observe reports). `q` latency stays ≤ this; granularity is one
/// quantum (~10 ms).
const POLL_SLEEP_CYCLES: u64 = 60_000_000;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Per-core tick baselines for delta-based CPU%.
    // Stack allocation - not global mutable state (§3.9).
    let mut prev_core_active = [0u64; MAX_CORES as usize];
    let mut prev_core_total  = [0u64; MAX_CORES as usize];
    // Per-TASK run-tick baseline for per-task CPU% - each task's share of its core (not the core's
    // whole busy ratio), so a service sharing a core with a busy-poller (xhci/ehci) is not tarred 100%.
    let mut prev_task_ticks  = [0u64; MAX_SLOTS as usize];

    if ctx.probe_mode() == MODE_NOW {
        // `observe now`: print exactly one frame, then park. The first frame has
        // no previous baseline, so CPU% is the cumulative share since boot - the
        // correct meaning for a point-in-time snapshot. There is no graceful
        // self-exit in v1; the shell kills any parked instance before the next
        // `observe now`, so at most one lingers. PARK (not yield) so the parked
        // instance does not peg its core until it is killed.
        print_state(&ctx, &mut prev_core_active, &mut prev_core_total, &mut prev_task_ticks, false);
        ctx.park();
    }

    if ctx.probe_mode() == MODE_LIVE_FG {
        // `observe` (live): the shell-brokered foreground view. We own the screen:
        // hide the cursor, suppress keystroke echo, repaint in place every
        // FRAME_CYCLES, and poll `q` to quit. On exit we restore the console and
        // park; the shell detects the park, cleans up, and reprints its prompt.
        run_live(&ctx, &mut prev_core_active, &mut prev_core_total, &mut prev_task_ticks);
        ctx.park();
    }

    // MODE_LIVE (full `osdev run` builds): refresh every ~500 yields to the log
    // stream. Not the interactive foreground view (that is MODE_LIVE_FG above).
    let _ = MODE_LIVE;
    ctx.log("observe: ready");
    let mut tick: u32 = 0;
    loop {
        ctx.yield_cpu();
        tick += 1;
        if tick < YIELD_INTERVAL { continue; }
        tick = 0;
        print_state(&ctx, &mut prev_core_active, &mut prev_core_total, &mut prev_task_ticks, false);
    }
}

/// Full-screen live view (MODE_LIVE_FG). Owns the console until `q` is pressed.
fn run_live(
    ctx:              &ServiceContext,
    prev_core_active: &mut [u64; MAX_CORES as usize],
    prev_core_total:  &mut [u64; MAX_CORES as usize],
    prev_task_ticks:  &mut [u64; MAX_SLOTS as usize],
) {
    // Take the screen: stop echoing keystrokes (we paint the display ourselves),
    // hide the underline cursor, and clear once.
    ctx.console_echo(false);
    ctx.console_write("\x1b[?25l\x1b[2J");

    let mut last = ctx.read_tsc();
    // Home + paint the first frame immediately.
    ctx.console_write("\x1b[H");
    print_state(ctx, prev_core_active, prev_core_total, prev_task_ticks, true);

    // Paint forever; the SHELL owns `q` while we run (it polls the console and KILLS us when
    // pressed, then restores the screen). We do NOT read input ourselves - one reader avoids a
    // race over the keyboard - and we SLEEP between frames so we never peg our core (a busy
    // refresh loop would make every task on this core read as ~100% in our own display, the
    // bug this fixes). Never returns; the shell reaps us.
    loop {
        ctx.sleep(POLL_SLEEP_CYCLES);
        let now = ctx.read_tsc();
        if now.wrapping_sub(last) >= FRAME_CYCLES {
            last = now;
            ctx.console_write("\x1b[H");
            print_state(ctx, prev_core_active, prev_core_total, prev_task_ticks, true);
        }
    }
}

fn print_state(
    ctx:              &ServiceContext,
    prev_core_active: &mut [u64; MAX_CORES as usize],
    prev_core_total:  &mut [u64; MAX_CORES as usize],
    prev_task_ticks:  &mut [u64; MAX_SLOTS as usize],
    live:             bool,
) {
    let num_cores = ctx.inspect_core_count().min(MAX_CORES);

    // --- Collect per-core tick deltas ---
    // core_total_delta is kept separately so per-task CPU% uses the same
    // interval denominator that was captured before prev_core_total is updated.
    let mut core_pct         = [0u32; MAX_CORES as usize];
    let mut core_total_delta = [0u64; MAX_CORES as usize];
    for c in 0..num_cores as usize {
        let active = ctx.inspect_core_active_ticks(c as u32);
        let total  = ctx.inspect_core_total_ticks(c as u32);
        let da = active.saturating_sub(prev_core_active[c]);
        let dt = total.saturating_sub(prev_core_total[c]);
        core_total_delta[c] = dt;
        core_pct[c] = if dt > 0 { ((da * 100) / dt) as u32 } else { 0 };
        prev_core_active[c] = active;
        prev_core_total[c]  = total;
    }

    // Total CPU% = average across all ready cores.
    let total_pct: u32 = if num_cores > 0 {
        let sum: u32 = (0..num_cores as usize).map(|c| core_pct[c]).sum();
        sum / num_cores
    } else {
        0
    };

    // --- Count live tasks ---
    let mut live_count: u32 = 0;
    for slot in 0..MAX_SLOTS {
        let st = ctx.task_stat(slot);
        // Count live tasks; among observers only the ACTIVE (Running) one counts (parked/dead excluded).
        if st.valid && !(st.name_str().starts_with("observe") && st.state != 1) { live_count += 1; }
    }

    // --- RAM ---
    let free_frames  = ctx.inspect_kernel_free_frames();
    let total_frames = ctx.inspect_kernel_total_frames();
    let used_bytes   = (total_frames - free_frames) * FRAME_SIZE;
    let total_mib    = (total_frames * FRAME_SIZE) / (1024 * 1024);
    // Show total in MiB under 1 GiB, GiB otherwise (avoids "0 GiB" for small RAM).
    let (total_val, total_unit) = if total_mib >= 1024 {
        ((total_mib + 512) / 1024, "GiB")
    } else {
        (total_mib, "MiB")
    };
    let used_mib     = used_bytes / (1024 * 1024);
    let used_pct     = if total_mib > 0 { (used_mib * 100) / total_mib } else { 0 };
    let (used_val, used_unit) = bytes_fmt(used_bytes);

    // Per-line prefix. Only the streamed log mode (MODE_LIVE, `osdev run`) tags lines with
    // `observe: ` so they stay identifiable amid other services' output. The interactive views -
    // `observe now` (snapshot) and the full-screen live view - own the display and drop the tag,
    // which otherwise just reads busy. (font8x8 is ASCII-only, so bars use '-'/'=' not box-drawing.)
    let p = if ctx.probe_mode() == MODE_LIVE { "observe: " } else { "" };

    // --- Title bar (live only) - the quit hint up top where the eye starts ---
    if live {
        ctx.console_line(true, "observe - live                                      (q to quit)");
        ctx.console_line(true, "================================================================");
    }

    // --- Legend: a single "legend" header, then grouped entries (no repeated "legend:" prefix) ---
    ctx.console_line_fmt(live, format_args!("{}--------------------------------- legend ---------------------------------", p));
    ctx.console_line_fmt(live, format_args!("{}TASK scheduler slot | NAME service name | CORE cpu core | STATE task state", p));
    ctx.console_line_fmt(live, format_args!("{}MEM_USED/LIMIT/% memory in use (binary+stack+alloc) / limit / % of limit", p));
    ctx.console_line_fmt(live, format_args!("{}RESTARTS deaths recovered (not clean re-runs) | QUEUE/LIMIT inbound depth / max", p));
    ctx.console_line_fmt(live, format_args!("{}CPU% core share since last snapshot | UPTIME since the service last (re)started", p));

    // --- System summary ---
    ctx.console_line_fmt(live, format_args!("{}----------- system state ({} live) -----------", p, live_count));

    // Uptime since boot (resets on reboot) - wall-clock RTC, same source as the `uptime` command.
    let up = ctx.uptime_secs() as u64;
    ctx.console_line_fmt(live, format_args!(
        "{}UPTIME: {}d {:02}:{:02}:{:02}", p, up / 86400, (up / 3600) % 24, (up / 60) % 60, up % 60));

    // Build CPU summary line: "C0  98%  C1  99%  ...  total (49%)"
    let mut cpu_line = [0u8; 128];
    let mut pos = 0usize;
    for c in 0..num_cores as usize {
        if c > 0 { cpu_line[pos] = b' '; cpu_line[pos+1] = b' '; pos += 2; }
        cpu_line[pos] = b'C'; pos += 1;
        pos += fmt_u32(&mut cpu_line[pos..], c as u32);
        cpu_line[pos] = b' '; pos += 1;
        pos += fmt_pct(&mut cpu_line[pos..], core_pct[c]);
    }
    let suffix = b"  total (";
    cpu_line[pos..pos + suffix.len()].copy_from_slice(suffix);
    pos += suffix.len();
    pos += fmt_u32(&mut cpu_line[pos..], total_pct);
    cpu_line[pos] = b'%'; pos += 1;
    cpu_line[pos] = b')'; pos += 1;

    if let Ok(s) = core::str::from_utf8(&cpu_line[..pos]) {
        ctx.console_line_fmt(live, format_args!("{}CPU: {}", p, s));
    }

    ctx.console_line_fmt(live, format_args!(
        "{}RAM: {} {} used / {} {} total ({}%)",
        p, used_val, used_unit, total_val, total_unit, used_pct,
    ));

    // --- Task table ---
    ctx.console_line_fmt(live, format_args!(
        "{}TASK NAME         CORE STATE      MEM_USED/LIMIT/%     RESTARTS QUEUE/LIM CPU% UPTIME", p));
    for slot in 0..MAX_SLOTS {
        let stat = ctx.task_stat(slot);
        if !stat.valid {
            prev_task_ticks[slot as usize] = 0; // slot empty - reset its baseline
            continue;
        }

        let c = (stat.core as usize).min(MAX_CORES as usize - 1);
        // Per-task CPU% = this task's run-tick delta as a share of its core's total-tick delta over
        // the interval. A task blocked on recv accrues no run ticks -> 0%, even when its core is
        // pegged by a co-resident busy-poller (xhci/ehci). First frame: no baseline -> share since
        // boot (the right meaning for a one-shot snapshot).
        let task_delta = stat.run_ticks.saturating_sub(prev_task_ticks[slot as usize]);
        prev_task_ticks[slot as usize] = stat.run_ticks;
        let cdt = core_total_delta[c];
        let task_pct = if cdt > 0 { ((task_delta * 100) / cdt).min(100) as u32 } else { 0 };

        // Show only the ACTIVE observer (the one rendering this frame, so Running) - a parked
        // `observe now` leftover (BlockRecv) or a dead one not yet reaped is just clutter. The active
        // observer reads its own slot as Running. Baseline already updated above so the skip doesn't desync.
        if stat.name_str().starts_with("observe") && stat.state != 1 { continue; }

        let (uval, uunit) = bytes_fmt(stat.mem_used);
        let (lval, lunit) = bytes_fmt(stat.mem_limit);
        let mem_pct = if stat.mem_limit > 0 { (stat.mem_used * 100 / stat.mem_limit) as u32 } else { 0 };
        let full_mark = if stat.queue_depth >= QUEUE_MAX { "!" } else { " " };
        // Per-service uptime as the largest unit (d/h/m/s) - compact; resets on restart, so a freshly
        // recovered service reads a small value (pairs with RESTARTS).
        let (up_val, up_unit) =
            if      stat.uptime_secs >= 86400 { (stat.uptime_secs / 86400, 'd') }
            else if stat.uptime_secs >= 3600  { (stat.uptime_secs / 3600,  'h') }
            else if stat.uptime_secs >= 60    { (stat.uptime_secs / 60,    'm') }
            else                              { (stat.uptime_secs,         's') };

        ctx.console_line_fmt(live, format_args!(
            "{}{:<4} {:<12} C{:<2} {:<10} {:>3} {:3}/{:>2} {:3}/{:>3}%  {:<8} {:>2}/{}{}  {:>3}%  {:>4}{}",
            p,
            slot,
            stat.name_str(),
            stat.core,
            stat.state_str(),
            uval, uunit,
            lval, lunit, mem_pct,
            stat.restart_count,
            stat.queue_depth, QUEUE_MAX, full_mark,
            task_pct,
            up_val, up_unit,
        ));
    }

    // In the live view, clear any rows left over below the frame (e.g. if a task
    // count shrank between frames). The quit hint lives in the title bar up top.
    if live {
        ctx.console_write("\x1b[J"); // erase from cursor to end of screen
    }
}

/// Return (value, unit) for a byte count - KiB when < 1 MiB, MiB otherwise.
fn bytes_fmt(bytes: u64) -> (u64, &'static str) {
    if bytes < 1024 * 1024 {
        (bytes / 1024, "KiB")
    } else {
        (bytes / (1024 * 1024), "MiB")
    }
}

/// Write a u32 as decimal ASCII into `buf`. Returns bytes written.
fn fmt_u32(buf: &mut [u8], mut v: u32) -> usize {
    if v == 0 { buf[0] = b'0'; return 1; }
    let mut tmp = [0u8; 10];
    let mut len = 0usize;
    while v > 0 { tmp[len] = b'0' + (v % 10) as u8; v /= 10; len += 1; }
    for i in 0..len { buf[i] = tmp[len - 1 - i]; }
    len
}

/// Write a right-aligned 3-char percentage followed by '%'. Returns 4 bytes written.
fn fmt_pct(buf: &mut [u8], pct: u32) -> usize {
    let pct = pct.min(100);
    if pct < 10 {
        buf[0] = b' '; buf[1] = b' '; buf[2] = b'0' + pct as u8; buf[3] = b'%';
    } else if pct < 100 {
        buf[0] = b' ';
        buf[1] = b'0' + (pct / 10) as u8;
        buf[2] = b'0' + (pct % 10) as u8;
        buf[3] = b'%';
    } else {
        buf[0] = b'1'; buf[1] = b'0'; buf[2] = b'0'; buf[3] = b'%';
    }
    4
}
