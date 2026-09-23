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
/// writes larger than this are split by [`read_into`](Fs::read_into) and [`write`]; it is public because a caller
/// sizing its own buffer benefits from knowing the natural stride.
pub const IO_CHUNK: usize = 7 * 508;

// The opcodes, owned here. These are `services/fs`'s numbers and must not be guessed.
const OP_WRITE_FILE: u8 = 10;
const OP_LIST_DIR: u8 = 14;
const OP_MOVE: u8 = 17;
const OP_MKDIR_P: u8 = 18;
const OP_DELETE_TREE: u8 = 19;
const OP_STAT_FILE: u8 = 12;
const OP_MKDIR: u8 = 13;
const OP_DELETE: u8 = 16;
const OP_RENAME: u8 = 15;
const OP_WRITE_NEW: u8 = 24;
const OP_WRITE_AT: u8 = 25;
const OP_READ_AT: u8 = 26;

/// The tag a fresh handle starts from. Any non-zero value would do; what matters is that
/// consecutive requests on one channel differ.
///
/// The tag is the first byte of every request and of its reply, so a late answer to an EARLIER
/// request is recognised instead of being read as the answer to this one. This is not decoration:
/// without it, running one command twice could leave the filesystem protocol "out of step" - a
/// reply arriving after its deadline matched to the next request, and every exchange after it
/// answering the question before.
///
/// **There is deliberately no separate tag RANGE for this library.** An earlier draft minted tags
/// from a 0xC0..0xFF band, which reads as tidy and is actively harmful: `services/shell` mints
/// from the whole 1..=255 range, so the two overlapped, and a collision does not fail loudly - it
/// lets a stale reply be ACCEPTED as the current request's answer. Two counters on one channel is
/// the bug the tag exists to prevent. Use [`Fs::from_tag`] to share the one counter instead.
const TAG_START: u8 = 1;

/// The most of a service-supplied failure reason [`Fs::reason`] keeps. Bounded on purpose: the
/// handle is a stack value, so its size must be readable from this line (26.6.1).
pub const REASON_MAX: usize = 64;

/// The most pages [`Fs::list_dir`] will request before stopping and saying so.
///
/// The same bound `services/shell` uses. A walk has to terminate on its own rather than on the
/// service being well-behaved (26.6), and 512 pages is far past any directory this filesystem
/// holds while still being a number rather than "however many it takes".
pub const DIR_PAGE_MAX: u16 = 512;

/// What [`Fs::list_dir`] did.
///
/// `visited` cannot be read without seeing `complete`, and that is the whole point of it being a
/// struct: a partial listing that reads as a full one is the failure this type exists to prevent.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Listing {
    /// How many entries the closure was shown.
    pub visited: usize,
    /// `true` if the directory ended. `false` means the walk stopped first - the closure returned
    /// `false`, or [`DIR_PAGE_MAX`] was reached - and there are MORE ENTRIES THAN YOU SAW. Say so
    /// in whatever you report; do not present the count as the size of the directory.
    pub complete: bool,
}

/// One entry from [`Fs::list_dir`].
///
/// `name` is BYTES, not `&str`, and that is not laziness. `services/fs` accepts any byte above
/// 0x1f except `/` and 0x7f in a name, so a name is not guaranteed to be UTF-8 and a type that
/// promised otherwise would be lying about the filesystem it reads. [`DirEntry::name_str`] is the
/// convenience for the ordinary case.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct DirEntry<'n> {
    /// The entry's name within its directory. Never contains a `/`.
    pub name: &'n [u8],
    /// `true` if this entry is itself a directory.
    pub is_dir: bool,
    /// Size in bytes. Zero for a directory.
    pub size: u64,
    /// Last-modified time as `services/fs` records it.
    pub mtime: u32,
    /// `true` if the file is sealed (append-only; writes to it are refused).
    pub sealed: bool,
}

impl<'n> DirEntry<'n> {
    /// The name as text, or `None` if it is not UTF-8.
    ///
    /// The honest call site is `e.name_str().unwrap_or("<not utf-8>")` - print something rather
    /// than skipping a file that really exists.
    pub fn name_str(&self) -> Option<&'n str> {
        core::str::from_utf8(self.name).ok()
    }
}

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
    reason: [u8; REASON_MAX],
    reason_len: u8,
    notice: Option<&'a dyn Fn()>,
}

/// What [`stat`](Fs::stat) found.
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
        Fs { ctx, tag: TAG_START, reason: [0; REASON_MAX], reason_len: 0, notice: None }
    }

    /// A handle that calls `notice` when a request has been waiting a while.
    ///
    /// The callback takes nothing and returns nothing, and that is the whole of its contract: it
    /// means only **"you have been waiting"**. It cannot cancel, cannot inspect the request, and
    /// the library learns nothing about consoles from it - the shell prints `[q] quit` there, and
    /// something headless prints nothing at all.
    ///
    /// **Use this for anything a person is waiting at.** A filesystem call can block on a device
    /// that is settling after a replug, and a command that goes quiet for twenty seconds with no
    /// way out is one the operator has to reboot out of (`utilities/0_conventions.md` rule 9). A
    /// request the operator then aborts returns [`Error::Cancelled`], which is NOT a fault - report
    /// it as the deliberate act it was.
    pub fn with_notice(ctx: &'a ServiceContext, notice: &'a dyn Fn()) -> Fs<'a> {
        Fs { ctx, tag: TAG_START, reason: [0; REASON_MAX], reason_len: 0, notice: Some(notice) }
    }

    /// Take a handle that CONTINUES an existing tag sequence for this channel.
    ///
    /// For a caller that still makes some `fs` requests by hand: lend the handle your counter, and
    /// take it back with [`tag`](Fs::tag) when you are done.
    ///
    /// ```ignore
    /// let mut fs = Fs::from_tag(ctx, my_fs_tag.get());
    /// let r = fs.list_dir("/data", |e| { .. })?;
    /// my_fs_tag.set(fs.tag());          // hand-rolled calls resume where the handle left off
    /// ```
    ///
    /// **Use this whenever anything else in the same task also talks to `fs` directly.** Two
    /// counters on one channel can mint the same tag for two in-flight requests, and the result is
    /// not a loud rejection - it is a stale reply silently accepted as the answer to the current
    /// question. See [`TAG_START`](self).
    pub fn from_tag(ctx: &'a ServiceContext, tag: u8) -> Fs<'a> {
        Fs { ctx, tag, reason: [0; REASON_MAX], reason_len: 0, notice: None }
    }

    /// Lend this handle a waiting-notice, as [`with_notice`](Fs::with_notice) describes.
    ///
    /// Separate from [`from_tag`](Fs::from_tag) rather than a fourth constructor taking both,
    /// because a caller that shares a tag counter usually also wants the notice and the
    /// combinations multiply faster than the constructors are worth.
    pub fn noticing(mut self, notice: &'a dyn Fn()) -> Fs<'a> {
        self.notice = Some(notice);
        self
    }

    /// The tag this handle last used, to hand back to whatever owns the channel's counter.
    pub fn tag(&self) -> u8 {
        self.tag
    }

    /// Why the last call failed, IN THE SERVICE'S OWN WORDS, or `""` if it did not fail or said
    /// nothing.
    ///
    /// **This is for reporting, never for control flow.** Branch on the [`Error`]; print this.
    /// It is free-text owned by `services/fs` and may be reworded at any time, so a caller that
    /// matched on its content would break silently the next time somebody improved a message.
    ///
    /// It exists because the service already sends it and this library used to discard it:
    /// `FS_ERR` became [`Error::Failed`] and "name already exists" was lost between the two. An
    /// operator reading `fs: failed` has to go to the service log for a sentence the reply was
    /// already carrying, which makes a loud failure quieter for no reason (26.7).
    pub fn reason(&self) -> &str {
        core::str::from_utf8(&self.reason[..self.reason_len as usize]).unwrap_or("")
    }

    /// The next request tag: wrapping +1, never 0.
    ///
    /// Identical to the rule `services/shell` uses, so one counter can be shared across both (see
    /// [`from_tag`](Fs::from_tag)). Zero is skipped because a zero tag can only have come from a
    /// sender that does not tag at all, and that must stay distinguishable.
    fn next_tag(&mut self) -> u8 {
        let t = self.tag.wrapping_add(1);
        self.tag = if t == 0 { 1 } else { t };
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

        let reply = call::request_within_notice(
            self.ctx, "fs", &Message::from_bytes(&req[..n]), secs, self.notice)?;
        let body = reply.payload_bytes();

        // The tag must match, or this is an answer to a question we already gave up on.
        if body.first() != Some(&tag) {
            return Err(Error::Malformed);
        }
        let status = *body.get(1).ok_or(Error::Malformed)?;
        // Take the service's words BEFORE `from_fs_status` collapses the byte to an `Error`. Only
        // FS_ERR carries a reason; every other status has one meaning and needs no sentence.
        self.reason_len = 0;
        if status == 1 {
            let why = if body.len() > 2 { &body[2..] } else { &[][..] };
            let n = why.len().min(REASON_MAX);
            self.reason[..n].copy_from_slice(&why[..n]);
            self.reason_len = n as u8;
        }
        from_fs_status(status)?;
        Ok(Reply { msg: reply })
    }


    /// Visit every entry of a directory, in the order `services/fs` stores them.
    ///
    /// `f` is called once per entry and returns `true` to continue or `false` to stop early.
    ///
    /// ```ignore
    /// let mut files = 0;
    /// let r = fs.list_dir("/data", |e| { if !e.is_dir { files += 1; } true })?;
    /// if !r.complete { io::println(ctx, "  (listing truncated - there are more)"); }
    /// ```
    ///
    /// # This is where a hand-rolled version goes wrong
    ///
    /// The reply is ONE PAGE: `[count, more, next]` plus the entries that fit in a block. A caller
    /// that reads `count` entries and stops has silently listed a PREFIX of a large directory, and
    /// nothing anywhere reports a problem. This follows `next` until `more` is clear, so the
    /// closure sees the whole directory or the call returns an error - never a quiet partial.
    ///
    /// **Blocks**, once per page. **Authority:** the caller's existing `fs` capability.
    ///
    /// # Errors
    /// - [`Error::NotFound`] - no such path, or it is a file rather than a directory. Returned
    ///   only for the FIRST page: a later page that fails ends the walk with `complete: false`,
    ///   because entries were already delivered and reporting "no such directory" for a read error
    ///   part way through would name the wrong fault.
    /// - [`Error::Malformed`] - the service claimed more entries without advancing. Listing is
    ///   read-only, so every no-answer error here is safe to retry.
    pub fn list_dir<F>(&mut self, path: &str, mut f: F) -> Result<Listing, Error>
    where
        F: FnMut(DirEntry<'_>) -> bool,
    {
        const HDR: usize = 6; // [count, more, next:u32] - the entries follow
        let mut from = 0u32;
        let mut total = 0usize;
        let mut pages = 0u16;
        loop {
            let r = match self.call(OP_LIST_DIR, path.as_bytes(), &from.to_le_bytes(), call::DEFAULT_SECS) {
                Ok(r) => r,
                // The FIRST page failing means the path is not a readable directory, and the caller
                // needs that error. A LATER one failing is a read error inside a walk that already
                // produced real entries - so the honest answer is the entries plus "not complete",
                // not an error that describes the directory rather than what happened.
                Err(e) if pages == 0 => return Err(e),
                Err(_) => return Ok(Listing { visited: total, complete: false }),
            };
            pages += 1;
            let b = r.body();
            if b.len() < HDR {
                return Err(Error::Malformed);
            }
            let count = b[0] as usize;
            let more = b[1] == 1;
            let next = u32::from_le_bytes([b[2], b[3], b[4], b[5]]);

            let mut w = HDR;
            for _ in 0..count {
                // Every field is bounds-checked against the REPLY, not against what the header
                // promised. A truncated or malformed page must not be read past its end.
                if w >= b.len() {
                    return Err(Error::Malformed);
                }
                let nl = b[w] as usize;
                if w + 15 + nl > b.len() {
                    return Err(Error::Malformed);
                }
                let name = &b[w + 1..w + 1 + nl];
                let size = u64::from_le_bytes([
                    b[w + 2 + nl], b[w + 3 + nl], b[w + 4 + nl], b[w + 5 + nl],
                    b[w + 6 + nl], b[w + 7 + nl], b[w + 8 + nl], b[w + 9 + nl],
                ]);
                let mtime = u32::from_le_bytes([
                    b[w + 10 + nl], b[w + 11 + nl], b[w + 12 + nl], b[w + 13 + nl],
                ]);
                total += 1;
                let go_on = f(DirEntry {
                    name,
                    is_dir: b[w + 1 + nl] == 1,
                    size,
                    mtime,
                    sealed: b[w + 14 + nl] & 1 != 0,
                });
                if !go_on {
                    // The caller stopped us, so the directory did not end: NOT complete.
                    return Ok(Listing { visited: total, complete: false });
                }
                w += 15 + nl;
            }

            if !more {
                return Ok(Listing { visited: total, complete: true });
            }
            // THE WALK TERMINATES ON ITS OWN. Following `more` for as long as the service offers it
            // is an unbounded loop wearing a condition; the cap is what makes this bounded (26.6),
            // and `complete: false` is what stops the cap from lying about what was read.
            if pages >= DIR_PAGE_MAX {
                return Ok(Listing { visited: total, complete: false });
            }
            // NO PROGRESS IS A LOUD ERROR, NOT A LOOP. `next` names the entry that did not fit, so
            // it must advance past where this page started. A service that answers "more" without
            // moving would otherwise spin here forever, asking the same question (26.6).
            if next <= from {
                return Err(Error::Malformed);
            }
            from = next;
        }
    }

    /// Create a directory and any missing parents, like `mkdir -p`.
    ///
    /// Succeeds if the directory already exists, which is what makes it usable at start-up: a
    /// service that ensures its own data directory should not have to care whether it is the first
    /// to run. Use [`create_dir`](Fs::create_dir) where the path's absence is itself the thing being asserted.
    ///
    /// **Blocks** up to [`call::DEFAULT_SECS`]. **Authority:** the caller's existing `fs` capability.
    ///
    /// # Errors
    /// [`Error::Failed`] with [`reason`](Fs::reason) naming the step that failed (a parent that is
    /// a file, say). **A no-answer error must NOT be blindly retried** - see [`Error::retry_is_safe`];
    /// creating directories is idempotent, but the transaction may have committed unseen, so
    /// re-issuing is only safe once you accept that it is a second attempt rather than the first.
    pub fn create_dir_all(&mut self, path: &str) -> Result<(), Error> {
        self.call(OP_MKDIR_P, path.as_bytes(), &[], call::DEFAULT_SECS)?;
        Ok(())
    }

    /// Move a file or directory to `dest`, ACROSS directories.
    ///
    /// This is the different operation from [`rename`](Fs::rename), which changes a name within one
    /// parent. `dest` is a full path, and `services/fs` refuses a move that would put a directory
    /// inside itself - the tree stays a tree, and that rule is the service's to enforce, not the
    /// caller's to remember.
    ///
    /// **Blocks** up to [`call::DEFAULT_SECS`]. **Authority:** the caller's existing `fs` capability.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the source is absent; [`Error::Failed`] with
    /// [`reason`](Fs::reason) for a refused move ("name already exists", "cannot move root", a
    /// destination inside the source). A move MUTATES - do not retry on a no-answer error without
    /// first checking what happened.
    pub fn move_to(&mut self, path: &str, dest: &str) -> Result<(), Error> {
        if dest.len() > PATH_MAX {
            return Err(Error::InvalidInput);
        }
        self.call(OP_MOVE, path.as_bytes(), dest.as_bytes(), call::DEFAULT_SECS)?;
        Ok(())
    }

    /// Delete a file, or a directory AND EVERYTHING INSIDE IT.
    ///
    /// The recursive one. [`delete`](Fs::delete) removes a single file and refuses a non-empty
    /// directory; this removes the subtree. The names differ deliberately - a caller reaching for
    /// the destructive one should have to type something that says so.
    ///
    /// **Blocks** up to [`call::DEFAULT_SECS`], and longer for a large tree than any single-file
    /// call. **Authority:** the caller's existing `fs` capability.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the path is absent; [`Error::Failed`] with [`reason`](Fs::reason)
    /// otherwise. **Never retry this on [`Error::OutcomeUnknown`]**: the service batches its
    /// frees, so a deadline that passes mid-delete may leave the tree partly removed, and a blind
    /// retry cannot tell that from never having started.
    pub fn delete_all(&mut self, path: &str) -> Result<(), Error> {
        self.call(OP_DELETE_TREE, path.as_bytes(), &[], call::DEFAULT_SECS)?;
        Ok(())
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
    ///   room, having learned the size from [`stat`](Fs::stat).
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
    /// The extent is reserved up front, so later [`write_at`](Fs::write_at) calls land in space that is already
    /// the file's. That is what makes a long append-style writer bounded: it cannot run out of room
    /// halfway and leave a half-file behind.
    ///
    /// **Blocks. Changes state**: on [`Error::OutcomeUnknown`] the file may exist. Use [`exists`](Fs::exists) to
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
    /// [`Error::OutcomeUnknown`] check with [`exists`](Fs::exists) rather than re-sending - this is precisely
    /// the case where a retry reports failure for work that succeeded.
    pub fn rename(&mut self, path: &str, new_name: &str) -> Result<(), Error> {
        self.call(OP_RENAME, path.as_bytes(), new_name.as_bytes(), call::DEFAULT_SECS)?;
        Ok(())
    }

    /// Does this path exist? A convenience over [`stat`](Fs::stat), and read-only.
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
