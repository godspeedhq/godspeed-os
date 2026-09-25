// SPDX-License-Identifier: Apache-2.0
//! A file as a capability, safely, from a service that serves other clients.
//!
//! # What this is
//!
//! [`Fs::open`](crate::fs::Fs::open) asks `fs` for a **delegated resource capability** (CLAUDE.md
//! 7.10) to one file, and returns a [`File`]. The capability is real and kernel-minted: unforgeable,
//! revocable, and non-escalating. A READ-only [`File`] cannot write, and the refusal comes from the
//! kernel before `fs` is ever reached.
//!
//! ```ignore
//! let mut fs = gs::fs::Fs::new(ctx);
//! let mut f = fs.open("/data/log.txt", cap::READ | cap::WRITE)?;
//! f.write_at(0, b"hello")?;
//! let mut buf = [0u8; 64];
//! let n = f.read_at(0, &mut buf)?;
//! f.close()?;
//! ```
//!
//! # Why this could not be written until the protocol carried a tag
//!
//! Using a resource capability is `resource_invoke` - a SEND that embeds a one-shot reply cap. The
//! kernel routes it to the owning service and then forgets the exchange, so the caller must wait for
//! the answer on **its own ordinary endpoint**, the same endpoint its clients send to. A task owns
//! exactly one endpoint; there is no second one to wait on.
//!
//! That leaves a caller two bad choices, and `services/shell` takes the second:
//!
//! - block on a plain `recv` and read whatever arrives as the reply, or
//! - drain the endpoint first, destroying any client request that was queued.
//!
//! The shell gets away with draining because the shell serves nobody. **A library cannot make that
//! assumption**, which is why this module did not exist for as long as the file-cap protocol carried
//! no correlation tag.
//!
//! It carries one now, matching the convention the NAMED `fs` protocol in the same service always
//! had. With a tag, a message that is not the reply is *recognisable*, and a caller that receives one
//! can **hold it and hand it back** instead of losing it. That is what [`File::take_held`] is for,
//! and it is the whole reason this is safe to hand to a service.
//!
//! **Nothing here is a new kernel facility.** The tag is a protocol byte and the holding is a fixed
//! array; the standard library is re-serving what the system already does, which is the only thing it
//! is allowed to do.
//!
//! # The obligation this puts on a serving caller
//!
//! If your task answers clients on its endpoint, **you must drain [`File::take_held`] after any
//! operation** and feed those messages back into your own loop. They are real client requests that
//! arrived while you were waiting. Ignoring them loses them just as surely as draining would have -
//! the difference is that here you are told, and there you were not.
//!
//! A program that serves nobody (most programs) can ignore all of this: nothing will ever be held.

use godspeed_sdk::capability::CapHandle;
use godspeed_sdk::ipc::Message;
use godspeed_sdk::service_context::ServiceContext;

use crate::call;
use crate::error::{from_fs_status, Error};
use crate::fs::Fs;
use crate::resource::{self, Held};

/// Read the file's contents.
pub const READ: u8 = 1 << 0;
/// Write the file's contents.
pub const WRITE: u8 = 1 << 1;
/// Write only PAST the end of what is already there: an append-only capability.
///
/// Enforced by `fs` against the file's size at the moment of the write, so a holder cannot read the
/// size, decide to overwrite, and send the old offset.
pub const APPEND: u8 = 1 << 6;

// The file-cap operations, as `services/fs` numbers them.
const FOP_READ: u8 = 1;
const FOP_WRITE: u8 = 2;
const FOP_STAT: u8 = 3;
const FOP_CLOSE: u8 = 4;

/// The most bytes one invocation moves, as `services/fs` frames it.
pub const IO_CHUNK: usize = crate::fs::IO_CHUNK;

/// How many messages that are NOT our reply a [`File`] will hold before it stops taking them.
///
/// See [`File::take_held`]. The bound and the reason for it live in the shared resource-invocation
/// module, because sockets need exactly the same thing.
pub const HELD_MAX: usize = resource::HELD_MAX;

/// An open file, held as a capability.
///
/// # Why it borrows the [`Fs`] handle
///
/// A [`File`] and its `Fs` speak to the same service on the same endpoint, so they must share ONE
/// correlation tag counter - two counters can mint the same tag for two exchanges in flight, and the
/// result is not a loud rejection but a stale reply silently accepted as the current answer. Holding
/// `&mut Fs` makes that structural: the borrow checker will not let the two be used at once, so
/// there is no way to spell the bug.
///
/// The cost is that one `Fs` handle opens one file at a time. To hold two, thread the counter
/// yourself with [`Fs::from_tag`](crate::fs::Fs::from_tag) and [`Fs::tag`](crate::fs::Fs::tag).
pub struct File<'f, 'a: 'f> {
    fs: &'f mut Fs<'a>,
    ctx: &'a ServiceContext,
    cap: CapHandle,
    right: u8,
    /// Messages that arrived while we were waiting and are NOT ours. Never dropped.
    held: Held,
    closed: bool,
}

impl<'f, 'a: 'f> File<'f, 'a> {
    pub(crate) fn new(fs: &'f mut Fs<'a>, ctx: &'a ServiceContext, cap: CapHandle, right: u8) -> Self {
        File { fs, ctx, cap, right, held: Held::new(), closed: false }
    }

    /// The rights this capability actually carries.
    ///
    /// May be NARROWER than you asked for: `fs` refuses a writable capability to a sealed file and
    /// hands back a read-only one rather than minting a cap it cannot honour (7.3 - rights narrow).
    /// Check this rather than assuming the open succeeded on your terms.
    pub fn rights(&self) -> u8 {
        self.right
    }

    /// Take one message that arrived during an operation and was NOT the reply.
    ///
    /// Call this in a loop until it returns `None` after every operation, if your task serves
    /// clients. These are real requests from them; the library held them rather than dropping them,
    /// but only you can answer them.
    ///
    /// Returns `None` for a task that serves nobody, always.
    pub fn take_held(&mut self) -> Option<Message> {
        self.held.take()
    }

    /// Read from the file through the capability.
    ///
    /// Returns how many bytes landed in `buf`, which may be fewer than asked for at end of file.
    /// One invocation moves at most [`IO_CHUNK`] bytes; call again with a later offset for more.
    ///
    /// **Blocks** up to [`call::DEFAULT_SECS`]. **Authority:** this capability's `READ` right, which
    /// the KERNEL checks before `fs` is reached.
    ///
    /// # Errors
    /// - [`Error::PermissionDenied`] - this capability does not carry `READ`.
    /// - [`Error::NotFound`] - the file was deleted; the capability has been revoked.
    /// - A read changes nothing, so every no-answer error here is safe to retry.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let want = buf.len().min(IO_CHUNK);
        let mut req = [0u8; 13];
        req[0] = FOP_READ;
        req[1..9].copy_from_slice(&offset.to_le_bytes());
        req[9..13].copy_from_slice(&(want as u32).to_le_bytes());
        let reply = self.invoke(READ, &req)?;
        let b = reply.payload_bytes();
        // `[tag, status, n:u32, bytes..]`. `invoke` VERIFIES the tag and leaves it in place - it does
        // not strip it, because stripping means rebuilding a 4 KiB `Message` on the stack for every
        // read. So the body starts at 2, not at 1.
        if b.len() < 6 {
            return Err(Error::Malformed);
        }
        let n = u32::from_le_bytes([b[2], b[3], b[4], b[5]]) as usize;
        if n > want || 6 + n > b.len() {
            return Err(Error::Malformed);
        }
        buf[..n].copy_from_slice(&b[6..6 + n]);
        Ok(n)
    }

    /// Write to the file through the capability.
    ///
    /// **Blocks** up to [`call::DEFAULT_SECS`]. **Authority:** this capability's `WRITE` right.
    ///
    /// # Errors
    /// - [`Error::PermissionDenied`] - no `WRITE`, or an `APPEND`-only capability was asked to write
    ///   back over bytes it had already written.
    /// - [`Error::InvalidInput`] - more than [`IO_CHUNK`] bytes in one call. **Nothing is written**;
    ///   split it rather than assuming a partial write happened.
    /// - **A write MUTATES.** Do not retry on [`Error::OutcomeUnknown`] without first reading back
    ///   what is actually there - see [`Error::retry_is_safe`].
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), Error> {
        if data.len() > IO_CHUNK {
            return Err(Error::InvalidInput);
        }
        let mut req = [0u8; 9 + IO_CHUNK];
        req[0] = FOP_WRITE;
        req[1..9].copy_from_slice(&offset.to_le_bytes());
        req[9..9 + data.len()].copy_from_slice(data);
        self.invoke(WRITE, &req[..9 + data.len()])?;
        Ok(())
    }

    /// The file's current size in bytes.
    ///
    /// **Blocks** up to [`call::DEFAULT_SECS`]. **Authority:** this capability's `READ` right.
    pub fn size(&mut self) -> Result<u64, Error> {
        let reply = self.invoke(READ, &[FOP_STAT])?;
        let b = reply.payload_bytes();
        // `[tag, status, size:u64]` - the body starts at 2, as in `read_at`.
        if b.len() < 10 {
            return Err(Error::Malformed);
        }
        Ok(u64::from_le_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]))
    }

    /// Close the file, revoking this capability and every copy of it.
    ///
    /// Consumes the handle, and returns what the service said. Dropping a `File` without calling
    /// this also closes it, but a `Drop` cannot report a failure - so close explicitly wherever the
    /// outcome matters.
    pub fn close(mut self) -> Result<(), Error> {
        self.close_inner()
    }

    fn close_inner(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        // CLOSE is permitted to any holder, so it is invoked under whatever right we hold rather
        // than under WRITE - a read-only holder must still be able to let go.
        let r = self.invoke(self.right, &[FOP_CLOSE]).map(|_| ());
        self.ctx.remove_cap(self.cap);
        r
    }

    /// One invocation, through the shared resource-capability path, then the fs status byte.
    ///
    /// The generic half - reply-cap lifetime, holding a message that is not ours, reading the
    /// kernel's refusal - is `crate::resource`. What is specific to a file is the status byte at
    /// index 1 of the reply, which this maps to an [`Error`].
    fn invoke(&mut self, right: u8, body: &[u8]) -> Result<Message, Error> {
        let tag = self.fs.next_tag_pub();
        // NO PATIENCE BYTE. `fs` answers a file-capability invocation from its own serve loop and
        // never puts one aside, so the byte would be carried and never read - and `serve_filecap`
        // reads the operation at index 1 of what it receives.
        let m = resource::invoke(self.ctx, self.cap, right, tag, None, body, call::DEFAULT_SECS,
                                 &mut self.held)?;
        // `[tag, status, ..]`. The tag is verified and LEFT IN PLACE, so a body starts at index 2.
        let status = *m.payload_bytes().get(1).ok_or(Error::Malformed)?;
        from_fs_status(status)?;
        Ok(m)
    }
}

impl<'f, 'a: 'f> Drop for File<'f, 'a> {
    /// Closes the file if [`close`](File::close) was not called.
    ///
    /// `fs` keeps a FIXED table of open resources, so leaking one is not merely untidy - enough
    /// leaks and nothing can be opened at all. That is why this closes rather than merely warning.
    ///
    /// A `Drop` cannot return a `Result`, so a close that FAILS here is invisible. That is the
    /// reason [`close`](File::close) exists and is worth calling wherever the outcome matters.
    fn drop(&mut self) {
        let _ = self.close_inner();
    }
}
