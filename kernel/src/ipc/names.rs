// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Kernel name registry — maps service names to recv endpoint IDs (§14.2).
//!
//! Populated by the kernel at spawn time for every service that gets a recv
//! endpoint.  Queried by syscall 10 (AcquireSendCap) for post-restart cap
//! rebinding and at spawn time to wire up send-peer SEND caps.

use crate::ipc::endpoint::EndpointId;
use crate::smp::SpinLock;

const MAX_ENTRIES: usize = 128;
const NAME_MAX:    usize = 32;

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

static NAMES: SpinLock<[NameEntry; MAX_ENTRIES]> = {
    const E: NameEntry = NameEntry::empty();
    SpinLock::new([E; MAX_ENTRIES])
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

    let mut names = NAMES.lock();
    // Update existing entry.
    for entry in names.iter_mut() {
        if entry.valid && entry.name_len == len
            && &entry.name[..len as usize] == bytes
        {
            entry.endpoint_id = endpoint_id;
            return;
        }
    }
    // Insert in first free slot.
    for entry in names.iter_mut() {
        if !entry.valid {
            entry.valid       = true;
            entry.name_len    = len;
            entry.name        = [0u8; NAME_MAX];
            entry.name[..len as usize].copy_from_slice(bytes);
            entry.endpoint_id = endpoint_id;
            return;
        }
    }
    drop(names);
    crate::kprintln!("ipc::names: table full, cannot register '{}'", name);
}

/// Remove the entry for `name`, freeing its slot for future registrations.
pub fn unregister(name: &str) {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_MAX { return; }
    let len = bytes.len() as u8;
    let mut names = NAMES.lock();
    for entry in names.iter_mut() {
        if entry.valid && entry.name_len == len
            && &entry.name[..len as usize] == bytes
        {
            entry.valid = false;
            return;
        }
    }
}

/// Remove the entry for `name` **only if** it still maps to `endpoint_id` — the dying instance.
///
/// Called from the task-kill path (§14.2) so a service's name stops resolving to a DEAD endpoint:
/// the supervisor's reconcile (it re-runs its spawn sequence on its own respawn) then finds the name
/// *missing* and respawns the service, instead of adopting the stale dead entry — the bug behind
/// `fs`/`block-driver` staying dead when a storm kills them in the same window the supervisor itself
/// is being respawned (so their death-notifications are lost). The `endpoint_id` guard is the
/// respawn-race safety: if a fresh instance has *already* re-registered the name to a new endpoint,
/// this is a no-op — we must never unregister the live one.
pub fn unregister_endpoint(name: &str, endpoint_id: EndpointId) {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_MAX { return; }
    let len = bytes.len() as u8;
    let mut names = NAMES.lock();
    for entry in names.iter_mut() {
        if entry.valid && entry.name_len == len
            && &entry.name[..len as usize] == bytes
            && entry.endpoint_id == endpoint_id
        {
            entry.valid = false;
            return;
        }
    }
}

/// Look up an endpoint ID by service name.
pub fn lookup(name: &str) -> Option<EndpointId> {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_MAX { return None; }
    let len = bytes.len() as u8;

    let names = NAMES.lock();
    names.iter().find(|e| {
        e.valid && e.name_len == len && &e.name[..len as usize] == bytes
    }).map(|e| e.endpoint_id)
}
