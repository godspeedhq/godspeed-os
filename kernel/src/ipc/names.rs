//! Kernel name registry — maps service names to recv endpoint IDs (§14.2).
//!
//! Populated by the kernel at spawn time for every service that gets a recv
//! endpoint.  Queried by syscall 10 (AcquireSendCap) for post-restart cap
//! rebinding and at spawn time to wire up send-peer SEND caps.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::capability::generation::Generation;
use crate::ipc::endpoint::EndpointId;

const MAX_ENTRIES: usize = 16;
const NAME_MAX:    usize = 32;

static NAMES_LOCKED: AtomicBool = AtomicBool::new(false);

fn lock() {
    while NAMES_LOCKED
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

fn unlock() {
    NAMES_LOCKED.store(false, Ordering::Release);
}

#[derive(Clone, Copy)]
struct NameEntry {
    valid:       bool,
    name_len:    u8,
    name:        [u8; NAME_MAX],
    endpoint_id: EndpointId,
}

impl NameEntry {
    const fn empty() -> Self {
        Self {
            valid: false,
            name_len: 0,
            name: [0u8; NAME_MAX],
            endpoint_id: EndpointId(0),
        }
    }
}

// SAFETY: protected by NAMES_LOCKED spinlock; single writer at a time.
static mut NAMES: [NameEntry; MAX_ENTRIES] = {
    const E: NameEntry = NameEntry::empty();
    [E; MAX_ENTRIES]
};

/// Register or update a `name → endpoint_id` mapping.
///
/// Updates an existing entry if the name is already present.
/// The generation is always the current one recorded in `ipc::routing`; callers
/// do not need to pass it — `AcquireSendCap` reads the fresh generation from
/// the routing table when minting the cap.
pub fn register(name: &str, endpoint_id: EndpointId) {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_MAX { return; }
    let len = bytes.len() as u8;

    lock();
    // SAFETY: lock held; only one writer at a time.
    unsafe {
        // Update existing entry.
        for entry in NAMES.iter_mut() {
            if entry.valid && entry.name_len == len
                && &entry.name[..len as usize] == bytes
            {
                entry.endpoint_id = endpoint_id;
                unlock();
                return;
            }
        }
        // Insert in first free slot.
        for entry in NAMES.iter_mut() {
            if !entry.valid {
                entry.valid       = true;
                entry.name_len    = len;
                entry.name        = [0u8; NAME_MAX];
                entry.name[..len as usize].copy_from_slice(bytes);
                entry.endpoint_id = endpoint_id;
                unlock();
                return;
            }
        }
    }
    unlock();
    crate::kprintln!("ipc::names: table full, cannot register '{}'", name);
}

/// Look up an endpoint ID by service name.
pub fn lookup(name: &str) -> Option<EndpointId> {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_MAX { return None; }
    let len = bytes.len() as u8;

    lock();
    // SAFETY: lock held.
    let result = unsafe {
        NAMES.iter().find(|e| {
            e.valid && e.name_len == len && &e.name[..len as usize] == bytes
        }).map(|e| e.endpoint_id)
    };
    unlock();
    result
}
