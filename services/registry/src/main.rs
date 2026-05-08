//! `registry` — name → endpoint resolution. TCB member (§6.1).
//!
//! Provides two operations over IPC:
//!   - `register(name, endpoint_cap)`: a service announces its endpoint.
//!     Only one registration per name; re-registration on restart replaces
//!     the previous entry (which is already dead by the time this is called).
//!   - `lookup(name)`: returns a fresh capability to the named endpoint, or
//!     `NotFound` if the service is not registered.
//!
//! The registry is stateless across its own restarts (it is non-restartable
//! in v1 — §6.1). If registry itself goes down, caps cannot be reacquired,
//! which is why its failure causes a kernel panic.
//!
//! Registry does NOT authenticate callers; that is the contract/cap system's job.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message};

const MAX_ENTRIES: usize = 64;

struct RegistryEntry {
    name: [u8; 32],
    name_len: usize,
    // The capability is stored as an opaque handle; the kernel holds the real cap.
    endpoint_cap_slot: u32,
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    let mut entries: [Option<RegistryEntry>; MAX_ENTRIES] = [const { None }; MAX_ENTRIES];

    ctx.log("registry: ready");

    loop {
        let msg = ctx.recv();
        match RegistryRequest::parse(&msg) {
            RegistryRequest::Register { name, cap_slot } => {
                handle_register(&mut entries, name, cap_slot, &ctx);
            }
            RegistryRequest::Lookup { name } => {
                handle_lookup(&entries, name, &ctx, &msg);
            }
            RegistryRequest::Unknown => {
                ctx.log("registry: unknown request, ignoring");
            }
        }
    }
}

fn handle_register(
    entries: &mut [Option<RegistryEntry>; MAX_ENTRIES],
    name: &[u8],
    cap_slot: u32,
    ctx: &ServiceContext,
) {
    todo!("find or replace entry for name; log registration")
}

fn handle_lookup(
    entries: &[Option<RegistryEntry>; MAX_ENTRIES],
    name: &[u8],
    ctx: &ServiceContext,
    request_msg: &Message,
) {
    todo!("find entry, reply with fresh cap or NotFound error message")
}

enum RegistryRequest<'a> {
    Register { name: &'a [u8], cap_slot: u32 },
    Lookup   { name: &'a [u8] },
    Unknown,
}

impl<'a> RegistryRequest<'a> {
    fn parse(msg: &'a Message) -> Self {
        todo!("decode first byte as opcode; decode name and optional cap slot from payload")
    }
}
