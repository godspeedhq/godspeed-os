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
    /// This name belongs to a SUPERVISOR-AUTHORED service, and is not available to a caller-chosen
    /// one. Survives the service's death, which `valid` deliberately does not - see `reserve`.
    reserved:    bool,
    name_len:    u8,
    name:        [u8; NAME_MAX],
    endpoint_id: EndpointId,
}

impl NameEntry {
    const fn empty() -> Self {
        Self {
            valid: false,
            reserved: false,
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

    // Update existing entry. `valid || reserved`, because a RESERVED entry for a dead service keeps
    // its name while its endpoint is gone - re-registering that name is exactly what a respawn does.
    for entry in names.iter_mut() {
        if (entry.valid || entry.reserved) && entry.name_len == len
            && &entry.name[..len as usize] == bytes
        {
            entry.valid       = true;
            entry.endpoint_id = endpoint_id;
            drop(names);
            report_evicted(evicted, name, endpoint_id);
            return;
        }
    }
    // Insert in first free slot. A reserved entry is OCCUPIED even with `valid == false`: its whole
    // purpose is to hold the name down after its service dies.
    for entry in names.iter_mut() {
        if !entry.valid && !entry.reserved {
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

/// Remove the entry for `name` **only if** it still maps to `endpoint_id` - the dying instance.
///
/// Called from the task-kill path (§14.2) so a service's name stops resolving to a DEAD endpoint:
/// the supervisor's reconcile (it re-runs its spawn sequence on its own respawn) then finds the name
/// *missing* and respawns the service, instead of adopting the stale dead entry - the bug behind
/// `fs`/`block-driver` staying dead when a storm kills them in the same window the supervisor itself
/// is being respawned (so their death-notifications are lost). The `endpoint_id` guard is the
/// respawn-race safety: if a fresh instance has *already* re-registered the name to a new endpoint,
/// this is a no-op - we must never unregister the live one.
/// Reserve `name` for a SUPERVISOR-AUTHORED service, permanently.
///
/// Restores a guarantee the step-C moves silently removed. `spawn_probe` lets a SPAWN holder choose
/// the name of the task it starts, and refused "a real service's name" by asking the kernel's service
/// catalogue. Moving the catalogue to the supervisor emptied that catalogue, so the refusal set
/// shrank to `{supervisor, probe}` and every other name - `fs`, `shell`, `logger`, `console` - became
/// available to squat.
///
/// The attack that guard was written against: wait for `fs` to die, start the PROBE binary under the
/// name `fs`, and clients reacquiring by name (14.3) wire themselves to it. The kernel's name
/// directory is a recovery ANCHOR, and an anchor that can be squatted is not one.
///
/// A reservation therefore outlives the service, because the danger window is precisely when the
/// service is DEAD - which is when a liveness check passes and `unregister_endpoint` has already
/// dropped the mapping. It does NOT make the name resolve: `lookup` still requires `valid`, so a dead
/// service's name misses exactly as before and its clients still get `EndpointDead`.
///
/// Reserving at spawn (rather than from a declared list) keeps this MECHANISM, not policy: the kernel
/// learns nothing about which services exist, it only remembers that a name it was given by the
/// spawner is not a name a caller may choose. That is sufficient for the actual threat - a client can
/// only be misdirected to a name it was wired to, and being wired to it means it was spawned, hence
/// reserved.
pub fn reserve(name: &str) {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_MAX { return; }
    let len = bytes.len() as u8;
    let mut names = NAMES.lock_irq();
    for entry in names.iter_mut() {
        if (entry.valid || entry.reserved) && entry.name_len == len
            && &entry.name[..len as usize] == bytes
        {
            entry.reserved = true;
            return;
        }
    }
    for entry in names.iter_mut() {
        if !entry.valid && !entry.reserved {
            entry.reserved = true;
            entry.name_len = len;
            entry.name     = [0u8; NAME_MAX];
            entry.name[..len as usize].copy_from_slice(bytes);
            return;
        }
    }
    drop(names);
    // Loud, because the consequence is a name that can now be squatted (invariant 12).
    crate::kprintln!("ipc::names: table full, cannot RESERVE '{}' - it is squattable", name);
}

/// Is `name` reserved for a supervisor-authored service? See `reserve`.
pub fn is_reserved(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_MAX { return false; }
    let len = bytes.len() as u8;
    let names = NAMES.lock_irq();
    names.iter().any(|e| e.reserved && e.name_len == len && &e.name[..len as usize] == bytes)
}

pub fn unregister_endpoint(name: &str, endpoint_id: EndpointId) {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_MAX { return; }
    let len = bytes.len() as u8;
    let mut names = NAMES.lock_irq();
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

    let names = NAMES.lock_irq();
    names.iter().find(|e| {
        e.valid && e.name_len == len && &e.name[..len as usize] == bytes
    }).map(|e| e.endpoint_id)
}
