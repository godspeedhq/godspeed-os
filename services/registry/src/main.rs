// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! `registry` — name → endpoint resolution (§14.2). TCB member (§6.1) in v1.
//!
//! Phase 5 (H11): a real userspace name service. Services announce themselves with
//! `register(name)` (granting the registry a SEND|GRANT cap to their endpoint); other
//! services `lookup(name)` and the registry derives a SEND copy of that cap and grants
//! it back. Cap minting stays in the kernel — the registry only ever *derives* copies
//! of caps it already holds (syscall 29), never fabricates authority.
//!
//! State is a single owned in-memory `name → cap` table (no global, §3.9). It is lost
//! on restart; making the registry restartable (re-registration + the unresolvable-name
//! window) is the next H11 step.

#![no_std]
#![no_main]

use godspeed_sdk::{
    CapHandle, Message, ServiceContext,
    REGISTRY_OP_REGISTER, REGISTRY_OP_LOOKUP, REGISTRY_FOUND, REGISTRY_NOT_FOUND,
    REGISTRY_NAME_MAX,
};

const MAX_ENTRIES: usize = 64;

#[derive(Clone, Copy)]
struct Entry {
    used:     bool,
    name:     [u8; REGISTRY_NAME_MAX],
    name_len: usize,
    cap:      CapHandle, // SEND|GRANT cap to the named service's endpoint
}

impl Entry {
    const EMPTY: Entry = Entry {
        used:     false,
        name:     [0u8; REGISTRY_NAME_MAX],
        name_len: 0,
        cap:      CapHandle(u32::MAX),
    };
}

struct Table {
    entries: [Entry; MAX_ENTRIES],
}

impl Table {
    fn find(&self, name: &[u8]) -> Option<CapHandle> {
        for e in &self.entries {
            if e.used && &e.name[..e.name_len] == name {
                return Some(e.cap);
            }
        }
        None
    }

    /// Insert or replace `name → cap`. Returns the old cap if a previous entry under
    /// this name was replaced (so the caller can free it).
    fn insert(&mut self, name: &[u8], cap: CapHandle) -> Option<CapHandle> {
        let len = name.len().min(REGISTRY_NAME_MAX);
        // Replace an existing entry with the same name.
        for e in &mut self.entries {
            if e.used && &e.name[..e.name_len] == name {
                let old = e.cap;
                e.cap = cap;
                return Some(old);
            }
        }
        // Otherwise take the first free slot.
        for e in &mut self.entries {
            if !e.used {
                e.used = true;
                e.name_len = len;
                e.name[..len].copy_from_slice(&name[..len]);
                e.cap = cap;
                return None;
            }
        }
        None // table full — drop the registration (loud failure would need a reply path)
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("registry: ready");

    let mut table = Table { entries: [Entry::EMPTY; MAX_ENTRIES] };

    loop {
        let msg = ctx.recv();
        let p = msg.payload_bytes();
        if p.len() < 2 { continue; }
        let op = p[0];
        let nlen = (p[1] as usize).min(REGISTRY_NAME_MAX);
        if p.len() < 2 + nlen { continue; }
        let name = &p[2..2 + nlen];

        match op {
            REGISTRY_OP_REGISTER => {
                // The registrant granted us a SEND|GRANT cap to its endpoint.
                if let Some(cap) = ctx.take_pending_cap() {
                    if let Some(old) = table.insert(name, cap) {
                        ctx.remove_cap(old); // free the stale cap from a re-registration
                    }
                }
            }
            REGISTRY_OP_LOOKUP => {
                // The client embedded a SEND cap to its own reply endpoint.
                if let Some(reply) = ctx.take_pending_cap() {
                    match table.find(name).and_then(|held| ctx.derive_cap(held)) {
                        Some(derived) => {
                            // Grant the derived SEND copy back to the client.
                            let _ = ctx.send_with_cap_by_handle(
                                reply, derived, &Message::from_bytes(&[REGISTRY_FOUND]));
                        }
                        None => {
                            let _ = ctx.send_by_handle(
                                reply, &Message::from_bytes(&[REGISTRY_NOT_FOUND]));
                        }
                    }
                    ctx.remove_cap(reply); // the reply cap is per-lookup; don't accumulate
                }
            }
            _ => {} // unknown op — ignore
        }
    }
}
