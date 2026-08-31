//! The test-probe parameter table - 193 probes, ONE program.
//!
//! This table used to live in `kernel/src/task/mod.rs`, as 193 `service_config` rows: the same
//! `probe` ELF, differing by a test-mode number. A parameter is policy, and policy belongs to a
//! service (§26.10) - so the kernel now holds ONE `probe` entry (the image and the defaults) and the
//! supervisor, which already decides which probes to run and when, supplies the rest at spawn.
//!
//! What is deliberately NOT here: `probe-11a`'s IRQ-33 route and `probe-5a-send`'s grantable peer
//! caps. Those are AUTHORITY, not settings, and the kernel keeps them keyed by name
//! (`task::probe_authority`). A caller may say what a probe IS; it may not assert what it may DO.
//!
//! Bounded and flat (§26.6): a `const` slice in rodata, no heap, no lookup structure. A linear scan
//! of 193 rows happens once per spawn, which is already the most expensive thing in the system.
//!
//! ## Why it lives here, and is shared by source
//!
//! TWO principals spawn probes: the `supervisor` starts each suite, and a probe RESPAWNS its own
//! victim (a restart test has to). Both need the same parameters, and two copies of a parameter
//! table is two truths (Commandment III) - the second one drifts, and a probe respawned with the
//! wrong mode is a test that passes while testing the wrong thing.
//!
//! So there is ONE file, and the `supervisor` includes it by path. It lives with the `probe` program
//! because it describes that program's test modes, not the supervisor's policy.
//!
//! See `docs/probe-params-design.md`.

use godspeed_sdk::service_context::{privbits, ServiceContext};

/// `(name, mode, has_recv_endpoint, memory MiB - 0 = default, core, send peers)`
const PROBES: &[(&str, u32, bool, u32, Option<u32>, &[&str])] = &[
    ("adv-a1", 80, false, 0, None, &[]),
    ("adv-a10", 90, false, 0, None, &[]),
    ("adv-a11", 161, false, 0, None, &[]),
    ("adv-a12", 162, false, 0, None, &[]),
    ("adv-a13", 163, false, 0, None, &[]),
    ("adv-a2", 81, false, 0, None, &[]),
    ("adv-a3", 82, false, 4, None, &[]),
    ("adv-a4", 83, true, 0, None, &[]),
    ("adv-a5", 84, false, 0, None, &["adv-a5-victim"]),
    ("adv-a5-victim", 0, true, 0, None, &[]),
    ("adv-a6", 85, true, 0, None, &[]),
    ("adv-a7", 86, false, 0, None, &["adv-a7-recv"]),
    ("adv-a7-recv", 0, true, 0, None, &[]),
    ("adv-a8", 87, false, 0, None, &[]),
    ("adv-a8-witness", 88, false, 0, None, &[]),
    ("adv-a9", 89, false, 0, None, &[]),
    ("adv-ba1", 144, false, 0, None, &[]),
    ("adv-ba10", 154, false, 0, None, &[]),
    ("adv-ba2", 145, false, 0, None, &[]),
    ("adv-ba3", 146, false, 4, None, &[]),
    ("adv-ba4", 147, true, 0, None, &[]),
    ("adv-ba5", 148, false, 0, None, &["adv-ba5-victim"]),
    ("adv-ba5-victim", 0, true, 0, None, &[]),
    ("adv-ba6", 149, true, 0, None, &[]),
    ("adv-ba7", 150, false, 0, None, &["adv-ba7-recv"]),
    ("adv-ba7-recv", 0, true, 0, None, &[]),
    ("adv-ba8", 151, false, 0, Some(3), &[]),
    ("adv-ba8-witness", 152, false, 0, Some(3), &[]),
    ("adv-ba9", 153, false, 0, None, &[]),
    ("adv-fault-de", 211, false, 0, None, &[]),
    ("adv-fault-gp", 210, false, 0, None, &[]),
    ("adv-fault-mon", 212, false, 0, None, &[]),
    ("adv-fault-usercopy", 213, false, 0, None, &[]),
    ("adv-fault-usercopy-mon", 214, false, 0, None, &[]),
    ("brutal-id-11", 97, true, 0, None, &["brutal-id-11"]),
    ("brutal-id-12-a", 98, false, 0, None, &["brutal-id-12-b"]),
    ("brutal-id-12-b", 99, true, 0, None, &["brutal-id-12-c"]),
    ("brutal-id-12-c", 100, true, 0, None, &[]),
    ("brutal-id-13-kill", 103, false, 0, Some(1), &[]),
    ("brutal-id-13-recv", 101, true, 0, Some(2), &[]),
    ("brutal-id-13-send", 102, false, 0, Some(0), &["brutal-id-13-recv"]),
    ("chaos-bc2-a", 91, false, 0, None, &[]),
    ("chaos-bc2-b", 91, false, 0, None, &[]),
    ("chaos-bc2-c", 91, false, 0, None, &[]),
    ("chaos-bc2-d", 91, false, 0, None, &[]),
    ("chaos-bc2-e", 91, false, 0, None, &[]),
    ("chaos-bc2-monitor", 155, false, 0, None, &[]),
    ("chaos-bc3", 156, false, 4, None, &[]),
    ("chaos-bc5", 157, false, 0, None, &[]),
    ("chaos-bc6-hog-a", 7, false, 0, Some(2), &[]),
    ("chaos-bc6-hog-b", 7, false, 0, Some(3), &[]),
    ("chaos-bc6-monitor", 158, false, 0, Some(0), &[]),
    ("chaos-bc7", 159, false, 0, Some(1), &["chaos-bc7-victim"]),
    ("chaos-bc7-victim", 0, true, 0, Some(2), &[]),
    ("chaos-c2", 91, false, 0, None, &[]),
    ("chaos-c2-monitor", 92, false, 0, None, &[]),
    ("chaos-c3", 93, false, 4, None, &[]),
    ("chaos-c5", 94, false, 0, None, &[]),
    ("chaos-c6-hog", 7, false, 0, Some(3), &[]),
    ("chaos-c6-monitor", 95, false, 0, Some(0), &[]),
    ("chaos-c7", 96, false, 0, Some(1), &["chaos-c7-victim"]),
    ("chaos-c7-victim", 0, true, 0, Some(2), &[]),
    ("fuzz-bf1", 114, false, 0, None, &[]),
    ("fuzz-bf2", 115, false, 0, None, &[]),
    ("fuzz-bf5", 116, false, 0, None, &["fuzz-bf5-recv"]),
    ("fuzz-bf5-recv", 0, true, 0, None, &[]),
    ("fuzz-bf6", 117, false, 0, None, &["fuzz-bf6-recv"]),
    ("fuzz-bf6-recv", 0, true, 0, None, &[]),
    ("fuzz-bf7", 118, false, 0, None, &["fuzz-bf7-victim"]),
    ("fuzz-bf7-victim", 0, true, 0, None, &[]),
    ("fuzz-bf8", 119, false, 0, None, &[]),
    ("fuzz-f1", 30, false, 0, None, &[]),
    ("fuzz-f2", 31, false, 0, None, &[]),
    ("fuzz-f5", 32, false, 0, None, &["fuzz-f5-recv"]),
    ("fuzz-f5-recv", 0, true, 0, None, &[]),
    ("fuzz-f6", 33, false, 0, None, &["fuzz-f6-recv"]),
    ("fuzz-f6-recv", 0, true, 0, None, &[]),
    ("fuzz-f7", 34, false, 0, None, &["fuzz-f7-victim"]),
    ("fuzz-f7-victim", 0, true, 0, None, &[]),
    ("fuzz-f8", 35, false, 0, None, &[]),
    ("perf-b1", 60, true, 0, Some(0), &[]),
    ("perf-b1-echo", 61, true, 0, Some(0), &["perf-b1"]),
    ("perf-b10", 71, false, 0, None, &[]),
    ("perf-b2", 62, true, 0, Some(0), &[]),
    ("perf-b2-echo", 63, true, 0, Some(1), &["perf-b2"]),
    ("perf-b3", 64, false, 0, None, &[]),
    ("perf-b4", 65, true, 0, None, &[]),
    ("perf-b5", 66, false, 0, None, &[]),
    ("perf-b5-victim", 0, true, 0, None, &[]),
    ("perf-b7", 67, true, 0, None, &[]),
    ("perf-b8", 68, false, 0, None, &[]),
    ("perf-b9", 69, false, 0, Some(0), &["perf-b9-recv"]),
    ("perf-b9-recv", 70, true, 0, Some(0), &[]),
    ("perf-bp1", 132, true, 0, Some(0), &[]),
    ("perf-bp1-echo", 133, true, 0, Some(0), &["perf-bp1"]),
    ("perf-bp10", 143, false, 0, None, &[]),
    ("perf-bp2", 134, true, 0, Some(0), &[]),
    ("perf-bp2-echo", 135, true, 0, Some(1), &["perf-bp2"]),
    ("perf-bp3", 136, false, 0, None, &[]),
    ("perf-bp4", 137, true, 0, None, &[]),
    ("perf-bp5", 138, false, 0, None, &[]),
    ("perf-bp5-victim", 0, true, 0, None, &[]),
    ("perf-bp7", 139, true, 0, None, &[]),
    ("perf-bp8", 140, false, 0, None, &[]),
    ("perf-bp9", 141, false, 0, Some(0), &["perf-bp9-recv"]),
    ("perf-bp9-recv", 142, true, 0, Some(0), &[]),
    ("probe-11a", 160, true, 0, None, &[]),
    ("probe-3b", 3, true, 0, Some(0), &[]),
    ("probe-4a", 4, false, 0, Some(0), &["probe-victim"]),
    ("probe-4b-recv", 0, true, 0, Some(0), &[]),
    ("probe-4b-send", 5, false, 0, Some(0), &["probe-4b-recv"]),
    ("probe-5a-recv", 9, true, 0, Some(0), &[]),
    ("probe-5a-send", 10, false, 0, Some(0), &["probe-5a-recv"]),
    ("probe-5b-send", 11, false, 0, Some(0), &["probe-5a-recv"]),
    ("probe-7a", 12, false, 0, Some(0), &[]),
    ("probe-7b", 13, false, 0, Some(0), &[]),
    ("probe-9b", 8, false, 0, Some(0), &[]),
    ("probe-hog", 7, false, 0, Some(0), &[]),
    ("probe-recv", 1, true, 0, Some(0), &[]),
    ("probe-sender", 2, false, 0, Some(0), &["probe-recv"]),
    ("probe-victim", 0, true, 0, Some(0), &[]),
    ("probe-yielder", 6, false, 0, Some(0), &[]),
    ("prop-bp1", 104, false, 0, None, &[]),
    ("prop-bp10", 113, false, 0, None, &[]),
    ("prop-bp2", 105, false, 0, None, &[]),
    ("prop-bp2-victim", 0, true, 0, None, &[]),
    ("prop-bp3", 106, true, 0, None, &[]),
    ("prop-bp4", 107, false, 0, None, &[]),
    ("prop-bp5", 108, false, 0, None, &[]),
    ("prop-bp5-victim", 0, true, 0, None, &[]),
    ("prop-bp6", 109, true, 0, None, &["prop-bp6"]),
    ("prop-bp7", 110, false, 0, None, &[]),
    ("prop-bp7-victim", 0, true, 0, None, &[]),
    ("prop-bp8", 111, false, 0, None, &[]),
    ("prop-bp8-victim", 0, true, 0, None, &[]),
    ("prop-bp9", 112, false, 0, None, &["prop-bp9-victim", "prop-bp9-victim", "prop-bp9-victim"]),
    ("prop-bp9-victim", 0, true, 0, None, &[]),
    ("prop-p1", 20, false, 0, Some(0), &[]),
    ("prop-p10", 22, false, 0, Some(0), &[]),
    ("prop-p2", 23, false, 0, Some(3), &[]),
    ("prop-p2-victim", 0, true, 0, None, &[]),
    ("prop-p3", 24, true, 0, None, &[]),
    ("prop-p4", 27, false, 0, None, &[]),
    ("prop-p5", 28, false, 0, None, &[]),
    ("prop-p5-victim", 0, true, 0, None, &[]),
    ("prop-p6", 25, true, 0, Some(2), &["prop-p6"]),
    ("prop-p7", 29, false, 0, None, &[]),
    ("prop-p7-victim", 0, true, 0, None, &[]),
    ("prop-p8", 26, false, 0, Some(1), &[]),
    ("prop-p8-victim", 0, true, 0, None, &[]),
    ("prop-p9", 21, false, 0, Some(0), &["prop-p9-victim", "prop-p9-victim", "prop-p9-victim"]),
    ("prop-p9-victim", 0, true, 0, Some(0), &[]),
    ("stress-bs1", 120, false, 0, None, &["stress-bs1-recv"]),
    ("stress-bs1-recv", 0, true, 0, None, &[]),
    ("stress-bs10", 131, false, 0, Some(0), &["stress-bs10-victim", "stress-bs10-victim", "stress-bs10-victim"]),
    ("stress-bs10-victim", 0, true, 0, Some(1), &[]),
    ("stress-bs2", 121, false, 0, None, &["stress-bs2-victim"]),
    ("stress-bs2-victim", 0, true, 0, None, &[]),
    ("stress-bs3-recv", 123, true, 0, Some(1), &[]),
    ("stress-bs3-send", 122, false, 0, Some(0), &["stress-bs3-recv"]),
    ("stress-bs4", 124, false, 0, None, &["stress-bs4-victim", "stress-bs4-victim"]),
    ("stress-bs4-victim", 0, true, 0, None, &[]),
    ("stress-bs5", 125, false, 0, None, &[]),
    ("stress-bs5-victim", 0, true, 0, None, &[]),
    ("stress-bs6", 126, true, 0, None, &["stress-bs6"]),
    ("stress-bs7", 127, false, 0, None, &[]),
    ("stress-bs8", 128, false, 0, None, &[]),
    ("stress-bs9-recv", 130, true, 0, Some(2), &[]),
    ("stress-bs9-send-a", 129, false, 0, Some(0), &["stress-bs9-recv"]),
    ("stress-bs9-send-b", 129, false, 0, Some(1), &["stress-bs9-recv"]),
    ("stress-s1", 40, false, 0, None, &["stress-s1-recv"]),
    ("stress-s1-recv", 0, true, 0, None, &[]),
    ("stress-s10", 46, false, 0, Some(0), &["stress-s10-victim", "stress-s10-victim", "stress-s10-victim"]),
    ("stress-s10-victim", 0, true, 0, Some(1), &[]),
    ("stress-s2", 41, false, 0, None, &["stress-s2-victim"]),
    ("stress-s2-victim", 0, true, 0, None, &[]),
    ("stress-s3-recv", 43, true, 0, Some(1), &[]),
    ("stress-s3-send", 42, false, 0, Some(0), &["stress-s3-recv"]),
    ("stress-s4", 44, false, 0, None, &["stress-s4-victim", "stress-s4-victim"]),
    ("stress-s4-victim", 0, true, 0, None, &[]),
    ("stress-s5", 47, false, 0, None, &[]),
    ("stress-s5-victim", 0, true, 0, None, &[]),
    ("stress-s6", 48, true, 0, None, &["stress-s6"]),
    ("stress-s7", 45, false, 0, None, &[]),
    ("stress-s8", 49, false, 0, None, &[]),
    ("stress-s9-recv", 51, true, 0, Some(2), &[]),
    ("stress-s9-send-a", 50, false, 0, Some(0), &["stress-s9-recv"]),
    ("stress-s9-send-b", 50, false, 0, Some(1), &["stress-s9-recv"]),
    ("xlife", 202, false, 0, Some(1), &[]),
    ("xlife-far", 203, false, 0, Some(2), &[]),
    ("xlife-near", 203, false, 0, Some(1), &[]),
    ("xsend", 200, false, 0, Some(1), &["xsend-recv"]),
    ("xsend-recv", 201, true, 0, Some(2), &[]),
];

/// One probe's parameters: `(name, mode, has_recv_endpoint, memory MiB, core, send peers)`.
pub type Row = (&'static str, u32, bool, u32, Option<u32>, &'static [&'static str]);

/// The row for `name`, or `None`.
pub fn row(name: &str) -> Option<Row> {
    PROBES.iter().copied().find(|&(n, ..)| n == name)
}

/// The privileges a probe is spawned with.
///
/// These used to be derived INSIDE THE KERNEL from the task's name - `is_probe` (an ELF-pointer
/// comparison) plus two name tests. That was workable while the kernel owned every name, and became
/// a hole once callers supplied them: `service_privileges` granted INTROSPECT to anything whose name
/// began `prop-` or `stress-`, so a SPAWN holder could obtain the privilege BY CHOOSING A STRING.
/// The kernel's own comment named the fix - "carry a privilege word like `SpawnImage` does, checked
/// against what the CALLER may delegate" - and this is it. The rule is unchanged; what changed is WHO
/// states it (the spawner, explicitly) and that the kernel now CHECKS it against the supervisor's own
/// holdings instead of inferring it from a string.
pub fn privileges_of(name: &str) -> u32 {
    // Every probe kills victims to exercise kill/revocation, and spawns them to begin with.
    let mut p = privbits::SPAWN | privbits::SERVICE_CONTROL;
    // adv-a13 is the §22 Test A13 NEGATIVE pin: it must hold NO ACQUIRE_ANY, so that
    // AcquireSendCap can be shown to deny a non-holder. Excluding it here is the test's subject.
    if name != "adv-a13" { p |= privbits::ACQUIRE_ANY; }
    // The property/stress drivers read their victims' generations. adv-a11 must NOT have this - it
    // asserts that a probe WITHOUT INTROSPECT is denied - which is why this cannot be "every probe".
    if name.starts_with("prop-") || name.starts_with("stress-") { p |= privbits::INTROSPECT; }
    p
}

/// The device class a probe drives, if any.
///
/// `probe-11a` receives the software test interrupt (§22 IR1). Routing a vector is AUTHORITY, so the
/// probe names a CLASS and the kernel supplies the number it assigned - the same rule every driver
/// follows, which is what let this stop being a name the kernel had to know.
pub fn hw_class_of(name: &str) -> u32 {
    if name == "probe-11a" { godspeed_sdk::service_context::hwclass::TEST_IRQ } else { 0 }
}

/// Does this probe need its peer caps minted WITH `GRANT`?
///
/// `probe-5a-send` re-delegates a cap it was given (§22 Test 5A), which is the whole of that test.
/// Handing out a re-delegatable capability is itself a grant, so this is authority too - carried as
/// a spawn FLAG now rather than a name the kernel keeps.
pub fn peers_grant_of(name: &str) -> bool { name == "probe-5a-send" }

/// Spawn a probe by name, supplying its parameters from the table above.
///
/// Loud on a name that is not in the table (invariant 12): a probe that silently did not run would
/// read as a passing test suite, which is the worst possible failure here.
///
/// HOW it starts is the caller's business, because the two principals that spawn probes differ in
/// what they hold. The `supervisor` holds the probe IMAGE and starts it directly; a probe respawning
/// its own victim does not, and asks the supervisor. Each crate supplies `spawn_probe_row`.
pub fn probe(ctx: &ServiceContext, name: &str) -> Result<(), godspeed_sdk::Error> {
    match row(name) {
        Some(r) => crate::spawn_probe_row(ctx, r),
        None => {
            ctx.log_fmt(format_args!("probe-table: no probe named '{}' - not spawned", name));
            Err(godspeed_sdk::Error::InvalidArgument)
        }
    }
}
