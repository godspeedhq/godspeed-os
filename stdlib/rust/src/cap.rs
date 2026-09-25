// SPDX-License-Identifier: Apache-2.0
//! Capabilities: the rights a capability carries, and the operations on one.
//!
//! # What a capability is here
//!
//! An unforgeable token naming a resource, the rights you hold over it, and the generation it was
//! minted at (CLAUDE.md 7.2). Holding one is necessary and sufficient authority for what it permits.
//! There is no other way to act: no ambient authority, no privilege inherited from who you are.
//!
//! # What this module is, and what it replaced
//!
//! `gs::cap` used to be the FILE module - 256 lines whose doc opened "a file as a capability" and
//! which held [`File`](crate::file::File) and nothing else. So a program needing to acquire, derive
//! or drop an ordinary capability found nothing here and reached into `godspeed_sdk`, which is the
//! layer a program is not supposed to need. The file half now lives in [`crate::file`].
//!
//! The rights constants stayed, because they were never file-specific: [`READ`] and [`WRITE`] are
//! capability rights (CLAUDE.md 7.4), and `cap::READ` at an `fs.open` call site reads correctly.
//!
//! # The three properties worth knowing before you use one
//!
//! **Rights only narrow.** A copy never carries more than its source, and there is no path back up.
//! [`duplicate`] makes an identical copy to hand away; narrowing happens where a capability is
//! minted or granted (CLAUDE.md 7.3), not on the copy.
//!
//! **A capability can go stale.** Every one carries a generation, and the resource bumps its own when
//! it dies or is replaced. The next use of a stale capability fails with
//! [`Error::EndpointDead`](crate::Error) or `CapRevoked` rather than reaching the new instance. That
//! is not a fault to route around: it is the system telling you the thing you held is gone, and
//! [`reacquire_by_name`] is how you answer it.
//!
//! **Transfer MOVES.** Sending a capability with [`send_granting`](crate::ipc) removes it from your
//! table. If the send fails it stayed, and it is yours to reclaim; if it succeeded it is not yours
//! any more. Either way the outcome of the send decides what you may do next, so it is never
//! ignorable (CLAUDE.md 8.5).

use godspeed_sdk::capability::CapHandle;
use godspeed_sdk::service_context::ServiceContext;

use crate::error::Error;

/// Read the file's contents.
pub const READ: u8 = 1 << 0;

/// Write the file's contents.
pub const WRITE: u8 = 1 << 1;

/// Write only PAST the end of what is already there: an append-only capability.
///
/// Enforced by `fs` against the file's size at the moment of the write, so a holder cannot read the
/// size, decide to overwrite, and send the old offset.
pub const APPEND: u8 = 1 << 6;

/// Transfer this capability onward (CLAUDE.md 7.4).
///
/// A capability handed out WITHOUT this right cannot be passed on by its holder. Grant it only when
/// the receiver is meant to re-delegate: rights narrow on transfer and never widen, so the floor you
/// set here is the floor for every copy that follows.
pub const GRANT: u8 = 1 << 4;

/// A capability this service holds, by the slot the kernel put it in.
///
/// Thin on purpose: the value is an index into this task's capability table, and the kernel is what
/// gives it meaning. Wrapping it in a type stops it being confused with the resource ids, endpoint
/// ids and file descriptors it is NOT.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cap(pub(crate) CapHandle);

impl Cap {
    /// The raw slot, for the rare call that still needs the SDK.
    ///
    /// Present so that reaching the lower layer is a visible, deliberate step rather than a reason to
    /// abandon this module. If you find yourself calling this often, the missing thing belongs here.
    pub fn handle(self) -> CapHandle {
        self.0
    }
}

/// Acquire a SEND capability to a service, by name, from the kernel's name directory.
///
/// This is how a client finds a service it was not wired to at spawn, and how it finds the NEW
/// instance after one died (CLAUDE.md 14.2). It is gated: a service that was not granted the
/// authority to acquire by name gets nothing, which is what stops name resolution being an ambient
/// back door around the contract.
pub fn acquire(ctx: &ServiceContext, name: &str) -> Result<Cap, Error> {
    match ctx.acquire_send_cap(name) {
        Some(h) => Ok(Cap(h)),
        None => Err(Error::NotFound),
    }
}

/// Acquire a SEND capability that may itself be passed on ([`GRANT`]).
///
/// Separate from [`acquire`] rather than a flag on it, because handing out a re-delegatable
/// capability is a different decision from using one and should read differently at the call site.
pub fn acquire_grantable(ctx: &ServiceContext, name: &str) -> Result<Cap, Error> {
    match ctx.acquire_send_grant_cap(name) {
        Some(h) => Ok(Cap(h)),
        None => Err(Error::NotFound),
    }
}

/// Re-acquire a service by name after its previous instance died.
///
/// The answer to a [`Error::EndpointDead`](crate::Error): the name is stable, the instance is not
/// (CLAUDE.md invariant 11). Returns `true` if a fresh capability was installed.
///
/// **Re-acquiring the endpoint is necessary and not sufficient.** Anything you derived from the DEAD
/// instance - an open file, a socket, a connection id, a cached copy of its state - was issued by a
/// service that no longer exists and the new one has never heard of. Re-establish those too, or you
/// are holding a handle the new instance will refuse (CLAUDE.md 14.3).
pub fn reacquire(ctx: &ServiceContext, name: &str) -> bool {
    ctx.reacquire_by_name(name)
}

/// Duplicate a capability you hold, to hand the copy away while keeping the original.
///
/// The copy carries the SAME resource, generation and rights - this does not narrow, and there is no
/// rights argument because the kernel's `DeriveCap` does not take one. Narrowing is real (CLAUDE.md
/// 7.3) and happens where a capability is minted or granted, not here.
///
/// Requires the source to hold [`GRANT`]: a capability you may not pass on is also one you may not
/// copy for passing on. Fails if it lacks GRANT, has gone stale, or the table is full - and those
/// three are not distinguished, which is a limitation of the syscall rather than of this wrapper.
pub fn duplicate(ctx: &ServiceContext, cap: Cap) -> Result<Cap, Error> {
    match ctx.derive_cap(cap.0) {
        Some(h) => Ok(Cap(h)),
        None => Err(Error::PermissionDenied),
    }
}

/// Drop a capability from this service's table.
///
/// Authority you no longer need is authority you should not hold. This is also how a table slot is
/// returned after a transfer failed and left the capability with you (CLAUDE.md 8.5).
pub fn remove(ctx: &ServiceContext, cap: Cap) {
    ctx.remove_cap(cap.0);
}

/// A grantable capability to THIS service's own endpoint, to hand to someone who must reply.
///
/// The shape behind request/reply: the caller sends one of these so the replier has somewhere to
/// answer, and the kernel wakes the caller with `ReplyDead` if the replier dies holding it, so a
/// reply that will never come is reported rather than waited for (CLAUDE.md 8.6).
pub fn self_grant(ctx: &ServiceContext) -> Result<Cap, Error> {
    match ctx.self_grant_handle() {
        Some(h) => Ok(Cap(h)),
        None => Err(Error::PermissionDenied),
    }
}
