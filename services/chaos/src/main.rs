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
//! pool, the way the loop used to exclude "shell". The two untouchables during a run are `chaos` and
//! the kernel; everything else, the shell included, is fair game and recovers.
//!
//! It claims exclusive console input (the foreground primitive, syscall 40) so a resurrected shell
//! polling the keyboard cannot swallow its `q`-to-quit, draws a bounded 20-line TUI of the storm, and
//! on finish or `q` ensures a live shell exists, releases the foreground, and self-terminates.
//!
//! Phase 2a (this commit) is the SKELETON: it proves the service spawns with its caps
//! (SERVICE_CONTROL, INTROSPECT, ACQUIRE_ANY, SPAWN, CONSOLE_READ, LOG_WRITE) and that the foreground
//! claim/release link. The carnage loop and the 20-line TUI ring replace the body in Phase 2b.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Take the keyboard so a resurrected shell cannot steal our `q`. Unclaimed is the normal state,
    // so this is the moment the shell goes "muted" for the duration of the run.
    ctx.claim_console_foreground();
    ctx.log("chaos: ready - foreground claimed (Phase 2a skeleton; carnage loop lands in 2b)");

    // --- Phase 2b: the carnage loop + the bounded 20-line TUI ring go here. ---

    // Hand the keyboard back so the live shell resumes, then idle. Phase 2b self-terminates here
    // (after ensuring a live shell exists) so a finished run leaves no chaos task behind.
    ctx.release_console_foreground();
    ctx.log("chaos: done - foreground released");
    ctx.park();
}
