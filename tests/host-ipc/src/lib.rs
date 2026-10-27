// SPDX-License-Identifier: GPL-2.0-only
// Host tests over the REAL kernel IPC routing code. See Cargo.toml.
#![allow(dead_code, unused_imports, unused_macros, unexpected_cfgs)]

#[macro_export]
macro_rules! kprintln {
    ($($t:tt)*) => {{ let _ = format_args!($($t)*); }};
}

pub mod arch {
    pub mod imp {
        #[inline(always)] pub fn local_irq_save() -> bool { false }
        #[inline(always)] pub fn local_irq_restore(_was: bool) {}
    }
}

pub mod smp;

pub mod capability;

pub mod task {
    pub mod scheduler {
        // The kernel's value (kernel/src/task/scheduler.rs, MAX_TASKS); host_test_check.py checks they agree.
        pub const MAX_TASKS: usize = 224;
    }
}

pub mod ipc;

// ---------------------------------------------------------------------------------------------
// Helpers shared by the scenarios. They model ONLY what the dispatcher does with routing's return
// values (dispatch.rs build_message stamps sender_ep; a returned slot is a task to wake). They do
// not model the scheduler; where a scenario needs the scheduler, it says so.
// ---------------------------------------------------------------------------------------------

use capability::generation::Generation;
use ipc::endpoint::EndpointId;
use ipc::message::Message;

/// A message as `build_message` (dispatch.rs:717-743) would build it: payload + sender_ep stamped
/// with the sending task's primary endpoint id.
pub fn msg(tag: u8, sender_ep: u64) -> Message {
    let mut m = Message::new(&[tag]).unwrap();
    m.sender_ep = sender_ep;
    m
}

pub fn ep(id: u64) -> EndpointId { EndpointId(id) }
pub fn g(n: u32) -> Generation { Generation(n) }

/// Drain an endpoint with non-blocking dequeues (None = no receiver registration), returning tags.
pub fn drain_tags(e: EndpointId, gen: Generation) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok((m, _)) = ipc::routing::dequeue(e, gen, None) {
        out.push(m.payload_bytes()[0]);
    }
    out
}
