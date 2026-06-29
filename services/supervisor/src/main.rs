// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

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

// ───────────────────────────────────────────────────────────────────────────────
// Phase 1 of moving naming out of the kernel (docs/naming-design.md).
//
// As the supervisor spawns the real services it records, in a bounded no-heap map, the
// SEND|GRANT endpoint cap the kernel hands back from `spawn_returning_endpoint` (syscall 38,
// Phase 0a). This proves the supervisor can hold a cap to everything it starts - the future
// name authority. It is a SHADOW map for now: nothing reads it to wire dependents yet (that is
// Phase 0b/3). Scoped to the real services; the 178 test probes are test infra (out of scope)
// and keep using plain `ctx.spawn`.
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
                "supervisor: {} wired spawn FAILED (stale peer cap) - retrying name-wired", name));
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

/// Reconcile to desired state: respawn any managed restartable service that is NOT actually alive. A
/// death notification can be DROPPED under a storm - our death-notification endpoint is 16-deep, so a
/// burst of deaths (or chaos flooding us) overflows it and a dropped name is silently never restarted
/// (the "fs gone from observe after a storm" bug). `acquire_*_cap` cannot detect this (the kernel
/// directory keeps a dead service's name), so we scan REAL liveness via `task_stat`. Order matters:
/// block-driver before fs before shell (each wires to the previous). Returns how many it respawned.
fn reconcile(ctx: &ServiceContext, map: &mut NameCapMap) -> u32 {
    const MANAGED: [&str; 6] = ["block-driver", "fs", "shell", "xhci", "ehci", "logger"];
    let mut alive = [false; 6];
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if !st.valid || st.state == 4 { continue; } // 4 = Dead
        let nm = st.name_str();
        for i in 0..MANAGED.len() { if nm == MANAGED[i] { alive[i] = true; } }
    }
    let mut n = 0;
    for i in 0..MANAGED.len() {
        if alive[i] { continue; }
        let ok = match MANAGED[i] {
            "fs"    => spawn_wired(ctx, map, "fs", &["block-driver"]),
            "shell" => spawn_wired(ctx, map, "shell", &["fs"]),
            other   => spawn_mapped(ctx, map, other, 0xFFFF),
        };
        if ok {
            n += 1;
            ctx.log_fmt(format_args!("supervisor: reconcile respawned {} (missed death notification)", MANAGED[i]));
        }
    }
    n
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Naming migration (docs/naming-design.md): `name → cap` map, built as we spawn the real
    // services. The supervisor wires dependents from it; clients resolve/reacquire names via the
    // kernel name-directory (Path C, §3.7 - the registry *service* is retired, Phase 4).
    #[allow(unused_mut)]
    let mut name_map = NameCapMap::new();

    // Path C / Phase 5: the kernel boots the supervisor directly (init is removed), so the
    // supervisor now spawns the logger - moved here from init. logger is not TCB (§11.3): retry
    // once on failure and continue without it (its output falls back to the kernel ring buffer).
    ctx.log("supervisor: spawning logger...");
    if ctx.spawn("logger").is_err() {
        ctx.log("supervisor: logger spawn failed, retrying");
        let _ = ctx.spawn("logger");
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
        let _ = ctx.spawn("probe-recv");    // Test 3A receiver
        let _ = ctx.spawn("probe-victim");  // Test 4A kill target
        let _ = ctx.spawn("probe-4b-recv"); // Test 4B kill target
        let _ = ctx.spawn("probe-3b");      // Test 3B (has recv slot for wrong-right probe)
        // Sender / active probes - need SEND caps to the services above.
        let _ = ctx.spawn("probe-sender");  // Test 3A sender; SEND cap to probe-recv
        let _ = ctx.spawn("probe-4a");      // Test 4A; SEND cap to probe-victim
        let _ = ctx.spawn("probe-4b-send"); // Test 4B; SEND cap to probe-4b-recv
        // Cap-transfer probes (Tests 5A and 5B) - receiver first so its endpoint
        // is registered before the senders' SEND|GRANT caps are wired.
        let _ = ctx.spawn("probe-5a-recv"); // Test 5A/5B receiver
        let _ = ctx.spawn("probe-5a-send"); // Test 5A sender (SEND|GRANT cap)
        let _ = ctx.spawn("probe-5b-send"); // Test 5B negative (SEND-only cap)
        // Probes with no send peers.
        let _ = ctx.spawn("probe-yielder"); // Test 8A
        let _ = ctx.spawn("probe-hog");     // Test 8B (tight loop; preemption via ping)
        let _ = ctx.spawn("probe-9b");      // Test 9B
        // Memory-limit probes - Tests 7A and 7B.
        let _ = ctx.spawn("probe-7a");
        let _ = ctx.spawn("probe-7b");
        // Interrupt-routing probe - Test IR1A (§12.2, §12.3).
        let _ = ctx.spawn("probe-11a");
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
    // Phase 4 (Path C): the registry service is gone. block-driver has no peers; fs's only peer is
    // block-driver, provided from the map. Clients reacquire names via the kernel directory.
    //
    // block-driver is also spawned in `identity-only` builds - it idles harmlessly with no disk
    // (QEMU has no -drive there: "no controller"), giving §22 Test 11 a restartable victim to kill.
    // `ensure_*` (Phase 6): spawn on a fresh boot, ADOPT the running instance on a supervisor respawn.
    #[cfg(any(feature = "bare-metal", feature = "blockdev", feature = "identity-only"))]
    ensure_mapped(&ctx, &mut name_map, "block-driver", 0xFFFF);
    // fs needs a disk → bare-metal / blockdev only.
    #[cfg(any(feature = "bare-metal", feature = "blockdev"))]
    ensure_wired(&ctx, &mut name_map, "fs", &["block-driver"]);

    // shell: the interactive prompt. Spawned in bare-metal (the USB image rests
    // here) and full builds; excluded from test-specific builds.
    #[cfg(not(any(feature = "identity-only", feature = "perf-only",
                  feature = "perf-brutal-only", feature = "stress-only",
                  feature = "adv-only", feature = "chaos-only", feature = "fuzz-only",
                  feature = "b2-only", feature = "bp2-only", feature = "perf-iso")))]
    // Phase 3a: shell's `fs` peer is wired from the supervisor's map (no registry - retired).
    // Phase 6: ensure_wired adopts a running shell on a supervisor respawn instead of duplicating it.
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

    // xhci: USB host-controller driver (§12). Spawned in bare-metal + full
    // builds; the kernel maps its controller's MMIO BAR at spawn (Stage 2).
    #[cfg(not(any(feature = "identity-only", feature = "perf-only",
                  feature = "perf-brutal-only", feature = "stress-only",
                  feature = "adv-only", feature = "chaos-only", feature = "fuzz-only",
                  feature = "b2-only", feature = "bp2-only", feature = "perf-iso")))]
    spawn_mapped(&ctx, &mut name_map, "xhci", 0xFFFF);

    // ehci: USB 2.0 host-controller driver (§12) for the back ports. Same builds
    // as xhci; the kernel grants its MMIO/DMA at spawn (E1b+).
    #[cfg(not(any(feature = "identity-only", feature = "perf-only",
                  feature = "perf-brutal-only", feature = "stress-only",
                  feature = "adv-only", feature = "chaos-only", feature = "fuzz-only",
                  feature = "b2-only", feature = "bp2-only", feature = "perf-iso")))]
    spawn_mapped(&ctx, &mut name_map, "ehci", 0xFFFF);

    // Phase 1 (docs/naming-design.md): report the shadow name→cap map. Proves the supervisor now
    // holds an endpoint cap to every real service it spawned - the future name authority. Nothing
    // reads it yet (Phase 0b/3 wire dependents from it; Phase 4 brokers reacquisition through it).
    ctx.log_fmt(format_args!("supervisor: name-cap map holds {} service(s)", name_map.count));

    ctx.log("supervisor: ready");

    // Death-notification restart loop (H11 ph6; extended for fs + block-driver in Phase D).
    // The kernel enqueues the name of a dead restartable service to our endpoint; we respawn
    // it. `recv` BLOCKS, so the core still reaches the idle/halt path and runs cool between
    // deaths (no polling). Restartable services routed here: `block-driver`, `fs`, `shell`, `xhci`,
    // `ehci`, `logger`. The supervisor itself is restartable too (Phase 6) but by the KERNEL - a dead
    // task can't respawn itself; the only death that is unrecoverable is the kernel's. (`registry`
    // retired, Phase 4; `init` removed, Phase 5.) Other restart/kill commands still arrive via the
    // COM2 control channel (control::process_pending in the timer ISR).
    //
    // If this build gave us no endpoint (minimal test manifests), fall back to park.
    if ctx.recv_handle().is_none() {
        ctx.park();
    }
    loop {
        let msg = ctx.recv();
        let name = core::str::from_utf8(msg.payload_bytes()).unwrap_or("");
        // Restartable services (§6.1): fs + block-driver (Phase D). Phase 3c/4 (docs/naming-design.md):
        // respawn WIRED FROM THE MAP - same peers as at boot - and the spawn refreshes the map with
        // the new instance's cap (record updates in place, so a kill-storm can't grow the map). The
        // restarted service is supervisor-wired just like at boot; clients reacquire it by name via
        // the kernel directory (§14.3). The "died/restarted" log lines are kept (tests gate on them).
        match name {
            "block-driver" => {
                ctx.log("supervisor: block-driver died, restarting");
                if spawn_mapped(&ctx, &mut name_map, "block-driver", 0xFFFF) { ctx.log("supervisor: block-driver restarted"); }
                else { ctx.log("supervisor: block-driver restart FAILED"); }
            }
            "fs" => {
                ctx.log("supervisor: fs died, restarting");
                if spawn_wired(&ctx, &mut name_map, "fs", &["block-driver"]) { ctx.log("supervisor: fs restarted"); }
                else { ctx.log("supervisor: fs restart FAILED"); }
            }
            "shell" => {
                // The user's interface is restartable too ("nothing escapes"): a crash or a
                // deliberate `kill shell` respawns a FRESH prompt. spawn_wired spawns a new instance
                // (the singleton guard only blocks a LIVE duplicate), re-granting its console-read +
                // service_control caps and wiring its `fs` peer from the map. The in-flight command
                // is lost (state is not resumed, §14.2/§25) but the session recovers.
                ctx.log("supervisor: shell died, restarting");
                if spawn_wired(&ctx, &mut name_map, "shell", &["fs"]) { ctx.log("supervisor: shell restarted"); }
                else { ctx.log("supervisor: shell restart FAILED"); }
            }
            // The USB host drivers + logger are directly restartable now: their OWN death respawns
            // them immediately (re-granting MMIO/DMA/IRQ caps + re-initialising the controller),
            // instead of waiting for a lucky supervisor respawn. This is what keeps a `chaos
            // max-carnage` that kills `xhci`/`ehci` in its last rounds from leaving the keyboard dead.
            "xhci" => {
                ctx.log("supervisor: xhci died, restarting");
                if spawn_mapped(&ctx, &mut name_map, "xhci", 0xFFFF) { ctx.log("supervisor: xhci restarted"); }
                else { ctx.log("supervisor: xhci restart FAILED"); }
            }
            "ehci" => {
                ctx.log("supervisor: ehci died, restarting");
                if spawn_mapped(&ctx, &mut name_map, "ehci", 0xFFFF) { ctx.log("supervisor: ehci restarted"); }
                else { ctx.log("supervisor: ehci restart FAILED"); }
            }
            "logger" => {
                ctx.log("supervisor: logger died, restarting");
                if spawn_mapped(&ctx, &mut name_map, "logger", 0xFFFF) { ctx.log("supervisor: logger restarted"); }
                else { ctx.log("supervisor: logger restart FAILED"); }
            }
            // counter (examples/counter, counter-test build): respawn it wired to `fs` - the fresh
            // instance reconstructs its count from /counter.dat (§14/§15). The "died/restarted" lines
            // are what `osdev test counter` gates on. (Only ever sent when counter is actually live.)
            "counter" => {
                ctx.log("supervisor: counter died, restarting");
                if spawn_wired(&ctx, &mut name_map, "counter", &["fs"]) { ctx.log("supervisor: counter restarted"); }
                else { ctx.log("supervisor: counter restart FAILED"); }
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
    let _ = ctx.spawn("perf-b1");
    let _ = ctx.spawn("perf-b1-echo");
    let _ = ctx.spawn("perf-b2");
    let _ = ctx.spawn("perf-b2-echo");
    let _ = ctx.spawn("perf-b3");
    let _ = ctx.spawn("perf-b4");
    let _ = ctx.spawn("perf-b5-victim");
    let _ = ctx.spawn("perf-b5");
    let _ = ctx.spawn("perf-b7");
    let _ = ctx.spawn("perf-b8");
    let _ = ctx.spawn("perf-b9-recv");
    let _ = ctx.spawn("perf-b9");
    let _ = ctx.spawn("perf-b10");
}

// perf-brutal-only: spawn only the brutal performance benchmark probe services.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), feature = "perf-brutal-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    let _ = ctx.spawn("perf-bp1");
    let _ = ctx.spawn("perf-bp1-echo");
    let _ = ctx.spawn("perf-bp2");
    let _ = ctx.spawn("perf-bp2-echo");
    let _ = ctx.spawn("perf-bp3");
    let _ = ctx.spawn("perf-bp4");
    let _ = ctx.spawn("perf-bp5-victim");
    let _ = ctx.spawn("perf-bp5");
    let _ = ctx.spawn("perf-bp7");
    let _ = ctx.spawn("perf-bp8");
    let _ = ctx.spawn("perf-bp9-recv");
    let _ = ctx.spawn("perf-bp9");
    let _ = ctx.spawn("perf-bp10");
}

// stress-only: spawn only the S1-S10 stress probe services.
// All stress probes are self-contained (use ctx.kill/ctx.spawn internally);
// no QEMU control port required - safe for real hardware.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), feature = "stress-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // Receivers/victims must register before their controllers so endpoints
    // exist when sender SEND caps are wired at spawn time.
    let _ = ctx.spawn("stress-s1-recv");
    let _ = ctx.spawn("stress-s1");
    let _ = ctx.spawn("stress-s2-victim");
    let _ = ctx.spawn("stress-s2");
    let _ = ctx.spawn("stress-s3-recv");    // core 1 - cross-core thrash receiver
    let _ = ctx.spawn("stress-s3-send");    // core 0 - cross-core thrash sender
    let _ = ctx.spawn("stress-s4-victim");
    let _ = ctx.spawn("stress-s4");
    let _ = ctx.spawn("stress-s5-victim");
    let _ = ctx.spawn("stress-s5");
    let _ = ctx.spawn("stress-s6");         // self-referential; endpoint registered at spawn
    let _ = ctx.spawn("stress-s7");
    let _ = ctx.spawn("stress-s8");
    let _ = ctx.spawn("stress-s9-recv");    // core 2 - IPI storm receiver
    let _ = ctx.spawn("stress-s9-send-a"); // core 0 → core 2
    let _ = ctx.spawn("stress-s9-send-b"); // core 1 → core 2
    let _ = ctx.spawn("stress-s10-victim"); // core 1 - cascading revocation target
    let _ = ctx.spawn("stress-s10");        // core 0 - kills victim cross-core
}

// chaos-only: spawn only the C2-C7 chaos probe services.
// C1 (degraded SMP boot) and C4 (minimal RAM) use bare-metal + hardware
// reconfiguration instead of probes.  All probes here are self-contained.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), not(feature = "adv-only"), feature = "chaos-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // BC7/C7 victims must be registered before their controllers so endpoints
    // exist when the controller's SEND caps are wired at spawn time.
    let _ = ctx.spawn("chaos-c2");          // non-TCB page fault, system continues
    let _ = ctx.spawn("chaos-c2-monitor");  // witness - alive after c2 faults
    let _ = ctx.spawn("chaos-c3");          // alloc-deny pressure cycles
    let _ = ctx.spawn("chaos-c5");          // recursive yields (kernel stack depth)
    let _ = ctx.spawn("chaos-c6-hog");      // tight-loop hog on core 3
    let _ = ctx.spawn("chaos-c6-monitor");  // witness on core 0
    let _ = ctx.spawn("chaos-c7-victim");   // passive recv target on core 2
    let _ = ctx.spawn("chaos-c7");          // TLB shootdown controller on core 1
}

// adv-only: spawn only the A1-A10 adversarial probe services.
// All adversarial probes are self-contained - no QEMU control port required.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), feature = "adv-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // adv-a11 first: it is self-contained (no peers, no IPC) and logs its pass
    // line within the first second, so it completes even when the CPU-heavy
    // attackers (A1's 10k-iteration loop, A2 brute-force) would otherwise starve
    // a TCG-throttled boot. Order is functionally irrelevant for it.
    let _ = ctx.spawn("adv-a11"); // introspection gated - denied without INTROSPECT cap
    let _ = ctx.spawn("adv-a12"); // reboot gated - denied without REBOOT cap (self-contained)
    let _ = ctx.spawn("adv-a13"); // AcquireSendCap gated - denied without ACQUIRE_ANY (self-contained)
    // Passive/victim services before their attackers so endpoints exist when
    // attacker SEND caps are wired at spawn time.
    let _ = ctx.spawn("adv-a1");
    let _ = ctx.spawn("adv-a2");
    let _ = ctx.spawn("adv-a3");
    let _ = ctx.spawn("adv-a4");
    let _ = ctx.spawn("adv-a5-victim"); // passive - killed by adv-a5
    let _ = ctx.spawn("adv-a5");
    let _ = ctx.spawn("adv-a6");
    let _ = ctx.spawn("adv-a7-recv");   // passive recv - registered before sender
    let _ = ctx.spawn("adv-a7");
    let _ = ctx.spawn("adv-a8");        // tight-loop attacker
    let _ = ctx.spawn("adv-a8-witness");
    let _ = ctx.spawn("adv-a9");
    let _ = ctx.spawn("adv-a10");
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
    let _ = ctx.spawn("fuzz-f1");
    let _ = ctx.spawn("fuzz-f2");
    let _ = ctx.spawn("fuzz-f5-recv");
    let _ = ctx.spawn("fuzz-f5");
    let _ = ctx.spawn("fuzz-f6-recv");
    let _ = ctx.spawn("fuzz-f6");
    let _ = ctx.spawn("fuzz-f7-victim");
    let _ = ctx.spawn("fuzz-f7");
    let _ = ctx.spawn("fuzz-f8");
    // Brutal fuzz probes (Milestone 17) - heavier iteration counts; run fast on
    // real silicon (no TCG throttling). Recv/victim partners first.
    let _ = ctx.spawn("fuzz-bf5-recv");
    let _ = ctx.spawn("fuzz-bf5");
    let _ = ctx.spawn("fuzz-bf6-recv");
    let _ = ctx.spawn("fuzz-bf6");
    let _ = ctx.spawn("fuzz-bf7-victim");
    let _ = ctx.spawn("fuzz-bf7");
    let _ = ctx.spawn("fuzz-bf1");
    let _ = ctx.spawn("fuzz-bf2");
    let _ = ctx.spawn("fuzz-bf8");
}

// b2-only: spawn only the regular B2 cross-core IPC probe pair (isolation build).
// No other benchmarks running - eliminates concurrent IPI noise from B5 spawn/kill
// and B6 restart cycles so the blocking round-trip can complete on Goldmont+.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), not(feature = "adv-only"), not(feature = "chaos-only"), feature = "b2-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    let _ = ctx.spawn("perf-b2");      // B2 sender (core 0) - registers endpoint first
    let _ = ctx.spawn("perf-b2-echo"); // B2 echo  (core 1) - wires SEND cap to perf-b2
}

// bp2-only: spawn only the brutal BP2 cross-core IPC probe pair (isolation build).
// Brutal equivalent of b2-only - higher iteration count, same isolation rationale.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), not(feature = "adv-only"), not(feature = "chaos-only"), not(feature = "b2-only"), feature = "bp2-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    let _ = ctx.spawn("perf-bp2");      // BP2 sender (core 0) - registers endpoint first
    let _ = ctx.spawn("perf-bp2-echo"); // BP2 echo  (core 1) - wires SEND cap to perf-bp2
}

// perf-iso: isolate ONE brutal perf probe (+ its partners) - no ping/pong, no
// other probes - for clean uncontended per-op latency on hardware. The probe is
// selected by an iso-bpN sub-feature (each pulls in perf-iso). bp5 covers both
// BP5 (spawn) and BP6 (restart) - same probe. Partners are spawned first
// (victim before perf-bp5; recv before perf-bp9) so endpoints/caps are wired.
#[cfg(feature = "perf-iso")]
fn spawn_extended_probes(ctx: &ServiceContext) {
    #[cfg(feature = "iso-bp3")]  { let _ = ctx.spawn("perf-bp3"); }
    #[cfg(feature = "iso-bp5")]  { let _ = ctx.spawn("perf-bp5-victim"); let _ = ctx.spawn("perf-bp5"); }
    #[cfg(feature = "iso-bp7")]  { let _ = ctx.spawn("perf-bp7"); }
    #[cfg(feature = "iso-bp9")]  { let _ = ctx.spawn("perf-bp9-recv"); let _ = ctx.spawn("perf-bp9"); }
    #[cfg(feature = "iso-bp10")] { let _ = ctx.spawn("perf-bp10"); }
    // Cross-core STRESS isolation (recv/partners first so endpoints are registered).
    #[cfg(feature = "iso-s3")]   { let _ = ctx.spawn("stress-s3-recv"); let _ = ctx.spawn("stress-s3-send"); }
    // iso-s5: victim first so its endpoint exists when stress-s5's caps are wired.
    #[cfg(feature = "iso-s5")]   { let _ = ctx.spawn("stress-s5-victim"); let _ = ctx.spawn("stress-s5"); }
    // iso-c7: victim (core 2) first so its endpoint exists when chaos-c7's (core 1)
    // SEND cap is wired; controller then drives 30 cross-core kill/respawn cycles.
    #[cfg(feature = "iso-c7")]   { let _ = ctx.spawn("chaos-c7-victim"); let _ = ctx.spawn("chaos-c7"); }
    // iso-xsend: receiver (core 2) first so its endpoint exists when xsend's (core 1)
    // SEND cap is wired; sender then times bare cross-core try_sends to a LIVE receiver.
    #[cfg(feature = "iso-xsend")] { let _ = ctx.spawn("xsend-recv"); let _ = ctx.spawn("xsend"); }
    // iso-xlife: both victims first so they exist when the controller's first kill
    // fires; controller (core 1) then times kill/spawn of near (core 1) and far (core 2).
    #[cfg(feature = "iso-xlife")] { let _ = ctx.spawn("xlife-near"); let _ = ctx.spawn("xlife-far"); let _ = ctx.spawn("xlife"); }
    // (iso-reg reg-roundtrip self-test removed - registry service retired, Path C / Phase 4.)
    #[cfg(feature = "iso-s9")]   {
        let _ = ctx.spawn("stress-s9-recv");
        let _ = ctx.spawn("stress-s9-send-a");
        let _ = ctx.spawn("stress-s9-send-b");
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
    let _ = ctx.spawn("adv-ba1");
    let _ = ctx.spawn("adv-ba2");
    let _ = ctx.spawn("adv-ba3");
    let _ = ctx.spawn("adv-ba4");
    let _ = ctx.spawn("adv-ba5-victim"); // registered before adv-ba5
    let _ = ctx.spawn("adv-ba5");
    let _ = ctx.spawn("adv-ba6");        // recv endpoint registered so self-fill works
    let _ = ctx.spawn("adv-ba7-recv");   // passive recv registered before sender
    let _ = ctx.spawn("adv-ba7");
    let _ = ctx.spawn("adv-ba8");        // tight-loop hog
    let _ = ctx.spawn("adv-ba8-witness");
    let _ = ctx.spawn("adv-ba9");
    let _ = ctx.spawn("adv-ba10");

    // --- Brutal chaos-test probes - Milestone 21 ---
    // Spawned EARLY before property/stress kill-respawn loops start.
    // BC2: 5 simultaneous faulters registered before the monitor so all 5
    // fault concurrently before the monitor starts counting yields.
    // BC7: victim registered before controller so its endpoint exists when
    // the controller's SEND cap is wired at spawn time.
    let _ = ctx.spawn("chaos-bc2-a");
    let _ = ctx.spawn("chaos-bc2-b");
    let _ = ctx.spawn("chaos-bc2-c");
    let _ = ctx.spawn("chaos-bc2-d");
    let _ = ctx.spawn("chaos-bc2-e");
    let _ = ctx.spawn("chaos-bc2-monitor");
    let _ = ctx.spawn("chaos-bc3");
    let _ = ctx.spawn("chaos-bc5");
    let _ = ctx.spawn("chaos-bc6-hog-a"); // hog on core 2
    let _ = ctx.spawn("chaos-bc6-hog-b"); // hog on core 3
    let _ = ctx.spawn("chaos-bc6-monitor"); // witness on core 0
    let _ = ctx.spawn("chaos-bc7-victim"); // passive target on core 2
    let _ = ctx.spawn("chaos-bc7");        // controller on core 1

    // Property-test probes - Milestone 9 Phase 1.
    // prop-p9-victim must register its endpoint before prop-p9 is spawned
    // (SEND caps to prop-p9-victim are wired at prop-p9 spawn time).
    let _ = ctx.spawn("prop-p9-victim");
    let _ = ctx.spawn("prop-p9");
    let _ = ctx.spawn("prop-p1");
    let _ = ctx.spawn("prop-p10");
    // Property-test probes - Milestone 9 Phase 2.
    // P3 and P6 are spawned BEFORE the kill/respawn controllers (P2, P8) so they
    // are already running by the time P2 and P8 begin their kill/respawn loops.
    // P2 and P8 each do rapid kill/respawn cycles that compete for kernel resources;
    // spawning the self-contained probes first prevents CPU starvation of P3/P6.
    let _ = ctx.spawn("prop-p3");        // P3: self-referential cap bounce (no victims)
    let _ = ctx.spawn("prop-p6");        // P6: self-referential queue depth test (no victims)
    // Kill/respawn victims must be registered before their controller probes start.
    let _ = ctx.spawn("prop-p2-victim"); // P2: kill/respawn generation target
    let _ = ctx.spawn("prop-p2");        // P2 controller - starts cycling immediately
    let _ = ctx.spawn("prop-p8-victim"); // P8: kill/respawn generation target
    let _ = ctx.spawn("prop-p8");        // P8 controller - starts cycling immediately

    // Property-test probes - Milestone 9 Phase 3.
    // P4 has no victim. P5 and P7 victims must be registered before their
    // controllers so endpoints exist when the controllers start cycling.
    let _ = ctx.spawn("prop-p4");
    let _ = ctx.spawn("prop-p5-victim");
    let _ = ctx.spawn("prop-p5");
    let _ = ctx.spawn("prop-p7-victim");
    let _ = ctx.spawn("prop-p7");

    // --- Brutal property test probes - Milestone 16 ---
    // Victims before controllers within each pair.
    // Self-referential probes (BP3, BP6) can go in any order.
    let _ = ctx.spawn("prop-bp1");
    let _ = ctx.spawn("prop-bp2-victim");
    let _ = ctx.spawn("prop-bp2");
    let _ = ctx.spawn("prop-bp3");       // self-referential
    let _ = ctx.spawn("prop-bp4");
    let _ = ctx.spawn("prop-bp5-victim");
    let _ = ctx.spawn("prop-bp5");
    let _ = ctx.spawn("prop-bp6");       // self-referential
    let _ = ctx.spawn("prop-bp7-victim");
    let _ = ctx.spawn("prop-bp7");
    let _ = ctx.spawn("prop-bp8-victim");
    let _ = ctx.spawn("prop-bp8");
    let _ = ctx.spawn("prop-bp9-victim");
    let _ = ctx.spawn("prop-bp9");
    let _ = ctx.spawn("prop-bp10");

    // --- Fuzz-test probes - Milestone 10 Phase 1 ---
    // Recv-endpoint victims/targets must be spawned before their controllers.
    let _ = ctx.spawn("fuzz-f1");
    let _ = ctx.spawn("fuzz-f2");
    let _ = ctx.spawn("fuzz-f5-recv");
    let _ = ctx.spawn("fuzz-f5");
    let _ = ctx.spawn("fuzz-f6-recv");
    let _ = ctx.spawn("fuzz-f6");
    let _ = ctx.spawn("fuzz-f7-victim");
    let _ = ctx.spawn("fuzz-f7");
    let _ = ctx.spawn("fuzz-f8");

    // --- Brutal fuzz test probes - Milestone 17 ---
    // Recv-endpoint victims must be spawned before controllers so their
    // endpoints are registered when the controllers' SEND caps are wired.
    let _ = ctx.spawn("fuzz-bf5-recv");
    let _ = ctx.spawn("fuzz-bf5");
    let _ = ctx.spawn("fuzz-bf6-recv");
    let _ = ctx.spawn("fuzz-bf6");
    let _ = ctx.spawn("fuzz-bf7-victim");
    let _ = ctx.spawn("fuzz-bf7");
    let _ = ctx.spawn("fuzz-bf1");
    let _ = ctx.spawn("fuzz-bf2");
    let _ = ctx.spawn("fuzz-bf8");

    // --- Stress-test probes - Milestone 11 Phase 1 ---
    // Recv-endpoint victims must be spawned before their controllers so their
    // endpoints are registered before the controllers' SEND caps are wired.
    let _ = ctx.spawn("stress-s1-recv");
    let _ = ctx.spawn("stress-s1");
    let _ = ctx.spawn("stress-s2-victim");
    let _ = ctx.spawn("stress-s2");
    let _ = ctx.spawn("stress-s3-recv");   // core 1 - cross-core thrash receiver
    let _ = ctx.spawn("stress-s3-send");   // core 0 - cross-core thrash sender
    let _ = ctx.spawn("stress-s4-victim");
    let _ = ctx.spawn("stress-s4");
    let _ = ctx.spawn("stress-s7");
    let _ = ctx.spawn("stress-s10-victim"); // core 1 - cascading revocation target
    let _ = ctx.spawn("stress-s10");        // core 0 - kills victim cross-core
    // Stress Phase 2 - S5, S6, S8, S9.
    // s5-victim must register before s5 starts cycling.
    // s9-recv must register before s9-send-a/b are wired with SEND caps.
    let _ = ctx.spawn("stress-s5-victim");
    let _ = ctx.spawn("stress-s5");
    let _ = ctx.spawn("stress-s6");        // self-referential; endpoint registered at spawn time
    let _ = ctx.spawn("stress-s8");
    let _ = ctx.spawn("stress-s9-recv");   // core 2 - concurrent IPI storm receiver
    let _ = ctx.spawn("stress-s9-send-a"); // core 0 → core 2
    let _ = ctx.spawn("stress-s9-send-b"); // core 1 → core 2

    // --- Brutal stress-test probes - Milestone 18 ---
    // Ordering: recv-endpoint victims before their controllers.
    let _ = ctx.spawn("stress-bs1-recv");   // passive saturation target
    let _ = ctx.spawn("stress-bs1");        // 50k try_send
    let _ = ctx.spawn("stress-bs2-victim"); // passive restart victim
    let _ = ctx.spawn("stress-bs2");        // 200 kill/respawn cycles
    let _ = ctx.spawn("stress-bs3-recv");   // core 1 - cross-core thrash receiver
    let _ = ctx.spawn("stress-bs3-send");   // core 0 - 2000 blocking sends
    let _ = ctx.spawn("stress-bs4-victim"); // passive churn victim
    let _ = ctx.spawn("stress-bs4");        // 50 churn cycles; 2 cap slots
    let _ = ctx.spawn("stress-bs5-victim"); // passive generation victim
    let _ = ctx.spawn("stress-bs5");        // 5000 kill/respawn; generation monotonic
    let _ = ctx.spawn("stress-bs6");        // self-referential; 20000 self-ping rounds
    let _ = ctx.spawn("stress-bs7");        // 500 alloc passes
    let _ = ctx.spawn("stress-bs8");        // 3000 yields
    let _ = ctx.spawn("stress-bs9-recv");   // core 2 - IPI storm receiver
    let _ = ctx.spawn("stress-bs9-send-a"); // core 0 → core 2; 2500 sends
    let _ = ctx.spawn("stress-bs9-send-b"); // core 1 → core 2; 2500 sends
    let _ = ctx.spawn("stress-bs10-victim"); // core 1 - cascading revocation victim
    let _ = ctx.spawn("stress-bs10");        // core 0; 50 cycles; 3 cap slots

    // --- Chaos-test probes - Milestone 14 ---
    // c7-victim must be registered on core 2 before chaos-c7 is spawned on core 1
    // so its endpoint exists when chaos-c7's SEND cap is wired at spawn time.
    let _ = ctx.spawn("chaos-c2");
    let _ = ctx.spawn("chaos-c2-monitor");
    let _ = ctx.spawn("chaos-c3");
    let _ = ctx.spawn("chaos-c5");
    let _ = ctx.spawn("chaos-c6-hog");
    let _ = ctx.spawn("chaos-c6-monitor");
    let _ = ctx.spawn("chaos-c7-victim"); // passive recv target - spawned before controller
    let _ = ctx.spawn("chaos-c7");

    // --- Adversarial-test probes - Milestone 13 ---
    // Passive/victim services must be spawned before their attackers so their
    // endpoints are registered when the attackers' SEND caps are wired.
    let _ = ctx.spawn("adv-a1");
    let _ = ctx.spawn("adv-a2");
    let _ = ctx.spawn("adv-a3");
    let _ = ctx.spawn("adv-a4");
    let _ = ctx.spawn("adv-a5-victim"); // passive - killed by adv-a5
    let _ = ctx.spawn("adv-a5");
    let _ = ctx.spawn("adv-a6");
    let _ = ctx.spawn("adv-a7-recv");   // passive - recv target before sender wired
    let _ = ctx.spawn("adv-a7");
    let _ = ctx.spawn("adv-a8");
    let _ = ctx.spawn("adv-a8-witness");
    let _ = ctx.spawn("adv-a9");
    let _ = ctx.spawn("adv-a10");
    let _ = ctx.spawn("adv-a11"); // introspection gated - denied without INTROSPECT cap
    let _ = ctx.spawn("adv-a12"); // reboot gated - denied without REBOOT cap
    let _ = ctx.spawn("adv-a13"); // AcquireSendCap gated - denied without ACQUIRE_ANY

    // --- Brutal performance-benchmark probes - Milestone 19 ---
    // Sender/controller BEFORE echo/recv so endpoints register first.
    // bp5-victim before bp5; bp9-recv before bp9.
    let _ = ctx.spawn("perf-bp1");         // BP1 sender (core 0) - registers endpoint first
    let _ = ctx.spawn("perf-bp1-echo");    // BP1 echo (core 0)
    let _ = ctx.spawn("perf-bp2");         // BP2 sender (core 0)
    let _ = ctx.spawn("perf-bp2-echo");    // BP2 echo (core 1)
    let _ = ctx.spawn("perf-bp3");
    let _ = ctx.spawn("perf-bp4");
    let _ = ctx.spawn("perf-bp5-victim");  // spawned before perf-bp5 so it exists to be killed
    let _ = ctx.spawn("perf-bp5");
    let _ = ctx.spawn("perf-bp7");
    let _ = ctx.spawn("perf-bp8");
    let _ = ctx.spawn("perf-bp9-recv");    // recv registered before sender is wired
    let _ = ctx.spawn("perf-bp9");
    let _ = ctx.spawn("perf-bp10");

    // --- Performance-benchmark probes - Milestone 12 ---
    // Spawn sender/controller probes BEFORE their echo/recv partners so the
    // sender's endpoint is registered when the echo partner wires its SEND cap.
    // perf-b5-victim must be registered before perf-b5 starts cycling.
    let _ = ctx.spawn("perf-b1");         // B1 sender (core 0) - registers endpoint first
    let _ = ctx.spawn("perf-b1-echo");    // B1 echo (core 0)   - wires SEND cap to perf-b1
    let _ = ctx.spawn("perf-b2");         // B2 sender (core 0) - registers endpoint first
    let _ = ctx.spawn("perf-b2-echo");    // B2 echo  (core 1)  - wires SEND cap to perf-b2
    let _ = ctx.spawn("perf-b3");
    let _ = ctx.spawn("perf-b4");
    let _ = ctx.spawn("perf-b5-victim");  // spawned before perf-b5 so it exists to be killed
    let _ = ctx.spawn("perf-b5");
    let _ = ctx.spawn("perf-b7");
    let _ = ctx.spawn("perf-b8");
    let _ = ctx.spawn("perf-b9-recv");    // recv partner registered before sender is wired
    let _ = ctx.spawn("perf-b9");
    let _ = ctx.spawn("perf-b10");

    // --- Brutal identity test probes - Milestone 15 ---
    // T12 chain: spawn C and B (recv-endpoint) before A (sender), so their
    // endpoints are registered when A's SEND cap to B is wired at spawn time.
    let _ = ctx.spawn("brutal-id-12-c"); // chain endpoint: registered first
    let _ = ctx.spawn("brutal-id-12-b"); // chain middle: registered before 12-a's SEND cap
    let _ = ctx.spawn("brutal-id-12-a"); // chain source: acquires cap to 12-c, sends to 12-b
    // T13 cross-core blocked send: recv must exist before sender's SEND cap is wired.
    // Kill runs independently on core 1 and yields before killing.
    let _ = ctx.spawn("brutal-id-13-recv"); // passive target on core 2
    let _ = ctx.spawn("brutal-id-13-kill"); // kills recv after brief delay on core 1
    let _ = ctx.spawn("brutal-id-13-send"); // fills queue then blocks on core 0
    // T11 self-referential queue: brutal-id-11 sends to itself; any spawn order.
    let _ = ctx.spawn("brutal-id-11");
}
