// SPDX-License-Identifier: Apache-2.0
//! Files, without the byte layout.
//!
//! # What this replaces
//!
//! Reading a file today means knowing the wire protocol. From `services/shell`, abbreviated:
//!
//! ```text
//!   let stat = fs_request_q(ctx, OP_STAT_FILE, path, &[]);   // three outcome arms
//!   let sp = stat.payload_bytes();
//!   let exists = sp.first() == Some(&FS_OK) && sp.len() >= 11 && sp[1] == 1;
//!   let is_dir = exists && sp[10] == 1;
//!   let size = u64::from_le_bytes([sp[2], sp[3], sp[4], sp[5], sp[6], sp[7], sp[8], sp[9]]);
//!   // then loop read_at in IO_CHUNK pieces, checking errors at each step
//! ```
//!
//! Roughly thirty lines, every one an opportunity to index the wrong byte. `FS_OK` was declared
//! independently in four crates, and the shell re-declared 21 of the filesystem's 27 opcodes. A
//! guessed opcode in one of them (`1` where the protocol says `10`) made a command write nothing
//! for twelve seconds and report that it was done.
//!
//! **The layout is owned here, once.** It is a fact `services/fs` defines; every other crate was
//! re-deriving it from a comment.
//!
//! # There is no `read_to_string`
//!
//! GodspeedOS has no heap, deliberately (CLAUDE.md §26.6.1: fixed stack arrays and bounded arenas,
//! so the maximum a subsystem can use is readable from its source). A function returning an
//! allocated `String` cannot exist, and adding an allocator to make this library look like Rust's
//! would be trading the architecture for a familiar shape.
//!
//! So the caller owns the buffer:
//!
//! ```ignore
//! let mut buf = [0u8; 4096];
//! let n = fs::read_into(&fs, "/data/hello.txt", &mut buf)?;
//! io::println(ctx, core::str::from_utf8(&buf[..n]).unwrap_or("<not utf-8>"));
//! ```
//!
//! Six lines against thirty, no byte indices, and the bound is visible in the source.

use godspeed_sdk::ipc::Message;
use godspeed_sdk::service_context::ServiceContext;

use crate::call;
use crate::error::{from_fs_status, Error};

/// The longest path `services/fs` accepts. A longer one is [`Error::InvalidInput`] and is never
/// sent, rather than being silently truncated into a request for a DIFFERENT file.
pub const PATH_MAX: usize = 120;

/// The most file content one request can carry, as `services/fs` frames it (7 * 508). Reads and
/// writes larger than this are split by [`read_into`] and [`write`]; it is public because a caller
/// sizing its own buffer benefits from knowing the natural stride.
pub const IO_CHUNK: usize = 7 * 508;

// The opcodes, owned here. These are `services/fs`'s numbers and must not be guessed.
const OP_WRITE_FILE: u8 = 10;
const OP_STAT_FILE: u8 = 12;
const OP_MKDIR: u8 = 13;
const OP_DELETE: u8 = 16;
const OP_RENAME: u8 = 15;
const OP_WRITE_NEW: u8 = 24;
const OP_WRITE_AT: u8 = 25;
const OP_READ_AT: u8 = 26;

/// The first byte of every request and of its reply, so a late answer to an EARLIER request is
/// recognised instead of being read as the answer to this one.
///
/// This is not decoration. Without it, running one command twice could leave the filesystem
/// protocol "out of step" - a reply arriving after its deadline was matched to the next request,
/// and every exchange after it was answering the question before.
const TAG_BASE: u8 = 0xC0;

/// A handle to the filesystem service.
///
/// # Authority
///
/// Constructed from a `&ServiceContext`, and carries no authority of its own. Holding one does not
/// grant filesystem access: every call goes through the context's existing send capability for
/// `fs`, and a task whose contract never asked for it gets [`Error::Unreachable`], exactly as it
/// would for any other peer it cannot reach. **This handle makes the filesystem convenient to use,
/// never available where it was not granted.**
///
/// # Why it owns a counter
///
/// The request tag has to differ between consecutive requests, and it may not live in a `static`:
/// unowned global mutable state is forbidden (invariant 9), and the shell's own tag counter was
/// moved out of a `static AtomicU8` for exactly that reason (audit finding C6-1). So the counter
/// lives here, owned by the handle, which is also why this type takes `&mut self` on operations
/// that send.
pub struct Fs<'a> {
    ctx: &'a ServiceContext,
    tag: u8,
}

/// What [`stat`] found.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Stat {
    /// Size in bytes. Zero for a directory.
    pub size: u64,
    /// `true` if this path is a directory rather than a file.
    pub is_dir: bool,
}

impl<'a> Fs<'a> {
    /// Take a filesystem handle from a context.
    ///
    /// Cheap, allocates nothing, and grants nothing. See the type's authority note.
    pub fn new(ctx: &'a ServiceContext) -> Fs<'a> {
        Fs { ctx, tag: TAG_BASE }
    }

    /// The next request tag. Wraps within the tag band, which is fine: it only has to differ from
    /// the request immediately before it.
    fn next_tag(&mut self) -> u8 {
        self.tag = TAG_BASE.wrapping_add(self.tag.wrapping_sub(TAG_BASE).wrapping_add(1) & 0x3F);
        self.tag
    }

    /// Build `[tag, op, path_len, path.., tail..]` and send it.
    ///
    /// Returns the reply body with the tag and status already checked and stripped, so callers work
    /// in terms of the operation's own answer rather than in terms of offsets.
    fn call(&mut self, op: u8, path: &[u8], tail: &[u8], secs: i64) -> Result<Reply, Error> {
        if path.len() > PATH_MAX {
            return Err(Error::InvalidInput);
        }
        let mut req = [0u8; 3 + PATH_MAX + 8 + IO_CHUNK];
        let n = 3 + path.len() + tail.len();
        if n > req.len() {
            // Could not be built, so it was never sent: a failure, not a slow answer.
            return Err(Error::InvalidInput);
        }
        let tag = self.next_tag();
        req[0] = tag;
        req[1] = op;
        req[2] = path.len() as u8;
        req[3..3 + path.len()].copy_from_slice(path);
        req[3 + path.len()..n].copy_from_slice(tail);

        let reply = call::request_within(self.ctx, "fs", &Message::from_bytes(&req[..n]), secs)?;
        let body = reply.payload_bytes();

        // The tag must match, or this is an answer to a question we already gave up on.
        if body.first() != Some(&tag) {
            return Err(Error::Malformed);
        }
        let status = *body.get(1).ok_or(Error::Malformed)?;
        from_fs_status(status)?;
        Ok(Reply { msg: reply })
    }

    /// Ask whether a path exists, and what it is.
    ///
    /// **Blocks** up to [`call::DEFAULT_SECS`]. **Authority:** the caller's existing `fs` capability.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the path is absent. See [`Error`] for the no-answer cases; a `stat` is
    /// read-only, so retrying any of them is safe.
    pub fn stat(&mut self, path: &str) -> Result<Stat, Error> {
        let r = self.call(OP_STAT_FILE, path.as_bytes(), &[], call::DEFAULT_SECS)?;
        let b = r.body();
        // [status, exists, size:u64, is_dir] after the tag - 11 bytes from the tag inclusive.
        if b.len() < 10 {
            return Err(Error::Malformed);
        }
        if b[0] != 1 {
            return Err(Error::NotFound);
        }
        let size = u64::from_le_bytes([b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8]]);
        Ok(Stat { size, is_dir: b[9] == 1 })
    }

    /// Read a whole file into `buf`, returning how many bytes were written.
    ///
    /// Streams in [`IO_CHUNK`] pieces, so a file far larger than one IPC message reads correctly.
    ///
    /// **Blocks**, once per chunk. **Authority:** the caller's existing `fs` capability.
    ///
    /// # Errors
    /// - [`Error::NotFound`] - no such file, or it is a directory.
    /// - [`Error::BufferTooSmall`] - the file does not fit. **Nothing is written**; call again with
    ///   room, having learned the size from [`stat`].
    /// - A read is idempotent, so every no-answer error here may safely be retried.
    pub fn read_into(&mut self, path: &str, buf: &mut [u8]) -> Result<usize, Error> {
        let st = self.stat(path)?;
        if st.is_dir {
            return Err(Error::NotFound);
        }
        let size = st.size as usize;
        if size > buf.len() {
            return Err(Error::BufferTooSmall);
        }
        let mut off = 0usize;
        while off < size {
            let want = (size - off).min(IO_CHUNK);
            let mut tail = [0u8; 12];
            tail[..8].copy_from_slice(&(off as u64).to_le_bytes());
            tail[8..].copy_from_slice(&(want as u32).to_le_bytes());
            let r = self.call(OP_READ_AT, path.as_bytes(), &tail, call::DEFAULT_SECS)?;
            let b = r.body();
            if b.len() < 4 {
                return Err(Error::Malformed);
            }
            let n = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
            if n == 0 || n > want || b.len() < 4 + n {
                return Err(Error::Malformed);
            }
            buf[off..off + n].copy_from_slice(&b[4..4 + n]);
            off += n;
        }
        Ok(off)
    }

    /// Create or replace a file with `data`.
    ///
    /// **Blocks**. **Authority:** the caller's existing `fs` capability.
    ///
    /// # This operation CHANGES STATE
    ///
    /// If it returns [`Error::OutcomeUnknown`], the write may have happened. Do not call it again
    /// to "make sure": read the file back, or report the uncertainty. [`Error::retry_is_safe`]
    /// answers this for you, and says `false` for that case on purpose.
    pub fn write(&mut self, path: &str, data: &[u8]) -> Result<(), Error> {
        if data.len() <= IO_CHUNK {
            self.call(OP_WRITE_FILE, path.as_bytes(), data, call::DEFAULT_SECS)?;
            return Ok(());
        }
        // Larger than one message: create it, then fill it positionally. `WRITE_AT` at a fixed
        // offset is one of the two operations `services/fs` documents as positionally idempotent,
        // which is what makes a chunked write safe to resume at all.
        self.call(OP_WRITE_FILE, path.as_bytes(), &data[..IO_CHUNK], call::DEFAULT_SECS)?;
        let mut off = IO_CHUNK;
        while off < data.len() {
            let n = (data.len() - off).min(IO_CHUNK);
            let mut tail = [0u8; 8 + IO_CHUNK];
            tail[..8].copy_from_slice(&(off as u64).to_le_bytes());
            tail[8..8 + n].copy_from_slice(&data[off..off + n]);
            self.call(OP_WRITE_AT, path.as_bytes(), &tail[..8 + n], call::DEFAULT_SECS)?;
            off += n;
        }
        Ok(())
    }

    /// Create a directory. Fails if the parent does not exist.
    ///
    /// **Blocks**. Changes state: see the note on [`write`] about [`Error::OutcomeUnknown`].
    pub fn create_dir(&mut self, path: &str) -> Result<(), Error> {
        self.call(OP_MKDIR, path.as_bytes(), &[], call::DEFAULT_SECS)?;
        Ok(())
    }

    /// Delete one file or one empty directory.
    ///
    /// **Blocks**. **Destructive, and not idempotent in the way that matters**: on
    /// [`Error::OutcomeUnknown`] the file may already be gone, and a second delete would report
    /// `NotFound` for work that succeeded. Report the uncertainty; do not re-send.
    pub fn delete(&mut self, path: &str) -> Result<(), Error> {
        self.call(OP_DELETE, path.as_bytes(), &[], call::DEFAULT_SECS)?;
        Ok(())
    }

    /// Allocate a file of `capacity` bytes without writing content into it.
    ///
    /// The extent is reserved up front, so later [`write_at`] calls land in space that is already
    /// the file's. That is what makes a long append-style writer bounded: it cannot run out of room
    /// halfway and leave a half-file behind.
    ///
    /// **Blocks. Changes state**: on [`Error::OutcomeUnknown`] the file may exist. Use [`exists`] to
    /// find out rather than calling this again, which would fail differently depending on timing.
    ///
    /// Added because `services/recorder` needed it during migration. It is a real filesystem
    /// operation, so it belongs in the typed surface rather than behind an opcode escape hatch.
    pub fn create_sized(&mut self, path: &str, capacity: u64) -> Result<(), Error> {
        self.call(OP_WRITE_NEW, path.as_bytes(), &capacity.to_le_bytes(), call::DEFAULT_SECS)?;
        Ok(())
    }

    /// Write `data` at a byte `offset` into an existing file.
    ///
    /// **Blocks. Changes state.** Unusually among the write operations, a positional write at a
    /// FIXED offset into an already-allocated extent is idempotent: repeating it puts the same bytes
    /// in the same place. `services/fs` names it as one of exactly two operations safe to re-send,
    /// which is what makes a chunked write resumable at all.
    ///
    /// Even so this returns [`Error::OutcomeUnknown`] honestly on a timeout, because the DECISION to
    /// re-send belongs to the caller who knows the offset is fixed - not to a library that would be
    /// guessing.
    pub fn write_at(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), Error> {
        if data.len() > IO_CHUNK {
            return Err(Error::InvalidInput);
        }
        let mut tail = [0u8; 8 + IO_CHUNK];
        tail[..8].copy_from_slice(&offset.to_le_bytes());
        tail[8..8 + data.len()].copy_from_slice(data);
        self.call(OP_WRITE_AT, path.as_bytes(), &tail[..8 + data.len()], call::DEFAULT_SECS)?;
        Ok(())
    }

    /// Rename a file within its directory. `new_name` is a bare name, not a path.
    ///
    /// **Blocks. Changes state, and is NOT idempotent**: a second rename after a successful one
    /// fails with [`Error::NotFound`], because the source is already gone. On
    /// [`Error::OutcomeUnknown`] check with [`exists`] rather than re-sending - this is precisely
    /// the case where a retry reports failure for work that succeeded.
    pub fn rename(&mut self, path: &str, new_name: &str) -> Result<(), Error> {
        self.call(OP_RENAME, path.as_bytes(), new_name.as_bytes(), call::DEFAULT_SECS)?;
        Ok(())
    }

    /// Does this path exist? A convenience over [`stat`], and read-only.
    pub fn exists(&mut self, path: &str) -> Result<bool, Error> {
        match self.stat(path) {
            Ok(_) => Ok(true),
            Err(Error::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// A checked reply: the tag matched and the status said OK.
struct Reply {
    msg: Message,
}

impl Reply {
    /// Everything after the tag and the status byte.
    fn body(&self) -> &[u8] {
        let b = self.msg.payload_bytes();
        if b.len() > 2 { &b[2..] } else { &[] }
    }
}
