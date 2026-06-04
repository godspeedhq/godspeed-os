//! `observe` — live task introspection (Appendix C §C.1).
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
const MODE_LIVE: u32 = 0; // `observe`     — refresh forever
const MODE_NOW:  u32 = 1; // `observe now` — one static frame, then park

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Per-core tick baselines for delta-based CPU%.
    // Stack allocation — not global mutable state (§3.9).
    let mut prev_core_active = [0u64; MAX_CORES as usize];
    let mut prev_core_total  = [0u64; MAX_CORES as usize];

    if ctx.probe_mode() == MODE_NOW {
        // `observe now`: print exactly one frame, then park. The first frame has
        // no previous baseline, so CPU% is the cumulative share since boot — the
        // correct meaning for a point-in-time snapshot. There is no graceful
        // self-exit in v1; the shell kills any parked instance before the next
        // `observe now`, so at most one lingers. PARK (not yield) so the parked
        // instance does not peg its core until it is killed.
        print_state(&ctx, &mut prev_core_active, &mut prev_core_total);
        ctx.park();
    }

    // `observe` (live): refresh every ~500 yields. Reachable from full (`osdev
    // run`) builds today; the shell's bare `observe` reports "coming soon" until
    // the live view's console-ownership handoff (clear+home, q-to-quit) is built.
    let _ = MODE_LIVE;
    ctx.log("observe: ready");
    let mut tick: u32 = 0;
    loop {
        ctx.yield_cpu();
        tick += 1;
        if tick < YIELD_INTERVAL { continue; }
        tick = 0;
        print_state(&ctx, &mut prev_core_active, &mut prev_core_total);
    }
}

fn print_state(
    ctx:              &ServiceContext,
    prev_core_active: &mut [u64; MAX_CORES as usize],
    prev_core_total:  &mut [u64; MAX_CORES as usize],
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
    let mut live: u32 = 0;
    for slot in 0..MAX_SLOTS {
        if ctx.task_stat(slot).valid { live += 1; }
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

    // --- Legend ---
    ctx.log("observe: legend: TASK: scheduler slot | NAME: service name");
    ctx.log("observe: legend: CORE: cpu core | STATE: task state");
    ctx.log("observe: legend: MEM_USED/LIMIT: heap memory allocated via alloc_mem syscall / contract memory limit");
    ctx.log("observe: legend: RESTARTS: restart count | QUEUE/LIMIT: inbound queue depth / max queue depth");
    ctx.log("observe: legend: CPU%: percentage of assigned core used since last snapshot");

    // --- System summary ---
    ctx.log_fmt(format_args!("observe: ----------- system state ({} live) -----------", live));

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
        ctx.log_fmt(format_args!("observe: CPU: {}", s));
    }

    ctx.log_fmt(format_args!(
        "observe: RAM: {} {} used / {} {} total ({}%)",
        used_val, used_unit, total_val, total_unit, used_pct,
    ));

    // --- Task table ---
    ctx.log("observe: TASK  NAME             CORE STATE        MEM_USED/LIMIT  RESTARTS  QUEUE/LIMIT  CPU%");
    for slot in 0..MAX_SLOTS {
        let stat = ctx.task_stat(slot);
        if !stat.valid { continue; }

        let (uval, uunit) = bytes_fmt(stat.mem_used);
        let (lval, lunit) = bytes_fmt(stat.mem_limit);
        let full = stat.queue_depth >= QUEUE_MAX;

        let c = (stat.core as usize).min(MAX_CORES as usize - 1);
        let task_pct = core_pct[c];

        ctx.log_fmt(format_args!(
            "observe: {:<5} {:<16} C{:<3} {:<12} {:>3} {:3}/{:>2} {:3}  {:<9} {:>2}/{}{}  {:>3}%",
            slot,
            stat.name_str(),
            stat.core,
            stat.state_str(),
            uval, uunit,
            lval, lunit,
            stat.generation,
            stat.queue_depth, QUEUE_MAX,
            if full { " (FULL)" } else { "       " },
            task_pct,
        ));
    }
}

/// Return (value, unit) for a byte count — KiB when < 1 MiB, MiB otherwise.
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
