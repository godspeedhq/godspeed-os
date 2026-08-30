// SPDX-License-Identifier: GPL-2.0-only
//! `supervisor` - restart authority. TCB member (§6.1). Non-restartable.
//!
//! Phase 5:
//!   1. Spawns `pong` on core 1 and `ping` on core 0 (§23.2 acceptance criteria).
//!   2. Logs "supervisor: ready".
//!   3. Yields indefinitely (death-notification restart loop deferred to Phase 6).
//!
//! The kernel wires send-peer SEND caps at spawn time, so supervisor does not
//! need to coordinate cap distribution manually.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, CapHandle};

// ONE table, shared by source with the other principal that spawns probes: a probe respawns its own
// victim, and a second copy of these parameters would be a second truth (Commandment III).
#[path = "../../probe/src/table.rs"]
mod probes;

// ───────────────────────────────────────────────────────────────────────────────
// Phase 1 of moving naming out of the kernel (docs/naming-design.md).
//
// As the supervisor spawns the real services it records, in a bounded no-heap map, the
// SEND|GRANT endpoint cap the kernel hands back from `spawn_returning_endpoint` (syscall 38,
// Phase 0a). This proves the supervisor can hold a cap to everything it starts - the future
// name authority. It is a SHADOW map for now: nothing reads it to wire dependents yet (that is
// Phase 0b/3). Scoped to the real services; the 193 test probes are test infra (out of scope)
// and are spawned with their parameters from `probes.rs` - the table that used to be 193 rows of
// the kernel service catalogue (docs/probe-params-design.md).
// ───────────────────────────────────────────────────────────────────────────────
const NAME_MAP_MAX:      usize = 16;  // bounded (§26.6) - real services, not the test probes
const NAME_MAP_NAME_MAX: usize = 16;

struct NameCapMap {
    names: [[u8; NAME_MAP_NAME_MAX]; NAME_MAP_MAX],
    lens:  [u8; NAME_MAP_MAX],
    caps:  [u32; NAME_MAP_MAX],       // endpoint cap slot; u32::MAX = empty
    count: usize,
}
impl NameCapMap {
    const fn new() -> Self {
        NameCapMap {
            names: [[0u8; NAME_MAP_NAME_MAX]; NAME_MAP_MAX],
            lens:  [0u8; NAME_MAP_MAX],
            caps:  [u32::MAX; NAME_MAP_MAX],
            count: 0,
        }
    }
    /// Record `name → cap_slot`, **updating in place** if `name` is already mapped (so a restart
    /// refreshes the cap - and a kill-storm can't grow the map past its bound, §26.6). Returns
    /// false (loud, never a silent drop) only if the name is new AND the map is full / too long.
    fn record(&mut self, name: &str, cap_slot: u32) -> bool {
        let nb = name.as_bytes();
        if nb.len() > NAME_MAP_NAME_MAX { return false; }
        // Update an existing entry (restart refresh).
        for i in 0..self.count {
            if self.lens[i] as usize == nb.len() && &self.names[i][..nb.len()] == nb {
                self.caps[i] = cap_slot;
                return true;
            }
        }
        // Append a new entry.
        if self.count >= NAME_MAP_MAX { return false; }
        let i = self.count;
        self.names[i][..nb.len()].copy_from_slice(nb);
        self.lens[i]  = nb.len() as u8;
        self.caps[i]  = cap_slot;
        self.count   += 1;
        true
    }
    /// The recorded endpoint cap slot for `name`, if mapped.
    fn get(&self, name: &str) -> Option<u32> {
        let nb = name.as_bytes();
        (0..self.count).find(|&i| self.lens[i] as usize == nb.len() && &self.names[i][..nb.len()] == nb)
            .map(|i| self.caps[i])
    }
}

/// Phase 2/3 (docs/naming-design.md): spawn `name`, **providing the listed `peers` from the
/// supervisor's name→cap map** (the caps recorded when those services were spawned) instead of the
/// kernel name table. Any declared peer NOT listed here is still name-wired by the kernel (the
/// merge) - peers flip one at a time. Records the new service's own endpoint cap. If none of the
/// requested peers are mapped yet, falls back to a fully name-wired spawn (loud). The flipped
/// wiring is proven functionally (e.g. fs←block-driver by real disk I/O; shell←fs by file commands).
/// Returns true if the service spawned (used by the restart loop).
fn spawn_wired(ctx: &ServiceContext, map: &mut NameCapMap, name: &str, peers: &[&str]) -> bool {
    let mut installs: [(&str, CapHandle); 4] = [("", CapHandle(0)); 4];
    let mut n = 0usize;
    for &p in peers {
        if n >= installs.len() { break; }
        match map.get(p) {
            Some(slot) => { installs[n] = (p, CapHandle(slot)); n += 1; }
            None => ctx.log_fmt(format_args!(
                "supervisor: {} peer '{}' not in name-cap map - kernel will name-wire it", name, p)),
        }
    }
    if n == 0 {
        return spawn_mapped(ctx, map, name, 0xFFFF); // nothing to provide - plain name-wired spawn
    }
    match ctx.spawn_with_caps(name, 0xFFFF, &installs[..n]) {
        Ok(Some(cap)) => {
            // Free the dead instance's cap on a restart (see spawn_mapped) - no cap-table leak.
            if let Some(old) = map.get(name) { ctx.remove_cap(CapHandle(old)); }
            let _ = map.record(name, cap.0);
            ctx.log_fmt(format_args!(
                "supervisor: {} wired from the name-cap map ({} peer(s) provided; rest name-wired)", name, n));
            true
        }
        Ok(None) => { ctx.log_fmt(format_args!("supervisor: {} wired (no endpoint to record)", name)); true }
        Err(_)   => {
            // A provided peer cap was stale/dead: the peer respawned under heavy restart churn (e.g.
            // `chaos max-carnage`), leaving the map cap pointing at a dead instance, so spawn_with_caps
            // rejected the whole spawn. Retry FULLY NAME-WIRED - the kernel resolves live peers by name
            // from its directory and the new service reacquires any down peer on EndpointDead (§14.3).
            // This is what makes fs/shell recover after a storm instead of staying dead on a stale cap.
            ctx.log_fmt(format_args!(
                "supervisor: {} cached peer cap stale (peer restarted) - name-wiring instead", name));
            spawn_mapped(ctx, map, name, 0xFFFF)
        }
    }
}

/// Spawn `name` on `core` (0xFFFF = round-robin) AND record its endpoint cap in `map` (Phase 1).
/// The spawn itself is identical to `ctx.spawn` - the new syscall just also hands back a cap.
/// Returns true if the service spawned with an endpoint cap (used by the restart loop).
fn spawn_mapped(ctx: &ServiceContext, map: &mut NameCapMap, name: &str, core: u32) -> bool {
    match ctx.spawn_returning_endpoint(name, core) {
        Some(cap) => {
            // On a restart, free the dead instance's cap before recording the new one, so a
            // kill-storm can't leak the supervisor's cap table (the map already updates in place).
            if let Some(old) = map.get(name) { ctx.remove_cap(CapHandle(old)); }
            if map.record(name, cap.0) {
                ctx.log_fmt(format_args!("supervisor: name-map + {} (endpoint cap slot {})", name, cap.0));
            } else {
                ctx.log_fmt(format_args!("supervisor: name-map FULL - dropped {}", name));
            }
            true
        }
        None => { ctx.log_fmt(format_args!("supervisor: spawn {} returned no endpoint cap", name)); false }
    }
}

/// Ensure `name` is running and recorded in the map (Path C / Phase 6 - unifies boot and recovery).
///
/// On a **fresh boot** nothing is running yet, so this spawns (via `spawn_mapped`/`spawn_wired`). On a
/// **supervisor respawn** the real services are still alive (only the supervisor died), so this
/// ADOPTS each - reacquires its endpoint cap by name from the kernel directory and records it -
/// instead of re-spawning a duplicate (which the kernel would reject as AlreadyRunning anyway). The
/// kernel re-points death notifications to the respawned supervisor via the directory, so after this
/// reconciliation the restart loop works exactly as on a fresh boot.
///
/// Known v1 limitation: the kernel directory keeps a name even after the service dies, so a service
/// that died *during* the supervisor's brief (~1 tick) downtime would be adopted as a stale cap
/// rather than respawned. Narrow race; full liveness-aware reconciliation is a follow-up.
fn ensure_mapped(ctx: &ServiceContext, map: &mut NameCapMap, name: &str, core: u32) -> bool {
    if let Some(cap) = ctx.acquire_send_grant_cap(name) {
        let _ = map.record(name, cap.0);
        ctx.log_fmt(format_args!("supervisor: adopted running {} (slot {})", name, cap.0));
        return true;
    }
    spawn_mapped(ctx, map, name, core)
}

/// `ensure_mapped` for a service with peers - adopt if already running, else `spawn_wired`.
fn ensure_wired(ctx: &ServiceContext, map: &mut NameCapMap, name: &str, peers: &[&str]) -> bool {
    if let Some(cap) = ctx.acquire_send_grant_cap(name) {
        let _ = map.record(name, cap.0);
        ctx.log_fmt(format_args!("supervisor: adopted running {} (slot {})", name, cap.0));
        return true;
    }
    spawn_wired(ctx, map, name, peers)
}

/// The restartable services the supervisor is responsible for (§6.1). Hoisted so the scan, `reconcile`,
/// and `converge` share ONE roster. Order matters: block-driver before fs before shell (each wires to
/// the previous); nic-driver before net-stack.
const MANAGED_N: usize = 12;
const MANAGED: [&str; MANAGED_N] =
    ["block-driver", "fs", "shell", "xhci", "ehci", "logger", "console", "nic-driver", "net-stack",
     // C1-6: both moved OUT of the kernel and so must be started BY someone. `time` owns the wall
     // clock the shell and net-stack now ask for; `control` owns the COM2 operator channel the test
     // harness drives. A service that is embedded and configured but never spawned is the C5-1 shape
     // one step earlier - not "unwatched", but never started at all.
     // dwc2 is arm32-only and absent elsewhere; reconcile skips any name absent from the map, so
     // listing it costs other ports nothing. It was in the kernel's death-notification set but in no
     // reconcile list at all - so a DROPPED notification left the Pi's storage, keyboard and network
     // down with no backstop to notice.
     "time", "control", "dwc2"];

/// Scan REAL liveness via `task_stat` (NOT a cap-acquire, which the kernel directory keeps succeeding
/// for a dead name - the `ensure_*` stale-cap-adopt race, line ~149): which MANAGED services have a live
/// task (valid AND not Dead) right now. Index-aligned to `MANAGED`.
fn managed_alive(ctx: &ServiceContext) -> [bool; MANAGED_N] {
    let mut alive = [false; MANAGED_N];
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if !st.valid || st.state == 4 { continue; } // 4 = Dead
        let nm = st.name_str();
        for i in 0..MANAGED_N { if nm == MANAGED[i] { alive[i] = true; } }
    }
    alive
}

/// Single-service liveness for the death-loop's race guard: is a LIVE task with exactly this name up
/// right now? Same `task_stat` discipline as `managed_alive` (valid AND not Dead=4), for one arbitrary
/// name - covers non-MANAGED names like `counter` too - so the death-loop and the convergence agree on
/// what "alive" means. Early-exits on the first match.
fn name_alive(ctx: &ServiceContext, name: &str) -> bool {
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if !st.valid || st.state == 4 { continue; } // 4 = Dead
        if st.name_str() == name { return true; }
    }
    false
}

/// Respawn one managed service WIRED to its peers (block-driver before fs before shell; nic before net).
fn respawn_managed(ctx: &ServiceContext, map: &mut NameCapMap, name: &str) -> bool {
    match name {
        "fs"        => spawn_wired(ctx, map, "fs", &["block-driver"]),
        "shell"     => spawn_wired(ctx, map, "shell", &["fs"]),
        "net-stack" => spawn_wired(ctx, map, "net-stack", &["nic-driver"]),
        "counter"   => spawn_wired(ctx, map, "counter", &["fs"]),
        other       => spawn_mapped(ctx, map, other, 0xFFFF),
    }
}

/// Respawn `name` with a small BOUNDED retry (audit M5). A steady-state respawn can hit a TRANSIENT
/// failure - NoMemory / CapTableFull the instant another service is mid-reclaim (frames/kstack/caps not
/// yet returned). A single attempt then logs "restart FAILED" and strands the service until the NEXT
/// death notification, which for an ISOLATED death may never arrive. Retry a few times with a yield
/// between - letting the transient shortage clear - before giving up loudly. Bounded (invariant 12): it
/// can never hang; a persistently-failing service still returns false and is reported by the caller. In
/// the common case the first attempt succeeds, so this costs exactly one try. (A STORM is separately
/// covered by the `reconcile` backstop riding the next notification; this closes the isolated case.)
fn respawn_retry(ctx: &ServiceContext, map: &mut NameCapMap, name: &str) -> bool {
    const TRIES: u32 = 5;
    for _ in 0..TRIES {
        if respawn_managed(ctx, map, name) { return true; }
        ctx.yield_cpu();   // let a transient resource shortage (mid-reclaim) clear before retrying
    }
    false
}

/// Reconcile to desired state: respawn any managed restartable service that is NOT actually alive. A
/// death notification can be DROPPED under a storm - our endpoint is 16-deep, so a burst overflows it
/// and a dropped name is silently never restarted (the "fs gone from observe after a storm" bug).
/// `acquire_*_cap` cannot detect this (the kernel directory keeps a dead name), so we scan REAL liveness
/// via `task_stat`. Returns how many it respawned. (One pass; the death-loop backstop.)
fn reconcile(ctx: &ServiceContext, map: &mut NameCapMap) -> u32 {
    let alive = managed_alive(ctx);
    let mut n = 0;
    for i in 0..MANAGED_N {
        // Skip a service this build never spawned (absent from the map) - e.g. a driver whose hardware
        // the PCI scan did not find (ehci/nic-driver/net-stack, below). Without this, reconcile would
        // "resurrect" a deliberately-skipped driver on the first death notification, undoing the skip.
        // Mirrors converge's `map.get(...).is_none()` guard.
        if alive[i] || map.get(MANAGED[i]).is_none() { continue; }
        // respawn_RETRY, not a single respawn_managed: the reconcile backstop recovers a service whose
        // death NOTIFICATION was dropped (endpoint overflow under a storm), so a transient respawn
        // failure here has no later death to ride - it must retry to satisfaction like the death arms
        // do (§IX, audit U3; the M5 fix reached the arms but not this backstop).
        if respawn_retry(ctx, map, MANAGED[i]) {
            n += 1;
            ctx.log_fmt(format_args!("supervisor: reconcile respawned {} (missed death notification)", MANAGED[i]));
        }
    }
    n
}

/// Reconverge to consistency after the supervisor's OWN death (Path C / Phase 6). `all-services` (and
/// any nuke that includes the supervisor) is the ONE case the notification path cannot cover: a LIVE
/// supervisor never drops a death notification, but while the supervisor is itself dead+respawning a
/// death arriving then finds a dead endpoint and is lost - the shell, killed last, was orphaned exactly
/// this way. So on coming back, before it drops into the blocking recv loop and starts trusting
/// notifications again, the supervisor reconverges its managed set to TRUTH: every service it manages is
/// actually non-Dead. Wait for the roster to be satisfied, NOT for a timer - the re-check catches a
/// service that dies mid-convergence (a cascade). A NO-OP on a fresh boot (`ensure_*` already brought
/// everything up, so the first scan is all-alive) and scoped to the services THIS build manages
/// (recorded in `map`), so it never spawns one the build never had. Bounded (invariant 12): a service
/// that will not come up after `MAX_TRIES` is given up LOUDLY, so this can never hang on an impossible
/// truth. Once consistent it returns to the recv loop and the live-supervisor notification path carries
/// every future death.
fn converge(ctx: &ServiceContext, map: &mut NameCapMap) {
    const MAX_TRIES: u32 = 7;
    let mut attempts = [0u32; MANAGED_N];
    let mut given_up = [false; MANAGED_N];
    loop {
        let alive = managed_alive(ctx);
        let mut all_settled = true;
        for i in 0..MANAGED_N {
            // Only reconverge a service this build actually manages (`ensure_*` recorded it in the map).
            if given_up[i] || alive[i] || map.get(MANAGED[i]).is_none() { continue; }
            all_settled = false;
            attempts[i] += 1;
            if attempts[i] > MAX_TRIES {
                ctx.log_fmt(format_args!(
                    "supervisor: could not bring up {} after {} tries - run 'spawn {}'",
                    MANAGED[i], MAX_TRIES, MANAGED[i]));
                given_up[i] = true;
                continue;
            }
            respawn_managed(ctx, map, MANAGED[i]);
        }
        if all_settled { break; }
        ctx.yield_cpu(); // let respawns/reclaims settle before the next truth check
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Naming migration (docs/naming-design.md): `name → cap` map, built as we spawn the real
    // services. The supervisor wires dependents from it; clients resolve/reacquire names via the
    // kernel name-directory (Path C, §3.7).
    #[allow(unused_mut)]
    let mut name_map = NameCapMap::new();

    // Path C / Phase 5: the kernel boots the supervisor directly (init is removed), so the
    // supervisor now spawns the logger - moved here from init. logger is not TCB (§11.3): retry
    // once on failure and continue without it (its output falls back to the kernel ring buffer).
    ctx.log("supervisor: spawning logger...");
    if let Some(cap) = ctx.acquire_send_grant_cap("logger") {
        // Supervisor RESPAWN: the logger is still alive (only the supervisor died). Adopt it - reacquire
        // its endpoint by name - instead of trying to spawn a duplicate the kernel's singleton guard
        // rejects, which used to print a misleading "logger spawn failed" on every `kill supervisor`.
        // Mirrors the block-driver/fs/shell adopt lines in the reconcile path.
        let _ = name_map.record("logger", cap.0);
        ctx.log("supervisor: adopted running logger");
    } else if ctx.spawn("logger").is_err() {
        ctx.log("supervisor: logger spawn failed, retrying");
        let _ = ctx.spawn("logger");
    }

    // The terminal (docs/console-service.md §9). Spawned right after the logger and before anything
    // that produces console output, so the display changes hands once, early, rather than mid-boot.
    // Like the logger it is not TCB: if it fails to spawn, console output still reaches serial (which
    // is the source of truth) and the kernel's boot floor keeps the display - degraded, never silent.
    ctx.log("supervisor: spawning console...");
    if let Some(cap) = ctx.acquire_send_grant_cap("console") {
        let _ = name_map.record("console", cap.0);
        ctx.log("supervisor: adopted running console");
    } else if ctx.spawn("console").is_err() {
        ctx.log("supervisor: console spawn failed - display stays on the kernel boot floor");
    }

    // Spawn pong and ping first so IPC between them is established well before
    // probe services compete for scheduler quanta.  Pong must precede ping:
    // ping's SEND cap to pong is wired by the kernel at spawn time.
    // Skipped in idle-only builds (S8): no active workload by design.
    // Skipped in bp2-only: that mode isolates the BP2 cross-core round-trip
    // (perf-bp2 on core 0 ⇄ perf-bp2-echo on core 1) so echo is not starved by
    // the ping→pong flood on core 1 - gives clean, fast BP2 latency numbers.
    // Skipped in perf-iso: per-probe isolation builds run one benchmark alone.
    // Skipped in bare-metal: the USB-boot image settles at a quiet `gsh>` prompt.
    // ping/pong are demo services (examples/) - spawn them on demand from the
    // shell (`spawn pong` then `spawn ping`) when you want the cross-core demo.
    #[cfg(not(any(feature = "bare-metal", feature = "idle-only", feature = "bp2-only", feature = "perf-iso")))]
    {
        ctx.log("supervisor: spawning pong...");
        if ctx.spawn_on("pong", 1).is_err() {
            ctx.log("supervisor: WARN: failed to spawn pong on core 1, trying core 0");
            let _ = ctx.spawn_on("pong", 0);
        }
        ctx.log("supervisor: spawning ping...");
        if ctx.spawn_on("ping", 0).is_err() {
            ctx.log("supervisor: WARN: failed to spawn ping");
        }
        ctx.log("supervisor: pong+ping done");
    }

    // Identity probe services are harness-driven (QEMU control port sends kill
    // commands in response to sentinel serial strings).  Skip them in bare-metal,
    // perf-only, and perf-brutal-only builds: probe-hog tight-loops on core 0,
    // probe-4b-send blocks waiting for a harness kill that never arrives on HW,
    // and the combined 16-task load starves IPC benchmarks of scheduler quanta.
    #[cfg(not(any(feature = "bare-metal", feature = "perf-only", feature = "perf-brutal-only", feature = "stress-only", feature = "adv-only", feature = "chaos-only", feature = "fuzz-only", feature = "b2-only", feature = "bp2-only", feature = "perf-iso")))]
    {
        // --- Probe services (§22 Group A identity tests) ---
        // Recv-endpoint probes must come first so their endpoints are registered
        // before sender probes are spawned (caps are wired at spawn time).
        let _ = probes::probe(&ctx, "probe-recv");    // Test 3A receiver
        let _ = probes::probe(&ctx, "probe-victim");  // Test 4A kill target
        let _ = probes::probe(&ctx, "probe-4b-recv"); // Test 4B kill target
        let _ = probes::probe(&ctx, "probe-3b");      // Test 3B (has recv slot for wrong-right probe)
        // Sender / active probes - need SEND caps to the services above.
        let _ = probes::probe(&ctx, "probe-sender");  // Test 3A sender; SEND cap to probe-recv
        let _ = probes::probe(&ctx, "probe-4a");      // Test 4A; SEND cap to probe-victim
        let _ = probes::probe(&ctx, "probe-4b-send"); // Test 4B; SEND cap to probe-4b-recv
        // Cap-transfer probes (Tests 5A and 5B) - receiver first so its endpoint
        // is registered before the senders' SEND|GRANT caps are wired.
        let _ = probes::probe(&ctx, "probe-5a-recv"); // Test 5A/5B receiver
        let _ = probes::probe(&ctx, "probe-5a-send"); // Test 5A sender (SEND|GRANT cap)
        let _ = probes::probe(&ctx, "probe-5b-send"); // Test 5B negative (SEND-only cap)
        // Probes with no send peers.
        let _ = probes::probe(&ctx, "probe-yielder"); // Test 8A
        let _ = probes::probe(&ctx, "probe-hog");     // Test 8B (tight loop; preemption via ping)
        let _ = probes::probe(&ctx, "probe-9b");      // Test 9B
        // Memory-limit probes - Tests 7A and 7B.
        let _ = probes::probe(&ctx, "probe-7a");
        let _ = probes::probe(&ctx, "probe-7b");
        // Interrupt-routing probe - Test IR1A (§12.2, §12.3).
        let _ = probes::probe(&ctx, "probe-11a");
    }

    // Property, fuzz, stress, perf, adversarial, chaos probes.
    // Excluded in identity-only builds so supervisor: ready appears in < 10 s on
    // TCG, giving WithRestart tests plenty of deadline margin (§22 flakiness fix).
    // Also excluded in bare-metal builds (no harness present).
    #[cfg(not(any(feature = "bare-metal", feature = "idle-only")))]
    spawn_extended_probes(&ctx);

    // observe: spawn in full (osdev run) builds only. Excluded from test-specific
    // builds (its 224-slot scan every 500 yields adds timing noise) and from
    // bare-metal - its periodic table dump would keep the display scrolling, but
    // the USB image rests at `gsh>`. Run `observe` from the shell on demand.
    #[cfg(not(any(feature = "bare-metal", feature = "identity-only", feature = "perf-only",
                  feature = "perf-brutal-only", feature = "stress-only",
                  feature = "adv-only", feature = "chaos-only", feature = "fuzz-only",
                  feature = "b2-only", feature = "bp2-only", feature = "perf-iso")))]
    let _ = ctx.spawn("observe");

    // Persistence (v2; docs/persistence.md) - block-driver + fs. Spawned in bare-metal
    // (so a usable OS / Prime sees its disk and `drives flash` can format it) and in the
    // blockdev smoke-test. block-driver MUST precede fs (fs's send-peer cap to it wires
    // from the name table at fs's spawn), and BOTH must precede the shell (the shell's
    // send-peer cap to `fs` wires the same way). On a machine with no SATA disk both come
    // up and idle gracefully (block-driver: "no controller"; fs: raw-tolerant).
    // block-driver has no peers; fs's only peer is block-driver, provided from the map. Clients
    // reacquire names via the kernel directory.
    //
    // block-driver is also spawned in `identity-only` builds - it idles harmlessly with no disk
    // (QEMU has no -drive there: "no controller"), giving §22 Test 11 a restartable victim to kill.
    // `ensure_*` (Phase 6): spawn on a fresh boot, ADOPT the running instance on a supervisor respawn.
    #[cfg(any(feature = "bare-metal", feature = "blockdev", feature = "identity-only"))]
    // block-driver: core 0 on ARM, unpinned elsewhere. On ARM it reaches a USB stick through the
    // `dwc2` SERVICE (spawned just above), not through kernel syscalls - the in-kernel DWC2 stack was
    // deleted in slice 5. The core-0 preference survives that change because the placement decision
    // lives in the kernel's `ServiceConfig.preferred_core`, which both the boot and restart paths read.
    // No override: the kernel's `ServiceConfig.preferred_core` decides, and it is arch-conditional
    // (0 on ARM for the reason above). Overriding here pinned only the BOOT spawn - the restart path
    // passes no override, so a respawned block-driver silently landed on a different core than the
    // one it requires. One source of placement, consulted by both paths.
    // time + control: started BEFORE the shell, because the shell asks `time` for the clock source on
    // its first prompt and net-stack asks it to accept an SNTP reading. Neither holds hardware, so
    // neither can delay the prompt the way a driver bring-up would.
    ensure_mapped(&ctx, &mut name_map, "time", 0xFFFF);
    ensure_mapped(&ctx, &mut name_map, "control", 0xFFFF);
    // dwc2 (arm32): the Pi 2's ENTIRE USB stack - storage, keyboard and networking all ride on this
    // one service. Spawned BEFORE block-driver and nic-driver because both name it as a send_peer: a
    // peer that does not exist yet costs them their direct cap and forces a name-wire later.
    //
    // This was MISSING from arm32 slice 5, which deleted the in-kernel DWC2 driver without starting
    // its replacement. The board booted with no USB at all and was rescued only by a human typing
    // `spawn dwc2`. The log said so five different ways and none of them said "nobody started it".
    //
    // `ensure_mapped` adopts a running instance rather than spawning a second: the supervisor is
    // restartable (Phase 6), so this line runs again on every respawn, and two drivers on one
    // controller is a worse failure than the one being fixed.
    // Gated on the ARCHITECTURE only, deliberately. The test-build feature list that guards `xhci`
    // below buys nothing here: on this board `dwc2` is not one driver among several, it is the only
    // path to storage, keyboard and network, so every arm32 build that boots at all wants it. Fewer
    // conditions also means fewer ways for this spawn to silently not happen - which is the exact
    // failure being fixed.
    #[cfg(target_arch = "arm")]
    ensure_mapped(&ctx, &mut name_map, "dwc2", 0xFFFF);

    ensure_mapped(&ctx, &mut name_map, "block-driver", 0xFFFF);
    // fs needs a disk → bare-metal / blockdev only.
    #[cfg(any(feature = "bare-metal", feature = "blockdev"))]
    ensure_wired(&ctx, &mut name_map, "fs", &["block-driver"]);

    // shell: the interactive prompt. Spawned in bare-metal (the USB image rests here) and full builds;
    // excluded from test-specific builds. Its `fs` peer is wired from the supervisor's map.
    // Phase 6: ensure_wired adopts a running shell on a supervisor respawn instead of duplicating it.
    //
    // Spawned EARLY - before the network services - and that ordering is deliberate: **the user's prompt
    // must never wait on hardware bring-up.** It was briefly moved last (so the boot log would finish
    // before the prompt painted, a cosmetic win); on real hardware, once the Pi's NIC actually came up,
    // net-stack's DHCP -> ARP -> ICMP dance ran its full ~45 s of budgets with nothing answering, and the
    // prompt sat behind it. A tidy boot log is not worth a 45-second wait for the shell: the shell comes
    // up first and net-stack configures itself in the background (it already self-configures on link-up).
    #[cfg(not(any(feature = "identity-only", feature = "perf-only",
                  feature = "perf-brutal-only", feature = "stress-only",
                  feature = "adv-only", feature = "chaos-only", feature = "fuzz-only",
                  feature = "b2-only", feature = "bp2-only", feature = "perf-iso")))]
    ensure_wired(&ctx, &mut name_map, "shell", &["fs"]);

    // counter (examples/counter): a STATEFUL example that survives its OWN restart by persisting its
    // running count to `fs` and reconstructing it on spawn (§14 restart, §15 persistence). Spawned
    // ONLY in the `counter-test` build (`osdev test counter`) - its per-tick writes to /counter.dat
    // would be disk-write noise in the daily-driver image and identity build. Wired from the map
    // like the shell (its `fs` send peer); on a supervisor respawn ensure_wired adopts the running
    // instance instead of duplicating it. block-driver + fs are spawned above (bare-metal set).
    #[cfg(feature = "counter-test")]
    ensure_wired(&ctx, &mut name_map, "counter", &["fs"]);

    // reply-server + asker (examples/): the request/reply (RPC) pair. Spawned ONLY in the
    // `reply-test` build (`osdev test reply-server`); idle/absent everywhere else. reply-server owns
    // its endpoint and has no send peer (it replies over each request's embedded reply cap), so it is
    // recorded in the name-cap map (ensure_mapped) and MUST precede asker - asker's SEND cap to
    // reply-server is wired from the map at asker's spawn (like ping after pong). asker sends a
    // request, reply-server replies, asker checks the echo (§8/§8.9). On a supervisor respawn
    // ensure_* adopts the running instances instead of duplicating them.
    #[cfg(feature = "reply-test")]
    {
        ensure_mapped(&ctx, &mut name_map, "reply-server", 0xFFFF);
        ensure_wired(&ctx, &mut name_map, "asker", &["reply-server"]);
    }

    // holder + resource-server (examples/): the delegated-resource-capability pair (§7.10). Spawned
    // ONLY in the `resource-test` build (`osdev test resource-server`); idle/absent everywhere else.
    // `holder` owns its endpoint and is the GRANT target, so it is recorded in the name-cap map
    // (ensure_mapped) and MUST precede resource-server - resource-server's SEND cap to `holder` is
    // wired from the map at its spawn (it GRANTs holder the minted resource cap). resource-server then
    // mints a resource, narrows a READ-ONLY copy, grants it to holder, and serves holder's invocations;
    // holder proves use / non-escalation / revoke. This is the REVERSE order of reply/asker (here the
    // server sends to the client). On a supervisor respawn ensure_* adopts the running instances.
    #[cfg(feature = "resource-test")]
    {
        ensure_mapped(&ctx, &mut name_map, "holder", 0xFFFF);
        ensure_wired(&ctx, &mut name_map, "resource-server", &["holder"]);
    }

    // xhci: USB host-controller driver (§12). Spawned in bare-metal + full builds; the kernel maps its
    // controller's MMIO BAR at spawn (Stage 2). ALWAYS spawned (unlike ehci/nic-driver below): xhci is
    // the near-universal primary USB controller, and even with no controller its idle path signals
    // input-ready - the boot-screen clear that lets the shell show `gsh>`. Skipping it would leave the
    // serial shell waiting for a prompt forever, so the idle-core cost on a (rare) xHCI-less machine is
    // the right trade.
    #[cfg(not(any(feature = "identity-only", feature = "perf-only",
                  feature = "perf-brutal-only", feature = "stress-only",
                  feature = "adv-only", feature = "chaos-only", feature = "fuzz-only",
                  feature = "b2-only", feature = "bp2-only", feature = "perf-iso")))]
    // Not on the arm32 port: there the USB host controller (DWC2) is driven IN-KERNEL, because that
    // port does not route device IRQs to userspace yet. There is no userspace driver ELF to load, so
    // this spawn can only ever fail - and it failed LOUDLY every boot with `LoadFailed(TooSmall)`,
    // which is a real error message for a service that was never supposed to exist here. Loud failure
    // is right for a thing that should have worked (invariant 12); a permanent error for a thing that
    // is not part of this architecture is just noise that trains the reader to ignore the log.
    //
    // **aarch64 is off that list permanently - the in-kernel driver is deleted.** The Pi 4's
    // VL805 is a PCIe endpoint the kernel already discovers and BAR-assigns; with the feature the
    // kernel publishes it in `pci::XHCI_*` and stops driving it, and the SAME service x86 has always
    // spawned takes over - same binary, same CONSOLE_PUSH capability, same MMIO/DMA grant path. That
    // is the point of reusing it: a second xHCI implementation would be the duplication Commandment
    // III forbids, and this controller is standards-conformant silicon behind a standards-conformant
    // bus, so there was nothing to reimplement.
    //
    // Kept as a supervisor feature rather than an unconditional aarch64 spawn because the KERNEL side
    // is a feature too. Spawn the service without it and two drivers own one controller.
    // aarch64 spawns it unconditionally now: the in-kernel driver is DELETED, so this service is the
    // only thing that can drive the controller. arm32 is excluded because its controller is a DWC2,
    // not an xHCI - a different driver, spawned above. Its in-kernel stack is deleted too (slice 5);
    // this comment used to say otherwise, and that stale sentence is exactly why nothing filled the
    // gap when the kernel driver went away.
    #[cfg(not(target_arch = "arm"))]
    // Adopt if already running - the same omission `nic-driver` and `net-stack` had. Both USB
    // drivers are in MANAGED, so the supervisor watches them for death; on its own respawn a bare
    // `spawn_*` is refused with "already running" and the driver is left out of the name-cap map.
    ensure_mapped(&ctx, &mut name_map, "xhci", 0xFFFF);

    // ehci: USB 2.0 host-controller driver (§12) for the back ports. Same builds
    // as xhci; the kernel grants its MMIO/DMA at spawn (E1b+). Skipped if the PCI
    // scan found no EHCI controller (e.g. the Wyse 5070 has none), freeing the core
    // an idle ehci would busy-hold.
    #[cfg(not(any(feature = "identity-only", feature = "perf-only",
                  feature = "perf-brutal-only", feature = "stress-only",
                  feature = "adv-only", feature = "chaos-only", feature = "fuzz-only",
                  feature = "b2-only", feature = "bp2-only", feature = "perf-iso")))]
    #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
    if ctx.ehci_present() {
        // Adopt if already running, as above. `ehci_present()` still gates whether it should exist
        // at all; this only decides spawn-versus-adopt once it should.
        ensure_mapped(&ctx, &mut name_map, "ehci", 0xFFFF);
    } else {
        ctx.log("supervisor: no EHCI controller (PCI scan) - not starting ehci (frees a core)");
    }

    // nic-driver: the userspace NIC driver (§12, docs/networking.md, Phase 1). Same builds as the
    // USB drivers; the kernel maps the Intel e1000's BAR0 by name at spawn. On a non-e1000 NIC
    // (the T630's Realtek) it gets no mapping and idles. Restart-on-death wiring (the MANAGED set)
    // lands with the DMA/IRQ phase, when it holds device state worth recovering.
    //
    // ALWAYS spawned (unlike ehci above): a NIC exists on nearly all hardware, so a presence-gated skip
    // (`ctx.nic_present()`, query 18 bit2) was parked as low-value. The SDK accessor stays for an easy
    // resume. On a NIC-less / unsupported-NIC box nic-driver + net-stack come up and idle gracefully
    // (nic-driver serves empty replies; net-stack degrades, no hang).
    #[cfg(not(any(feature = "identity-only", feature = "perf-only",
                  feature = "perf-brutal-only", feature = "stress-only",
                  feature = "adv-only", feature = "chaos-only", feature = "fuzz-only",
                  feature = "b2-only", feature = "bp2-only", feature = "perf-iso")))]
    // ADOPT if already running, like every other managed service. These two used `spawn_*`
    // directly, which always tries to SPAWN - so on a supervisor respawn the kernel refused
    // ("spawn 'net-stack' rejected: already running"), the supervisor got no endpoint cap back, and
    // the still-running services ended up in neither branch: not adopted, not respawned, and absent
    // from the name-cap map. Networking was dead from that moment (`net tx 0/0`, `0 to us`) and the
    // selfcheck lease case failed twenty minutes later, which is how it surfaced.
    //
    // They were simply missed when adoption came in with Path C / Phase 6; `fs`, `shell`, `dwc2`,
    // `block-driver`, `time` and `control` were all converted. Nothing else about them is special.
    ensure_mapped(&ctx, &mut name_map, "nic-driver", 0xFFFF);

    // net-stack: the model-agnostic half of networking (docs/networking.md). Speaks ARP/IP over raw
    // frames THROUGH nic-driver's frame interface, so it is spawned right AFTER nic-driver and WIRED
    // to it (its SEND cap to nic-driver comes from the name-cap map). Same builds as nic-driver; on a
    // non-e1000 NIC, nic-driver serves empty replies, so net-stack degrades (no hang) rather than
    // resolving. Restart-on-death wiring (the MANAGED set) lands with Phase 2, when it holds protocol
    // state worth recovering.
    #[cfg(not(any(feature = "identity-only", feature = "perf-only",
                  feature = "perf-brutal-only", feature = "stress-only",
                  feature = "adv-only", feature = "chaos-only", feature = "fuzz-only",
                  feature = "b2-only", feature = "bp2-only", feature = "perf-iso")))]
    // `ensure_wired` adopts a running net-stack rather than duplicating it. On the adopt path the
    // peers are deliberately NOT re-installed: the running service still holds the caps its original
    // spawn gave it - they live in its own table and a supervisor restart does not touch them.
    ensure_wired(&ctx, &mut name_map, "net-stack", &["nic-driver"]);

    // Phase 1 (docs/naming-design.md): report the shadow name→cap map. Proves the supervisor now
    // holds an endpoint cap to every real service it spawned - the future name authority. Nothing
    // reads it yet (Phase 0b/3 wire dependents from it; Phase 4 brokers reacquisition through it).
    ctx.log_fmt(format_args!("supervisor: name-cap map holds {} service(s)", name_map.count));

    // Reconverge to consistency before trusting the notification stream (Path C / Phase 6). A no-op on a
    // fresh boot (everything above just came up); on a supervisor RESPAWN (`kill all-services`) this
    // catches any managed service still Dead/settling in the churn - including a shell that `ensure_*`
    // adopted as a stale cap - and only returns once the roster is truly satisfied. From here the recv
    // loop plus the live-supervisor notification path carry every future death.
    converge(&ctx, &mut name_map);

    ctx.log("supervisor: ready");

    // Death-notification restart loop (H11 ph6; extended for fs + block-driver in Phase D).
    // The kernel enqueues the name of a dead restartable service to our endpoint; we respawn
    // it. `recv` BLOCKS, so the core still reaches the idle/halt path and runs cool between
    // deaths (no polling). Restartable services routed here: `block-driver`, `fs`, `shell`, `xhci`,
    // `ehci`, `logger`. The supervisor itself is restartable too (Phase 6) but by the KERNEL - a dead
    // task can't respawn itself; the only death that is unrecoverable is the kernel's. Other
    // restart/kill commands still arrive via the
    // COM2 control channel (control::process_pending in the timer ISR).
    //
    // If this build gave us no endpoint (minimal test manifests), fall back to park.
    if ctx.recv_handle().is_none() {
        ctx.park();
    }
    loop {
        let msg = ctx.recv();
        let name = core::str::from_utf8(msg.payload_bytes()).unwrap_or("");
        // Two recovery paths race after a mass-kill: the convergence (converge()/reconcile()) may have
        // ALREADY respawned this service before its queued death notification reached us. If it is
        // already alive, a restart here hits the kernel singleton guard ("already running") and logs a
        // FALSE "restart FAILED" - a loud non-failure that erodes trust in the signal (§26.4). Skip the
        // doomed restart quietly (log "already recovered"); still run the reconcile backstop below so a
        // genuinely-dropped OTHER death is caught this iteration. Same `task_stat` liveness the
        // convergence uses, so the two paths agree on "alive".
        if !name.is_empty() && name_alive(&ctx, name) {
            ctx.log_fmt(format_args!("supervisor: {} already recovered (reconcile won the race)", name));
            reconcile(&ctx, &mut name_map);
            continue;
        }
        // Restartable services (§6.1): fs + block-driver (Phase D). Phase 3c/4 (docs/naming-design.md):
        // respawn WIRED FROM THE MAP - same peers as at boot - and the spawn refreshes the map with
        // the new instance's cap (record updates in place, so a kill-storm can't grow the map). The
        // restarted service is supervisor-wired just like at boot; clients reacquire it by name via
        // the kernel directory (§14.3). The "died/restarted" log lines are kept (tests gate on them).
        match name {
            "block-driver" => {
                ctx.log("supervisor: block-driver died, restarting");
                if respawn_retry(&ctx, &mut name_map, "block-driver") { ctx.log("supervisor: block-driver restarted"); }
                else { ctx.log("supervisor: block-driver restart FAILED"); }
            }
            "fs" => {
                ctx.log("supervisor: fs died, restarting");
                if respawn_retry(&ctx, &mut name_map, "fs") { ctx.log("supervisor: fs restarted"); }
                else { ctx.log("supervisor: fs restart FAILED"); }
            }
            "shell" => {
                // The user's interface is restartable too ("nothing escapes"): a crash or a
                // deliberate `kill shell` respawns a FRESH prompt. spawn_wired spawns a new instance
                // (the singleton guard only blocks a LIVE duplicate), re-granting its console-read +
                // service_control caps and wiring its `fs` peer from the map. The in-flight command
                // is lost (state is not resumed, §14.2/§25) but the session recovers.
                ctx.log("supervisor: shell died, restarting");
                if respawn_retry(&ctx, &mut name_map, "shell") { ctx.log("supervisor: shell restarted"); }
                else { ctx.log("supervisor: shell restart FAILED"); }
            }
            // The USB host drivers + logger are directly restartable now: their OWN death respawns
            // them immediately (re-granting MMIO/DMA/IRQ caps + re-initialising the controller),
            // instead of waiting for a lucky supervisor respawn. This is what keeps a `chaos
            // max-carnage` that kills `xhci`/`ehci` in its last rounds from leaving the keyboard dead.
            "xhci" => {
                ctx.log("supervisor: xhci died, restarting");
                if respawn_retry(&ctx, &mut name_map, "xhci") { ctx.log("supervisor: xhci restarted"); }
                else { ctx.log("supervisor: xhci restart FAILED"); }
            }
            "ehci" => {
                ctx.log("supervisor: ehci died, restarting");
                if respawn_retry(&ctx, &mut name_map, "ehci") { ctx.log("supervisor: ehci restarted"); }
                else { ctx.log("supervisor: ehci restart FAILED"); }
            }
            // dwc2 (ARM32 only): the Pi 2's USB host. block-driver and nic-driver both name it as a
            // peer, so its permanent death takes storage, the keyboard and networking with it - which
            // is exactly why nothing may be exempt from restart (C5-1). Its respawn re-grants the
            // DWC2 MMIO window, DMA arena and IRQ, re-initialises the controller and re-enumerates;
            // clients reacquire it by name and retry (§14.3).
            "dwc2" => {
                ctx.log("supervisor: dwc2 died, restarting");
                if respawn_retry(&ctx, &mut name_map, "dwc2") { ctx.log("supervisor: dwc2 restarted"); }
                else { ctx.log("supervisor: dwc2 restart FAILED"); }
            }
            "logger" => {
                ctx.log("supervisor: logger died, restarting");
                if respawn_retry(&ctx, &mut name_map, "logger") { ctx.log("supervisor: logger restarted"); }
                else { ctx.log("supervisor: logger restart FAILED"); }
            }
            // counter (examples/counter, counter-test build): respawn it wired to `fs` - the fresh
            // instance reconstructs its count from /counter.dat (§14/§15). The "died/restarted" lines
            // are what `osdev test counter` gates on. (Only ever sent when counter is actually live.)
            "counter" => {
                ctx.log("supervisor: counter died, restarting");
                if respawn_retry(&ctx, &mut name_map, "counter") { ctx.log("supervisor: counter restarted"); }
                else { ctx.log("supervisor: counter restart FAILED"); }
            }
            // The NIC stack is restartable too: nic-driver re-grants its MMIO/DMA/IRQ (its DMA arena is
            // reserved once and reused, NIC_DMA_PHYS) + re-inits the controller; net-stack re-runs its
            // DHCP/ARP/ICMP dance and re-registers. Clients (the shell's net/ping) reacquire net-stack by
            // name (§14.3). net-stack also reacquires nic-driver by name, so either death order recovers.
            "nic-driver" => {
                ctx.log("supervisor: nic-driver died, restarting");
                if respawn_retry(&ctx, &mut name_map, "nic-driver") { ctx.log("supervisor: nic-driver restarted"); }
                else { ctx.log("supervisor: nic-driver restart FAILED"); }
            }
            "net-stack" => {
                ctx.log("supervisor: net-stack died, restarting");
                if respawn_retry(&ctx, &mut name_map, "net-stack") { ctx.log("supervisor: net-stack restarted"); }
                else { ctx.log("supervisor: net-stack restart FAILED"); }
            }
            _ => {}
        }
        // Reconcile backstop: catch any managed service whose death notification was DROPPED under the
        // storm (our 16-deep endpoint overflowed, or a flood clogged it) - it would otherwise stay dead
        // forever (the "fs gone from observe after a storm" bug). A storm always has a next death to
        // ride, so a dropped one is recovered on the following notification. Cheap when nothing is dead.
        reconcile(&ctx, &mut name_map);
    }
}

// ---------------------------------------------------------------------------
// Extended probes - all non-identity test categories.
//
// Feature-gated variants (in priority order):
//   identity-only     → spawn nothing (fastest boot, used by `osdev test identity`)
//   perf-only         → spawn only regular perf-b* probes (used by `osdev test perf`)
//   perf-brutal-only  → spawn only brutal perf-bp* probes (used by `osdev test perf-brutal`)
//   (none)            → spawn everything (used by `osdev build` / `osdev run`)
// ---------------------------------------------------------------------------

// bare-metal: no probes at all - spawn_extended_probes is never called, but
// the function must exist so the linker is happy.
#[cfg(feature = "bare-metal")]
#[inline(always)]
fn spawn_extended_probes(_ctx: &ServiceContext) {}

// idle-only (S8): no probes, no pong/ping.
#[cfg(feature = "idle-only")]
#[inline(always)]
fn spawn_extended_probes(_ctx: &ServiceContext) {}

// identity-only: skip all extended probes.
#[cfg(all(not(feature = "bare-metal"), feature = "identity-only"))]
#[inline(always)]
fn spawn_extended_probes(_ctx: &ServiceContext) {}

// perf-only: spawn only the regular performance benchmark probe services.
// Cuts spawn wait from ~18-120 s (178 probes) to ~2-5 s (~30 services) on TCG.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), feature = "perf-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // Sender/controller before echo/recv so the sender's endpoint is registered
    // when the echo partner's SEND cap is wired at spawn time.
    // perf-b5-victim must be registered before perf-b5 starts cycling.
    let _ = probes::probe(&ctx, "perf-b1");
    let _ = probes::probe(&ctx, "perf-b1-echo");
    let _ = probes::probe(&ctx, "perf-b2");
    let _ = probes::probe(&ctx, "perf-b2-echo");
    let _ = probes::probe(&ctx, "perf-b3");
    let _ = probes::probe(&ctx, "perf-b4");
    let _ = probes::probe(&ctx, "perf-b5-victim");
    let _ = probes::probe(&ctx, "perf-b5");
    let _ = probes::probe(&ctx, "perf-b7");
    let _ = probes::probe(&ctx, "perf-b8");
    let _ = probes::probe(&ctx, "perf-b9-recv");
    let _ = probes::probe(&ctx, "perf-b9");
    let _ = probes::probe(&ctx, "perf-b10");
}

// perf-brutal-only: spawn only the brutal performance benchmark probe services.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), feature = "perf-brutal-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    let _ = probes::probe(&ctx, "perf-bp1");
    let _ = probes::probe(&ctx, "perf-bp1-echo");
    let _ = probes::probe(&ctx, "perf-bp2");
    let _ = probes::probe(&ctx, "perf-bp2-echo");
    let _ = probes::probe(&ctx, "perf-bp3");
    let _ = probes::probe(&ctx, "perf-bp4");
    let _ = probes::probe(&ctx, "perf-bp5-victim");
    let _ = probes::probe(&ctx, "perf-bp5");
    let _ = probes::probe(&ctx, "perf-bp7");
    let _ = probes::probe(&ctx, "perf-bp8");
    let _ = probes::probe(&ctx, "perf-bp9-recv");
    let _ = probes::probe(&ctx, "perf-bp9");
    let _ = probes::probe(&ctx, "perf-bp10");
}

// stress-only: spawn only the S1-S10 stress probe services.
// All stress probes are self-contained (use ctx.kill/ctx.spawn internally);
// no QEMU control port required - safe for real hardware.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), feature = "stress-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // Receivers/victims must register before their controllers so endpoints
    // exist when sender SEND caps are wired at spawn time.
    let _ = probes::probe(&ctx, "stress-s1-recv");
    let _ = probes::probe(&ctx, "stress-s1");
    let _ = probes::probe(&ctx, "stress-s2-victim");
    let _ = probes::probe(&ctx, "stress-s2");
    let _ = probes::probe(&ctx, "stress-s3-recv");    // core 1 - cross-core thrash receiver
    let _ = probes::probe(&ctx, "stress-s3-send");    // core 0 - cross-core thrash sender
    let _ = probes::probe(&ctx, "stress-s4-victim");
    let _ = probes::probe(&ctx, "stress-s4");
    let _ = probes::probe(&ctx, "stress-s5-victim");
    let _ = probes::probe(&ctx, "stress-s5");
    let _ = probes::probe(&ctx, "stress-s6");         // self-referential; endpoint registered at spawn
    let _ = probes::probe(&ctx, "stress-s7");
    let _ = probes::probe(&ctx, "stress-s8");
    let _ = probes::probe(&ctx, "stress-s9-recv");    // core 2 - IPI storm receiver
    let _ = probes::probe(&ctx, "stress-s9-send-a"); // core 0 → core 2
    let _ = probes::probe(&ctx, "stress-s9-send-b"); // core 1 → core 2
    let _ = probes::probe(&ctx, "stress-s10-victim"); // core 1 - cascading revocation target
    let _ = probes::probe(&ctx, "stress-s10");        // core 0 - kills victim cross-core
}

// chaos-only: spawn only the C2-C7 chaos probe services.
// C1 (degraded SMP boot) and C4 (minimal RAM) use bare-metal + hardware
// reconfiguration instead of probes.  All probes here are self-contained.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), not(feature = "adv-only"), feature = "chaos-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // BC7/C7 victims must be registered before their controllers so endpoints
    // exist when the controller's SEND caps are wired at spawn time.
    let _ = probes::probe(&ctx, "chaos-c2");          // non-TCB page fault, system continues
    let _ = probes::probe(&ctx, "chaos-c2-monitor");  // witness - alive after c2 faults
    let _ = probes::probe(&ctx, "chaos-c3");          // alloc-deny pressure cycles
    let _ = probes::probe(&ctx, "chaos-c5");          // recursive yields (kernel stack depth)
    let _ = probes::probe(&ctx, "chaos-c6-hog");      // tight-loop hog on core 3
    let _ = probes::probe(&ctx, "chaos-c6-monitor");  // witness on core 0
    let _ = probes::probe(&ctx, "chaos-c7-victim");   // passive recv target on core 2
    let _ = probes::probe(&ctx, "chaos-c7");          // TLB shootdown controller on core 1
}

// adv-only: spawn only the A1-A10 adversarial probe services.
// All adversarial probes are self-contained - no QEMU control port required.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), feature = "adv-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // adv-a11 first: it is self-contained (no peers, no IPC) and logs its pass
    // line within the first second, so it completes even when the CPU-heavy
    // attackers (A1's 10k-iteration loop, A2 brute-force) would otherwise starve
    // a TCG-throttled boot. Order is functionally irrelevant for it.
    let _ = probes::probe(&ctx, "adv-a11"); // introspection gated - denied without INTROSPECT cap
    let _ = probes::probe(&ctx, "adv-a12"); // reboot gated - denied without REBOOT cap (self-contained)
    let _ = probes::probe(&ctx, "adv-a13"); // AcquireSendCap gated - denied without ACQUIRE_ANY (self-contained)
    // Passive/victim services before their attackers so endpoints exist when
    // attacker SEND caps are wired at spawn time.
    let _ = probes::probe(&ctx, "adv-a1");
    let _ = probes::probe(&ctx, "adv-a2");
    let _ = probes::probe(&ctx, "adv-a3");
    let _ = probes::probe(&ctx, "adv-a4");
    let _ = probes::probe(&ctx, "adv-a5-victim"); // passive - killed by adv-a5
    let _ = probes::probe(&ctx, "adv-a5");
    let _ = probes::probe(&ctx, "adv-a6");
    let _ = probes::probe(&ctx, "adv-a7-recv");   // passive recv - registered before sender
    let _ = probes::probe(&ctx, "adv-a7");
    let _ = probes::probe(&ctx, "adv-a8");        // tight-loop attacker
    let _ = probes::probe(&ctx, "adv-a8-witness");
    let _ = probes::probe(&ctx, "adv-a9");
    let _ = probes::probe(&ctx, "adv-a10");
    // A14 (kernel-audit C1/C2 regression): two ring-3 faulters (#GP, #DE) that must be KILLED by the
    // kernel, and a monitor that witnesses the system surviving both. Self-contained, no peers.
    let _ = probes::probe(&ctx, "adv-fault-gp");
    let _ = probes::probe(&ctx, "adv-fault-de");
    let _ = probes::probe(&ctx, "adv-fault-mon");
    // A15 (kernel-audit V1 regression): a bad user pointer to a syscall must kill the CALLER, not halt
    // the machine. A faulter (bad ptr to `log`) + a monitor that witnesses the system surviving.
    let _ = probes::probe(&ctx, "adv-fault-usercopy");
    let _ = probes::probe(&ctx, "adv-fault-usercopy-mon");
}

// fuzz-only: spawn only the §22 fuzz probe services (F1/F2/F5/F6/F7/F8 + brutal
// BF1/BF2/BF5/BF6/BF7/BF8). All self-run and print "fuzz: F* pass (n/n)" over
// serial - no QEMU control port required, safe for real hardware. Recv-endpoint
// victims/targets are spawned before their controllers so endpoints are registered
// when the controllers' SEND caps are wired at spawn time (same ordering rule as
// every other category). F3/BF3 (ELF-loader fuzz) need a separate test-bad-elf
// kernel build that halts after fuzzing; F4 is host-side contract validation only.
#[cfg(all(not(feature = "bare-metal"), not(feature = "idle-only"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), not(feature = "adv-only"), not(feature = "chaos-only"), feature = "fuzz-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // Regular fuzz probes (Milestone 10 Phase 1).
    let _ = probes::probe(&ctx, "fuzz-f1");
    let _ = probes::probe(&ctx, "fuzz-f2");
    let _ = probes::probe(&ctx, "fuzz-f5-recv");
    let _ = probes::probe(&ctx, "fuzz-f5");
    let _ = probes::probe(&ctx, "fuzz-f6-recv");
    let _ = probes::probe(&ctx, "fuzz-f6");
    let _ = probes::probe(&ctx, "fuzz-f7-victim");
    let _ = probes::probe(&ctx, "fuzz-f7");
    let _ = probes::probe(&ctx, "fuzz-f8");
    // Brutal fuzz probes (Milestone 17) - heavier iteration counts; run fast on
    // real silicon (no TCG throttling). Recv/victim partners first.
    let _ = probes::probe(&ctx, "fuzz-bf5-recv");
    let _ = probes::probe(&ctx, "fuzz-bf5");
    let _ = probes::probe(&ctx, "fuzz-bf6-recv");
    let _ = probes::probe(&ctx, "fuzz-bf6");
    let _ = probes::probe(&ctx, "fuzz-bf7-victim");
    let _ = probes::probe(&ctx, "fuzz-bf7");
    let _ = probes::probe(&ctx, "fuzz-bf1");
    let _ = probes::probe(&ctx, "fuzz-bf2");
    let _ = probes::probe(&ctx, "fuzz-bf8");
}

// b2-only: spawn only the regular B2 cross-core IPC probe pair (isolation build).
// No other benchmarks running - eliminates concurrent IPI noise from B5 spawn/kill
// and B6 restart cycles so the blocking round-trip can complete on Goldmont+.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), not(feature = "adv-only"), not(feature = "chaos-only"), feature = "b2-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    let _ = probes::probe(&ctx, "perf-b2");      // B2 sender (core 0) - registers endpoint first
    let _ = probes::probe(&ctx, "perf-b2-echo"); // B2 echo  (core 1) - wires SEND cap to perf-b2
}

// bp2-only: spawn only the brutal BP2 cross-core IPC probe pair (isolation build).
// Brutal equivalent of b2-only - higher iteration count, same isolation rationale.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), not(feature = "adv-only"), not(feature = "chaos-only"), not(feature = "b2-only"), feature = "bp2-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    let _ = probes::probe(&ctx, "perf-bp2");      // BP2 sender (core 0) - registers endpoint first
    let _ = probes::probe(&ctx, "perf-bp2-echo"); // BP2 echo  (core 1) - wires SEND cap to perf-bp2
}

// perf-iso: isolate ONE brutal perf probe (+ its partners) - no ping/pong, no
// other probes - for clean uncontended per-op latency on hardware. The probe is
// selected by an iso-bpN sub-feature (each pulls in perf-iso). bp5 covers both
// BP5 (spawn) and BP6 (restart) - same probe. Partners are spawned first
// (victim before perf-bp5; recv before perf-bp9) so endpoints/caps are wired.
#[cfg(feature = "perf-iso")]
fn spawn_extended_probes(ctx: &ServiceContext) {
    #[cfg(feature = "iso-bp3")]  { let _ = probes::probe(&ctx, "perf-bp3"); }
    #[cfg(feature = "iso-bp5")]  { let _ = probes::probe(&ctx, "perf-bp5-victim"); let _ = probes::probe(&ctx, "perf-bp5"); }
    #[cfg(feature = "iso-bp7")]  { let _ = probes::probe(&ctx, "perf-bp7"); }
    #[cfg(feature = "iso-bp9")]  { let _ = probes::probe(&ctx, "perf-bp9-recv"); let _ = probes::probe(&ctx, "perf-bp9"); }
    #[cfg(feature = "iso-bp10")] { let _ = probes::probe(&ctx, "perf-bp10"); }
    // Cross-core STRESS isolation (recv/partners first so endpoints are registered).
    #[cfg(feature = "iso-s3")]   { let _ = probes::probe(&ctx, "stress-s3-recv"); let _ = probes::probe(&ctx, "stress-s3-send"); }
    // iso-s5: victim first so its endpoint exists when stress-s5's caps are wired.
    #[cfg(feature = "iso-s5")]   { let _ = probes::probe(&ctx, "stress-s5-victim"); let _ = probes::probe(&ctx, "stress-s5"); }
    // iso-c7: victim (core 2) first so its endpoint exists when chaos-c7's (core 1)
    // SEND cap is wired; controller then drives 30 cross-core kill/respawn cycles.
    #[cfg(feature = "iso-c7")]   { let _ = probes::probe(&ctx, "chaos-c7-victim"); let _ = probes::probe(&ctx, "chaos-c7"); }
    // iso-xsend: receiver (core 2) first so its endpoint exists when xsend's (core 1)
    // SEND cap is wired; sender then times bare cross-core try_sends to a LIVE receiver.
    #[cfg(feature = "iso-xsend")] { let _ = probes::probe(&ctx, "xsend-recv"); let _ = probes::probe(&ctx, "xsend"); }
    // iso-xlife: both victims first so they exist when the controller's first kill
    // fires; controller (core 1) then times kill/spawn of near (core 1) and far (core 2).
    #[cfg(feature = "iso-xlife")] { let _ = probes::probe(&ctx, "xlife-near"); let _ = probes::probe(&ctx, "xlife-far"); let _ = probes::probe(&ctx, "xlife"); }
    #[cfg(feature = "iso-s9")]   {
        let _ = probes::probe(&ctx, "stress-s9-recv");
        let _ = probes::probe(&ctx, "stress-s9-send-a");
        let _ = probes::probe(&ctx, "stress-s9-send-b");
    }
    let _ = ctx; // used by every sub-feature arm; silences the no-arm case
}

// Full build: spawn all non-identity probe categories.
#[cfg(not(any(feature = "bare-metal", feature = "idle-only", feature = "identity-only", feature = "perf-only", feature = "perf-brutal-only", feature = "stress-only", feature = "adv-only", feature = "chaos-only", feature = "fuzz-only", feature = "b2-only", feature = "bp2-only", feature = "perf-iso")))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // --- Brutal adversarial test probes - Milestone 20 ---
    // Spawned EARLY, before property/stress kill-respawn loops start, so the
    // supervisor's spawn calls land while the system is still lightly loaded.
    // Victims/passive services must be registered before their attackers so
    // their endpoints exist when the attacker's SEND caps are wired at spawn.
    let _ = probes::probe(&ctx, "adv-ba1");
    let _ = probes::probe(&ctx, "adv-ba2");
    let _ = probes::probe(&ctx, "adv-ba3");
    let _ = probes::probe(&ctx, "adv-ba4");
    let _ = probes::probe(&ctx, "adv-ba5-victim"); // registered before adv-ba5
    let _ = probes::probe(&ctx, "adv-ba5");
    let _ = probes::probe(&ctx, "adv-ba6");        // recv endpoint registered so self-fill works
    let _ = probes::probe(&ctx, "adv-ba7-recv");   // passive recv registered before sender
    let _ = probes::probe(&ctx, "adv-ba7");
    let _ = probes::probe(&ctx, "adv-ba8");        // tight-loop hog
    let _ = probes::probe(&ctx, "adv-ba8-witness");
    let _ = probes::probe(&ctx, "adv-ba9");
    let _ = probes::probe(&ctx, "adv-ba10");

    // --- Brutal chaos-test probes - Milestone 21 ---
    // Spawned EARLY before property/stress kill-respawn loops start.
    // BC2: 5 simultaneous faulters registered before the monitor so all 5
    // fault concurrently before the monitor starts counting yields.
    // BC7: victim registered before controller so its endpoint exists when
    // the controller's SEND cap is wired at spawn time.
    let _ = probes::probe(&ctx, "chaos-bc2-a");
    let _ = probes::probe(&ctx, "chaos-bc2-b");
    let _ = probes::probe(&ctx, "chaos-bc2-c");
    let _ = probes::probe(&ctx, "chaos-bc2-d");
    let _ = probes::probe(&ctx, "chaos-bc2-e");
    let _ = probes::probe(&ctx, "chaos-bc2-monitor");
    let _ = probes::probe(&ctx, "chaos-bc3");
    let _ = probes::probe(&ctx, "chaos-bc5");
    let _ = probes::probe(&ctx, "chaos-bc6-hog-a"); // hog on core 2
    let _ = probes::probe(&ctx, "chaos-bc6-hog-b"); // hog on core 3
    let _ = probes::probe(&ctx, "chaos-bc6-monitor"); // witness on core 0
    let _ = probes::probe(&ctx, "chaos-bc7-victim"); // passive target on core 2
    let _ = probes::probe(&ctx, "chaos-bc7");        // controller on core 1

    // Property-test probes - Milestone 9 Phase 1.
    // prop-p9-victim must register its endpoint before prop-p9 is spawned
    // (SEND caps to prop-p9-victim are wired at prop-p9 spawn time).
    let _ = probes::probe(&ctx, "prop-p9-victim");
    let _ = probes::probe(&ctx, "prop-p9");
    let _ = probes::probe(&ctx, "prop-p1");
    let _ = probes::probe(&ctx, "prop-p10");
    // Property-test probes - Milestone 9 Phase 2.
    // P3 and P6 are spawned BEFORE the kill/respawn controllers (P2, P8) so they
    // are already running by the time P2 and P8 begin their kill/respawn loops.
    // P2 and P8 each do rapid kill/respawn cycles that compete for kernel resources;
    // spawning the self-contained probes first prevents CPU starvation of P3/P6.
    let _ = probes::probe(&ctx, "prop-p3");        // P3: self-referential cap bounce (no victims)
    let _ = probes::probe(&ctx, "prop-p6");        // P6: self-referential queue depth test (no victims)
    // Kill/respawn victims must be registered before their controller probes start.
    let _ = probes::probe(&ctx, "prop-p2-victim"); // P2: kill/respawn generation target
    let _ = probes::probe(&ctx, "prop-p2");        // P2 controller - starts cycling immediately
    let _ = probes::probe(&ctx, "prop-p8-victim"); // P8: kill/respawn generation target
    let _ = probes::probe(&ctx, "prop-p8");        // P8 controller - starts cycling immediately

    // Property-test probes - Milestone 9 Phase 3.
    // P4 has no victim. P5 and P7 victims must be registered before their
    // controllers so endpoints exist when the controllers start cycling.
    let _ = probes::probe(&ctx, "prop-p4");
    let _ = probes::probe(&ctx, "prop-p5-victim");
    let _ = probes::probe(&ctx, "prop-p5");
    let _ = probes::probe(&ctx, "prop-p7-victim");
    let _ = probes::probe(&ctx, "prop-p7");

    // --- Brutal property test probes - Milestone 16 ---
    // Victims before controllers within each pair.
    // Self-referential probes (BP3, BP6) can go in any order.
    let _ = probes::probe(&ctx, "prop-bp1");
    let _ = probes::probe(&ctx, "prop-bp2-victim");
    let _ = probes::probe(&ctx, "prop-bp2");
    let _ = probes::probe(&ctx, "prop-bp3");       // self-referential
    let _ = probes::probe(&ctx, "prop-bp4");
    let _ = probes::probe(&ctx, "prop-bp5-victim");
    let _ = probes::probe(&ctx, "prop-bp5");
    let _ = probes::probe(&ctx, "prop-bp6");       // self-referential
    let _ = probes::probe(&ctx, "prop-bp7-victim");
    let _ = probes::probe(&ctx, "prop-bp7");
    let _ = probes::probe(&ctx, "prop-bp8-victim");
    let _ = probes::probe(&ctx, "prop-bp8");
    let _ = probes::probe(&ctx, "prop-bp9-victim");
    let _ = probes::probe(&ctx, "prop-bp9");
    let _ = probes::probe(&ctx, "prop-bp10");

    // --- Fuzz-test probes - Milestone 10 Phase 1 ---
    // Recv-endpoint victims/targets must be spawned before their controllers.
    let _ = probes::probe(&ctx, "fuzz-f1");
    let _ = probes::probe(&ctx, "fuzz-f2");
    let _ = probes::probe(&ctx, "fuzz-f5-recv");
    let _ = probes::probe(&ctx, "fuzz-f5");
    let _ = probes::probe(&ctx, "fuzz-f6-recv");
    let _ = probes::probe(&ctx, "fuzz-f6");
    let _ = probes::probe(&ctx, "fuzz-f7-victim");
    let _ = probes::probe(&ctx, "fuzz-f7");
    let _ = probes::probe(&ctx, "fuzz-f8");

    // --- Brutal fuzz test probes - Milestone 17 ---
    // Recv-endpoint victims must be spawned before controllers so their
    // endpoints are registered when the controllers' SEND caps are wired.
    let _ = probes::probe(&ctx, "fuzz-bf5-recv");
    let _ = probes::probe(&ctx, "fuzz-bf5");
    let _ = probes::probe(&ctx, "fuzz-bf6-recv");
    let _ = probes::probe(&ctx, "fuzz-bf6");
    let _ = probes::probe(&ctx, "fuzz-bf7-victim");
    let _ = probes::probe(&ctx, "fuzz-bf7");
    let _ = probes::probe(&ctx, "fuzz-bf1");
    let _ = probes::probe(&ctx, "fuzz-bf2");
    let _ = probes::probe(&ctx, "fuzz-bf8");

    // --- Stress-test probes - Milestone 11 Phase 1 ---
    // Recv-endpoint victims must be spawned before their controllers so their
    // endpoints are registered before the controllers' SEND caps are wired.
    let _ = probes::probe(&ctx, "stress-s1-recv");
    let _ = probes::probe(&ctx, "stress-s1");
    let _ = probes::probe(&ctx, "stress-s2-victim");
    let _ = probes::probe(&ctx, "stress-s2");
    let _ = probes::probe(&ctx, "stress-s3-recv");   // core 1 - cross-core thrash receiver
    let _ = probes::probe(&ctx, "stress-s3-send");   // core 0 - cross-core thrash sender
    let _ = probes::probe(&ctx, "stress-s4-victim");
    let _ = probes::probe(&ctx, "stress-s4");
    let _ = probes::probe(&ctx, "stress-s7");
    let _ = probes::probe(&ctx, "stress-s10-victim"); // core 1 - cascading revocation target
    let _ = probes::probe(&ctx, "stress-s10");        // core 0 - kills victim cross-core
    // Stress Phase 2 - S5, S6, S8, S9.
    // s5-victim must register before s5 starts cycling.
    // s9-recv must register before s9-send-a/b are wired with SEND caps.
    let _ = probes::probe(&ctx, "stress-s5-victim");
    let _ = probes::probe(&ctx, "stress-s5");
    let _ = probes::probe(&ctx, "stress-s6");        // self-referential; endpoint registered at spawn time
    let _ = probes::probe(&ctx, "stress-s8");
    let _ = probes::probe(&ctx, "stress-s9-recv");   // core 2 - concurrent IPI storm receiver
    let _ = probes::probe(&ctx, "stress-s9-send-a"); // core 0 → core 2
    let _ = probes::probe(&ctx, "stress-s9-send-b"); // core 1 → core 2

    // --- Brutal stress-test probes - Milestone 18 ---
    // Ordering: recv-endpoint victims before their controllers.
    let _ = probes::probe(&ctx, "stress-bs1-recv");   // passive saturation target
    let _ = probes::probe(&ctx, "stress-bs1");        // 50k try_send
    let _ = probes::probe(&ctx, "stress-bs2-victim"); // passive restart victim
    let _ = probes::probe(&ctx, "stress-bs2");        // 200 kill/respawn cycles
    let _ = probes::probe(&ctx, "stress-bs3-recv");   // core 1 - cross-core thrash receiver
    let _ = probes::probe(&ctx, "stress-bs3-send");   // core 0 - 2000 blocking sends
    let _ = probes::probe(&ctx, "stress-bs4-victim"); // passive churn victim
    let _ = probes::probe(&ctx, "stress-bs4");        // 50 churn cycles; 2 cap slots
    let _ = probes::probe(&ctx, "stress-bs5-victim"); // passive generation victim
    let _ = probes::probe(&ctx, "stress-bs5");        // 5000 kill/respawn; generation monotonic
    let _ = probes::probe(&ctx, "stress-bs6");        // self-referential; 20000 self-ping rounds
    let _ = probes::probe(&ctx, "stress-bs7");        // 500 alloc passes
    let _ = probes::probe(&ctx, "stress-bs8");        // 3000 yields
    let _ = probes::probe(&ctx, "stress-bs9-recv");   // core 2 - IPI storm receiver
    let _ = probes::probe(&ctx, "stress-bs9-send-a"); // core 0 → core 2; 2500 sends
    let _ = probes::probe(&ctx, "stress-bs9-send-b"); // core 1 → core 2; 2500 sends
    let _ = probes::probe(&ctx, "stress-bs10-victim"); // core 1 - cascading revocation victim
    let _ = probes::probe(&ctx, "stress-bs10");        // core 0; 50 cycles; 3 cap slots

    // --- Chaos-test probes - Milestone 14 ---
    // c7-victim must be registered on core 2 before chaos-c7 is spawned on core 1
    // so its endpoint exists when chaos-c7's SEND cap is wired at spawn time.
    let _ = probes::probe(&ctx, "chaos-c2");
    let _ = probes::probe(&ctx, "chaos-c2-monitor");
    let _ = probes::probe(&ctx, "chaos-c3");
    let _ = probes::probe(&ctx, "chaos-c5");
    let _ = probes::probe(&ctx, "chaos-c6-hog");
    let _ = probes::probe(&ctx, "chaos-c6-monitor");
    let _ = probes::probe(&ctx, "chaos-c7-victim"); // passive recv target - spawned before controller
    let _ = probes::probe(&ctx, "chaos-c7");

    // --- Adversarial-test probes - Milestone 13 ---
    // Passive/victim services must be spawned before their attackers so their
    // endpoints are registered when the attackers' SEND caps are wired.
    let _ = probes::probe(&ctx, "adv-a1");
    let _ = probes::probe(&ctx, "adv-a2");
    let _ = probes::probe(&ctx, "adv-a3");
    let _ = probes::probe(&ctx, "adv-a4");
    let _ = probes::probe(&ctx, "adv-a5-victim"); // passive - killed by adv-a5
    let _ = probes::probe(&ctx, "adv-a5");
    let _ = probes::probe(&ctx, "adv-a6");
    let _ = probes::probe(&ctx, "adv-a7-recv");   // passive - recv target before sender wired
    let _ = probes::probe(&ctx, "adv-a7");
    let _ = probes::probe(&ctx, "adv-a8");
    let _ = probes::probe(&ctx, "adv-a8-witness");
    let _ = probes::probe(&ctx, "adv-a9");
    let _ = probes::probe(&ctx, "adv-a10");
    let _ = probes::probe(&ctx, "adv-a11"); // introspection gated - denied without INTROSPECT cap
    let _ = probes::probe(&ctx, "adv-a12"); // reboot gated - denied without REBOOT cap
    let _ = probes::probe(&ctx, "adv-a13"); // AcquireSendCap gated - denied without ACQUIRE_ANY

    // --- Brutal performance-benchmark probes - Milestone 19 ---
    // Sender/controller BEFORE echo/recv so endpoints register first.
    // bp5-victim before bp5; bp9-recv before bp9.
    let _ = probes::probe(&ctx, "perf-bp1");         // BP1 sender (core 0) - registers endpoint first
    let _ = probes::probe(&ctx, "perf-bp1-echo");    // BP1 echo (core 0)
    let _ = probes::probe(&ctx, "perf-bp2");         // BP2 sender (core 0)
    let _ = probes::probe(&ctx, "perf-bp2-echo");    // BP2 echo (core 1)
    let _ = probes::probe(&ctx, "perf-bp3");
    let _ = probes::probe(&ctx, "perf-bp4");
    let _ = probes::probe(&ctx, "perf-bp5-victim");  // spawned before perf-bp5 so it exists to be killed
    let _ = probes::probe(&ctx, "perf-bp5");
    let _ = probes::probe(&ctx, "perf-bp7");
    let _ = probes::probe(&ctx, "perf-bp8");
    let _ = probes::probe(&ctx, "perf-bp9-recv");    // recv registered before sender is wired
    let _ = probes::probe(&ctx, "perf-bp9");
    let _ = probes::probe(&ctx, "perf-bp10");

    // --- Performance-benchmark probes - Milestone 12 ---
    // Spawn sender/controller probes BEFORE their echo/recv partners so the
    // sender's endpoint is registered when the echo partner wires its SEND cap.
    // perf-b5-victim must be registered before perf-b5 starts cycling.
    let _ = probes::probe(&ctx, "perf-b1");         // B1 sender (core 0) - registers endpoint first
    let _ = probes::probe(&ctx, "perf-b1-echo");    // B1 echo (core 0)   - wires SEND cap to perf-b1
    let _ = probes::probe(&ctx, "perf-b2");         // B2 sender (core 0) - registers endpoint first
    let _ = probes::probe(&ctx, "perf-b2-echo");    // B2 echo  (core 1)  - wires SEND cap to perf-b2
    let _ = probes::probe(&ctx, "perf-b3");
    let _ = probes::probe(&ctx, "perf-b4");
    let _ = probes::probe(&ctx, "perf-b5-victim");  // spawned before perf-b5 so it exists to be killed
    let _ = probes::probe(&ctx, "perf-b5");
    let _ = probes::probe(&ctx, "perf-b7");
    let _ = probes::probe(&ctx, "perf-b8");
    let _ = probes::probe(&ctx, "perf-b9-recv");    // recv partner registered before sender is wired
    let _ = probes::probe(&ctx, "perf-b9");
    let _ = probes::probe(&ctx, "perf-b10");

    // --- Brutal identity test probes - Milestone 15 ---
    // T12 chain: spawn C and B (recv-endpoint) before A (sender), so their
    // endpoints are registered when A's SEND cap to B is wired at spawn time.
    let _ = probes::probe(&ctx, "brutal-id-12-c"); // chain endpoint: registered first
    let _ = probes::probe(&ctx, "brutal-id-12-b"); // chain middle: registered before 12-a's SEND cap
    let _ = probes::probe(&ctx, "brutal-id-12-a"); // chain source: acquires cap to 12-c, sends to 12-b
    // T13 cross-core blocked send: recv must exist before sender's SEND cap is wired.
    // Kill runs independently on core 1 and yields before killing.
    let _ = probes::probe(&ctx, "brutal-id-13-recv"); // passive target on core 2
    let _ = probes::probe(&ctx, "brutal-id-13-kill"); // kills recv after brief delay on core 1
    let _ = probes::probe(&ctx, "brutal-id-13-send"); // fills queue then blocks on core 0
    // T11 self-referential queue: brutal-id-11 sends to itself; any spawn order.
    let _ = probes::probe(&ctx, "brutal-id-11");
}
