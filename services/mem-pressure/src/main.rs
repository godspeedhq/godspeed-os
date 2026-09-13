// SPDX-License-Identifier: GPL-2.0-only
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
//! `mem-pressure` - a spawn-on-demand memory pressure victim for the shell's `chaos mem-pressure`
//! command. Idle until spawned; on spawn it allocates 4 MiB chunks up to its contract memory
//! limit, asserting the §22 S7 invariant - once `AllocDenied` appears it must be STICKY (an `Ok`
//! after a `Denied` is a kernel memory-accounting bug, §10.3/§10.4) - then parks.
//!
//! v1 reclaims memory only at DEATH (no free syscall), so `chaos mem-pressure` "frees" this
//! service's memory by KILLING it: it watches the kernel's free-frame count drop while we hold our
//! allocation and return to baseline once we die (the no-leak check). Not in any auto-spawn set;
//! the shell spawns it by name only when running the command.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, service_context::AllocError};

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Force the EL0 fault the kernel's recovery path is supposed to survive (see this crate's
    // `el0-fault-test` feature). The kernel must KILL this task and keep running; if the machine
    // stops here, the recovery is broken and the log's last line says which task it stopped on.
    #[cfg(feature = "el0-fault-test")]
    {
        ctx.log("mem-pressure: el0-fault-test - taking a deliberate null read; the kernel must kill ME, not the machine");
        godspeed_sdk::adversarial::fault_null_read();
        ctx.log("mem-pressure: STILL ALIVE after a null read - the kernel did NOT fault-kill this task");
    }
    const CHUNK: usize = 4 * 1024 * 1024; // 4 MiB - same chunk the S7 probe uses

    let mut at_limit = false;
    let mut ok_after_denied = false;
    // Far more passes than (limit / CHUNK), so we definitely reach the limit and keep trying after.
    for _ in 0..64u32 {
        match ctx.alloc_mem(CHUNK) {
            Ok(_) => {
                // §22 S7 / §10.4: once denied, the limit is enforced for good. An Ok here means the
                // kernel under-counted our usage - a real accounting bug.
                if at_limit { ok_after_denied = true; }
            }
            Err(AllocError::Denied) => at_limit = true,
            Err(_)                  => {} // unexpected; the at_limit check below catches non-enforcement
        }
    }

    if ok_after_denied {
        ctx.log("mem-pressure: FAIL - Ok returned after AllocDenied (kernel memory-accounting bug)");
    } else if at_limit {
        ctx.log("mem-pressure: at limit - AllocDenied appeared and stuck (accounting consistent)");
    } else {
        ctx.log("mem-pressure: WARN - AllocDenied never returned (limit not enforced?)");
    }

    // Hold the allocation and idle until the chaos command kills us (death = the only way memory is
    // reclaimed in v1, §10.5). Park rather than busy-yield so the core can still halt.
    ctx.park();
}
