// SPDX-License-Identifier: GPL-2.0-only
//! Kernel name directory - maps service names to recv endpoint IDs (§14.2).
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


/// Report a stale `name -> endpoint` entry evicted by `register`. Called with the table lock RELEASED,
/// because logging under it is what the rest of this module already avoids.
fn report_evicted(evicted: Option<([u8; NAME_MAX], u8)>, new_name: &str, endpoint_id: EndpointId) {
    let Some((buf, n)) = evicted else { return };
    let stale = core::str::from_utf8(&buf[..n as usize]).unwrap_or("<invalid utf8>");
    crate::kprintln!(
        "ipc::names: endpoint {:?} was still mapped to '{}' when '{}' claimed it - evicted the stale          entry (its owner died without unregistering; a lookup of '{}' would have MISROUTED here)",
        endpoint_id, stale, new_name, stale
    );
}

/// Register or update a `name → endpoint_id` mapping.
///
/// Updates an existing entry if the name is already present.
/// The generation is always the current one recorded in `ipc::routing`; callers
/// do not need to pass it - `AcquireSendCap` reads the fresh generation from
/// the routing table when minting the cap.
pub fn register(name: &str, endpoint_id: EndpointId) {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_MAX { return; }
    let len = bytes.len() as u8;

    let mut names = NAMES.lock_irq();

    // ONE ENDPOINT, ONE NAME. An endpoint id belongs to exactly one task (property P5), so at most
    // one name may map to it. If a DIFFERENT name still points at the id being registered here, that
    // entry is provably stale: the id has just been handed to `name`, so whoever held it before is
    // gone and its unregister did not run against this id.
    //
    // Leaving it costs a MISROUTE, not merely a stale lookup - and a misroute the generation check
    // cannot catch, because the cap minted from that name is fresh and its endpoint is genuinely
    // alive. It just belongs to somebody else. That is how a 1000-round carnage run ended with `fs`
    // sending block requests to `time`, which answered "unknown op 161"; `fs` saw a reply that did
    // not match its tag, concluded "device I/O error", failed its re-mount and DEGRADED a filesystem
    // whose disk was perfectly healthy.
    //
    // Evicting here is the fix, not a diagnostic: after this, the id resolves only to its current
    // owner. It is still reported, because a stale entry means a death path did not complete and
    // that is worth knowing even once it is survivable (§26.7).
    let mut evicted: Option<([u8; NAME_MAX], u8)> = None;
    for entry in names.iter_mut() {
        if entry.valid
            && entry.endpoint_id == endpoint_id
            && !(entry.name_len == len && &entry.name[..len as usize] == bytes)
        {
            evicted = Some((entry.name, entry.name_len));
            entry.valid = false;
        }
    }

    // Update existing entry.
    for entry in names.iter_mut() {
        if entry.valid && entry.name_len == len
            && &entry.name[..len as usize] == bytes
        {
            entry.endpoint_id = endpoint_id;
            drop(names);
            report_evicted(evicted, name, endpoint_id);
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
            drop(names);
            report_evicted(evicted, name, endpoint_id);
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
    let mut names = NAMES.lock_irq();
    for entry in names.iter_mut() {
        if entry.valid && entry.name_len == len
            && &entry.name[..len as usize] == bytes
        {
            entry.valid = false;
            return;
        }
    }
}

/// What `unregister_endpoint` actually did. Returned rather than logged, because this module does its
/// logging with the table lock RELEASED and because only the CALLER knows whether the outcome is
/// interesting: the caller is the kill path, and it can see whether another live task still answers to
/// this name (`scheduler::live_task_named_other_than`).
///
/// The distinction exists to catch ONE bug, and it is silent in a healthy run. Two instances of one
/// service can be alive at once (a supervisor respawn racing a death notification), and when the older
/// one dies its unregister must be a no-op - the newer instance has already claimed the name. That is
/// what the endpoint-id guard is for. If it ever CLEARS while a live task of the same name remains, the
/// guard has been defeated (an endpoint id reused, a kill path run twice, a respawn that never
/// registered), and the consequence is a live service that no client can find by name: `fs` reporting
/// storage unavailable against a `block-driver` that is running perfectly.
///
/// That is a failure whose symptom points at the wrong service, so it is worth naming at the exact
/// moment it happens rather than inferring it from the wreckage several minutes later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnregisterOutcome {
    /// The entry matched this endpoint and was removed.
    Cleared,
    /// The name is registered to a DIFFERENT endpoint - a newer instance already claimed it. No-op,
    /// which is the guard doing its job.
    KeptNewer(EndpointId),
    /// No entry for this name at all.
    NotFound,
}

/// Remove the entry for `name` **only if** it still maps to `endpoint_id` - the dying instance.
///
/// Called from the task-kill path (§14.2) so a service's name stops resolving to a DEAD endpoint:
/// the supervisor's reconcile (it re-runs its spawn sequence on its own respawn) then finds the name
/// *missing* and respawns the service, instead of adopting the stale dead entry - the bug behind
/// `fs`/`block-driver` staying dead when a storm kills them in the same window the supervisor itself
/// is being respawned (so their death-notifications are lost). The `endpoint_id` guard is the
/// respawn-race safety: if a fresh instance has *already* re-registered the name to a new endpoint,
/// this is a no-op - we must never unregister the live one.
pub fn unregister_endpoint(name: &str, endpoint_id: EndpointId) -> UnregisterOutcome {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_MAX { return UnregisterOutcome::NotFound; }
    let len = bytes.len() as u8;
    let mut names = NAMES.lock_irq();
    for entry in names.iter_mut() {
        if entry.valid && entry.name_len == len
            && &entry.name[..len as usize] == bytes
        {
            // The name is present. Whether it is OURS is the whole question, and the answer is
            // reported either way so the caller can tell a working guard from a defeated one.
            if entry.endpoint_id == endpoint_id {
                entry.valid = false;
                return UnregisterOutcome::Cleared;
            }
            return UnregisterOutcome::KeptNewer(entry.endpoint_id);
        }
    }
    UnregisterOutcome::NotFound
}

/// Look up an endpoint ID by service name.
pub fn lookup(name: &str) -> Option<EndpointId> {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_MAX { return None; }
    let len = bytes.len() as u8;

    let names = NAMES.lock_irq();
    names.iter().find(|e| {
        e.valid && e.name_len == len && &e.name[..len as usize] == bytes
    }).map(|e| e.endpoint_id)
}
