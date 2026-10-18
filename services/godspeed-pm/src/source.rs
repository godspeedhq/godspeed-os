// SPDX-License-Identifier: GPL-2.0-only
//! Where the registry's documents come from: the registry itself over HTTPS, or a `.gsp` file that holds the same
//! documents under the same paths (`docs/developer-services-design.md` 15). Everything that reads a document goes
//! through [`Source`], so a package checked from a file is checked by exactly the code that checks one fetched,
//! in the same order: the file is one more place the paths are answered from, and nothing else changes.
//!
//! The file's layout is the shared crate's (`godspeed_package::gsp`), so the CLI that writes it and this reader
//! cannot disagree about it. It is read once for its table of entries, then each entry at its offset through
//! `fs`; nothing of it is held but the entry being read.

use godspeed::{self as gs, ServiceContext};
use godspeed_package::gsp::{self, GspError, Line};

use crate::http::Head;
use crate::https::{Fail, Session};

/// A place the registry's documents are read from.
pub trait Source {
    /// GET `path`, as [`Session::get_sized`]: the status and headers come back, `on_head` is told a 200's before
    /// its body, and the body goes to `sink` a piece at a time. Either returning `false` stops it
    /// ([`Fail::Refused`]).
    fn get_sized(&mut self, path: &str, on_head: &mut dyn FnMut(&Head) -> bool, sink: &mut dyn FnMut(&[u8]) -> bool)
        -> Result<Head, Fail>;

    /// [`get_sized`](Source::get_sized) with no interest in the head.
    fn get(&mut self, path: &str, sink: &mut dyn FnMut(&[u8]) -> bool) -> Result<Head, Fail> {
        self.get_sized(path, &mut |_| true, sink)
    }

    /// True when this source is a file, so what it says is as of the day it was made, not now.
    fn offline(&self) -> bool { false }
}

impl Source for Session<'_, '_, '_, '_> {
    fn get_sized(&mut self, path: &str, on_head: &mut dyn FnMut(&Head) -> bool, sink: &mut dyn FnMut(&[u8]) -> bool)
        -> Result<Head, Fail> {
        Session::get_sized(self, path, on_head, sink)
    }
}

/// The longest file path a `.gsp` may be opened from.
pub const GSP_FILE_MAX: usize = 200;

#[derive(Clone, Copy)]
struct Entry {
    path: [u8; gsp::PATH_MAX],
    path_len: usize,
    offset: u64,
    size: u64,
}

const NO_ENTRY: Entry = Entry { path: [0; gsp::PATH_MAX], path_len: 0, offset: 0, size: 0 };

/// Why a `.gsp` could not be opened.
#[derive(Clone, Copy, Debug)]
pub enum OpenError {
    /// `fs` could not read it.
    Store(gs::Error),
    /// Its layout is wrong: the text says how.
    Layout(GspError),
    /// It ends before its `end` line, or an entry runs past its end, or something follows `end`.
    Truncated,
    /// The path is longer than this machine opens a package file from.
    PathTooLong,
}

impl OpenError {
    pub fn text(&self) -> &'static str {
        match self {
            OpenError::Store(gs::Error::NotFound) => "there is no such file",
            OpenError::Store(_) => "it could not be read",
            OpenError::Layout(e) => e.message(),
            OpenError::Truncated => "it is cut short, or has bytes after its end - copy it again",
            OpenError::PathTooLong => "its path is longer than a package file is opened from",
        }
    }
}

/// A `.gsp` file, its entries found and checked for shape. Each document is read when asked for.
pub struct GspSource<'c> {
    ctx: &'c ServiceContext,
    file: [u8; GSP_FILE_MAX],
    file_len: usize,
    entries: [Entry; gsp::ENTRIES_MAX],
    count: usize,
}

/// The line starting at `off` (without its `\n`), into `buf`, and where the next one starts. `None` at the end
/// of the file or for a line longer than `buf`.
fn line_at(fs: &mut gs::fs::Fs<'_>, file: &str, off: u64, buf: &mut [u8; gsp::LINE_MAX]) -> Result<Option<(usize, u64)>, OpenError> {
    let got = fs.read_at(file, off, buf).map_err(OpenError::Store)?;
    match buf[..got].iter().position(|&b| b == b'\n') {
        Some(n) => Ok(Some((n, off + n as u64 + 1))),
        None if got == 0 => Ok(None),
        None => Err(OpenError::Layout(GspError::BadLine)),
    }
}

impl<'c> GspSource<'c> {
    /// Open `file`: read its header and every entry line, each entry's bytes skipped over, and check that the
    /// entries fill the file exactly to its `end`.
    #[inline(never)]
    pub fn open(ctx: &'c ServiceContext, file: &str) -> Result<GspSource<'c>, OpenError> {
        if file.len() > GSP_FILE_MAX { return Err(OpenError::PathTooLong); }
        let mut s = GspSource { ctx, file: [0; GSP_FILE_MAX], file_len: file.len(), entries: [NO_ENTRY; gsp::ENTRIES_MAX], count: 0 };
        s.file[..file.len()].copy_from_slice(file.as_bytes());
        let mut fs = gs::fs::Fs::new(ctx);
        let total = fs.stat(file).map_err(OpenError::Store)?.size;
        let mut buf = [0u8; gsp::LINE_MAX];
        // A first line that is not one at all - no newline, or nothing - is not a package file, whatever else it is.
        let (n, mut off) = match line_at(&mut fs, file, 0, &mut buf) {
            Ok(Some(x)) => x,
            Ok(None) | Err(OpenError::Layout(_)) => return Err(OpenError::Layout(GspError::NotAGsp)),
            Err(e) => return Err(e),
        };
        gsp::header(core::str::from_utf8(&buf[..n]).unwrap_or("")).map_err(OpenError::Layout)?;
        loop {
            let Some((n, next)) = line_at(&mut fs, file, off, &mut buf)? else { return Err(OpenError::Truncated) };
            let text = core::str::from_utf8(&buf[..n]).map_err(|_| OpenError::Layout(GspError::BadLine))?;
            match gsp::line(text).map_err(OpenError::Layout)? {
                Line::End => {
                    if next != total { return Err(OpenError::Truncated); }
                    return Ok(s);
                }
                Line::Entry { size, path } => {
                    if s.count == gsp::ENTRIES_MAX { return Err(OpenError::Layout(GspError::TooMany)); }
                    if s.entries[..s.count].iter().any(|e| &e.path[..e.path_len] == path.as_bytes()) {
                        return Err(OpenError::Layout(GspError::Repeated));
                    }
                    // The bytes, then the `\n` that closes them, must lie inside the file.
                    let end = next.checked_add(size).filter(|&e| e < total).ok_or(OpenError::Truncated)?;
                    let mut nl = [0u8; 1];
                    if fs.read_at(file, end, &mut nl).map_err(OpenError::Store)? != 1 || nl[0] != b'\n' {
                        return Err(OpenError::Layout(GspError::BadLine));
                    }
                    let e = &mut s.entries[s.count];
                    e.path[..path.len()].copy_from_slice(path.as_bytes());
                    e.path_len = path.len();
                    e.offset = next;
                    e.size = size;
                    s.count += 1;
                    off = end + 1;
                }
            }
        }
    }

    fn file(&self) -> &str { core::str::from_utf8(&self.file[..self.file_len]).unwrap_or("") }

    /// Whether it holds `path`.
    pub fn has(&self, path: &str) -> bool {
        self.entries[..self.count].iter().any(|e| &e.path[..e.path_len] == path.as_bytes())
    }

    /// The paths it holds, in the order the file holds them.
    pub fn each_path(&self, f: &mut dyn FnMut(&str, u64)) {
        for e in &self.entries[..self.count] {
            f(core::str::from_utf8(&e.path[..e.path_len]).unwrap_or(""), e.size);
        }
    }
}

impl Source for GspSource<'_> {
    /// The entry the registry would serve at `path`, as a 200 with its length; a 404 for one the file does not
    /// hold, exactly as the registry answers a document it does not have. The leading `/` of a registry path is
    /// not part of an entry's.
    fn get_sized(&mut self, path: &str, on_head: &mut dyn FnMut(&Head) -> bool, sink: &mut dyn FnMut(&[u8]) -> bool)
        -> Result<Head, Fail> {
        let want = path.trim_start_matches('/');
        let Some(e) = self.entries[..self.count].iter().find(|e| &e.path[..e.path_len] == want.as_bytes()).copied() else {
            return Ok(Head { status: 404, length: Some(0), chunked: false, close: false });
        };
        let head = Head { status: 200, length: Some(e.size), chunked: false, close: false };
        if !on_head(&head) { return Err(Fail::Refused); }
        let mut fs = gs::fs::Fs::new(self.ctx);
        let mut chunk = [0u8; gs::fs::IO_CHUNK];
        let mut done = 0u64;
        while done < e.size {
            let want = ((e.size - done) as usize).min(chunk.len());
            let got = fs.read_at(self.file(), e.offset + done, &mut chunk[..want]).map_err(|_| Fail::Transport)?;
            if got == 0 { return Err(Fail::Closed); }
            if !sink(&chunk[..got]) { return Err(Fail::Refused); }
            done += got as u64;
        }
        Ok(head)
    }

    fn offline(&self) -> bool { true }
}

// ---- making a `.gsp` on this machine ------------------------------------------------------------------------

/// Where a recording keeps each document until the file is written: one numbered file a document.
pub const STAGING: &str = "/system/scratch/godspeed/gsp/";
/// The largest path a staged or kept file is read from.
const FS_PATH_MAX: usize = 200;

/// The most documents one recording keeps: every root, the four signed roles and their signatures, an index, a
/// release's three documents and a binary, with room. Bounded below the layout's own limit because the table is on
/// the stack beside a TLS session.
const RECORD_MAX: usize = 32;
/// Files already on this disk a recording may point at instead of a staged copy: the snapshot, a fetched binary.
const EXTERNAL_MAX: usize = 2;

#[derive(Clone, Copy)]
struct Kept {
    /// The registry path it is served at.
    path: [u8; gsp::PATH_MAX],
    path_len: usize,
    /// Where on this disk its bytes are: staged file `<STAGING><n>`, or `externals[n]`.
    external: bool,
    n: usize,
    size: u64,
}

const NO_KEPT: Kept = Kept { path: [0; gsp::PATH_MAX], path_len: 0, external: false, n: 0, size: 0 };

/// Why a `.gsp` could not be made.
#[derive(Clone, Copy, Debug)]
pub enum SaveError {
    /// `fs` refused a write or a read.
    Store(gs::Error),
    /// The registry answered a document without saying its size, which a file on this disk must be given first.
    NoLength,
    /// More documents than a package file holds.
    TooMany,
    /// A path too long to record.
    PathTooLong,
}

impl SaveError {
    pub fn text(&self) -> &'static str {
        match self {
            SaveError::Store(_) => "this disk refused part of it",
            SaveError::NoLength => "the registry sent a document without its size, so it could not be kept",
            SaveError::TooMany => "it would hold more documents than a package file may",
            SaveError::PathTooLong => "a path in it is longer than a package file holds",
        }
    }
}

/// A [`Source`] that answers from another one and keeps every document it answers with a 200, under the path it
/// was asked by, so the documents a verified fetch read become a `.gsp` ([`Recorder::write`]). What the recorder
/// passes on is exactly what the source sent; it keeps a copy and changes nothing.
pub struct Recorder<'s, 'c> {
    inner: &'s mut dyn Source,
    ctx: &'c ServiceContext,
    kept: [Kept; RECORD_MAX],
    count: usize,
    externals: [[u8; FS_PATH_MAX]; EXTERNAL_MAX],
    external_lens: [usize; EXTERNAL_MAX],
    external_count: usize,
    /// How many staged files were made, for the next one's name.
    staged: usize,
    failed: Option<SaveError>,
}

/// `<STAGING><n>`.
fn staged_path(n: usize, out: &mut [u8; FS_PATH_MAX]) -> usize {
    let mut d = [0u8; 20];
    let mut i = d.len();
    let mut x = n;
    loop {
        i -= 1;
        d[i] = b'0' + (x % 10) as u8;
        x /= 10;
        if x == 0 { break; }
    }
    let s = STAGING.as_bytes();
    out[..s.len()].copy_from_slice(s);
    out[s.len()..s.len() + d.len() - i].copy_from_slice(&d[i..]);
    s.len() + d.len() - i
}

impl<'s, 'c> Recorder<'s, 'c> {
    /// A recorder over `inner`, its staging folder emptied first.
    pub fn new(ctx: &'c ServiceContext, inner: &'s mut dyn Source) -> Result<Recorder<'s, 'c>, SaveError> {
        let mut fs = gs::fs::Fs::new(ctx);
        let _ = fs.delete_all(STAGING);
        fs.create_dir_all(STAGING).map_err(SaveError::Store)?;
        Ok(Recorder { inner, ctx, kept: [NO_KEPT; RECORD_MAX], count: 0, externals: [[0; FS_PATH_MAX]; EXTERNAL_MAX],
                      external_lens: [0; EXTERNAL_MAX], external_count: 0, staged: 0, failed: None })
    }

    /// Whether `path` was recorded.
    pub fn has(&self, path: &str) -> bool {
        let want = path.trim_start_matches('/').as_bytes();
        self.kept[..self.count].iter().any(|k| &k.path[..k.path_len] == want)
    }

    /// Record `path` as the bytes of a file already on this disk - the snapshot this machine kept, or a binary it
    /// fetched before - when the fetch did not need to read it again.
    pub fn add_file(&mut self, path: &str, file: &str) -> Result<(), SaveError> {
        let size = gs::fs::Fs::new(self.ctx).stat(file).map_err(SaveError::Store)?.size;
        if self.external_count == EXTERNAL_MAX { return Err(SaveError::TooMany); }
        if file.len() > FS_PATH_MAX { return Err(SaveError::PathTooLong); }
        let n = self.external_count;
        self.externals[n][..file.len()].copy_from_slice(file.as_bytes());
        self.external_lens[n] = file.len();
        self.external_count += 1;
        self.record(path, true, n, size)
    }

    fn record(&mut self, path: &str, external: bool, n: usize, size: u64) -> Result<(), SaveError> {
        let path = path.trim_start_matches('/');
        if path.len() > gsp::PATH_MAX { return Err(SaveError::PathTooLong); }
        // A path asked for twice - a second session after the first closed - is the later answer.
        let at = match self.kept[..self.count].iter().position(|k| &k.path[..k.path_len] == path.as_bytes()) {
            Some(i) => i,
            None if self.count == RECORD_MAX => return Err(SaveError::TooMany),
            None => { self.count += 1; self.count - 1 }
        };
        let k = &mut self.kept[at];
        k.path[..path.len()].copy_from_slice(path.as_bytes());
        k.path_len = path.len();
        k.external = external;
        k.n = n;
        k.size = size;
        Ok(())
    }

    /// Write every recorded document to `partial` as a `.gsp`, sized first and written in whole blocks, then
    /// rename it `name` (a bare name in the same folder) and empty the staging folder. Its size.
    #[inline(never)]
    pub fn write(&self, partial: &str, name: &str, out: &str) -> Result<u64, SaveError> {
        if let Some(e) = self.failed { return Err(e); }
        let mut fs = gs::fs::Fs::new(self.ctx);
        // The size first: the header, each entry's line, bytes and newline, and the end line.
        let digits = |mut v: u64| { let mut n = 1u64; while v >= 10 { v /= 10; n += 1; } n };
        let mut total = gsp::HEADER.len() as u64 + 1 + 4;
        for k in &self.kept[..self.count] {
            total += 6 + digits(k.size) + 1 + k.path_len as u64 + 1 + k.size + 1;
        }
        let _ = fs.delete(partial);
        fs.create_sized(partial, total).map_err(SaveError::Store)?;
        let mut buf = [0u8; gs::fs::IO_CHUNK];
        {
            let mut w = Blocks { fs: &mut fs, file: partial, chunk: [0; gs::fs::IO_CHUNK], held: 0, at: 0 };
            w.put(gsp::HEADER.as_bytes())?;
            w.put(b"\n")?;
            for k in &self.kept[..self.count] {
                let mut line = [0u8; gsp::LINE_MAX];
                let mut n = 0usize;
                let mut d = [0u8; 20];
                let mut i = d.len();
                let mut x = k.size;
                loop { i -= 1; d[i] = b'0' + (x % 10) as u8; x /= 10; if x == 0 { break; } }
                for part in [&b"entry "[..], &d[i..], &b" "[..], &k.path[..k.path_len], &b"\n"[..]] {
                    line[n..n + part.len()].copy_from_slice(part);
                    n += part.len();
                }
                w.put(&line[..n])?;
                let mut staged = [0u8; FS_PATH_MAX];
                let src = if k.external {
                    core::str::from_utf8(&self.externals[k.n][..self.external_lens[k.n]]).unwrap_or("")
                } else {
                    let l = staged_path(k.n, &mut staged);
                    core::str::from_utf8(&staged[..l]).unwrap_or("")
                };
                let mut off = 0u64;
                while off < k.size {
                    let want = ((k.size - off) as usize).min(buf.len());
                    let got = w.fs.read_at(src, off, &mut buf[..want]).map_err(SaveError::Store)?;
                    if got == 0 { return Err(SaveError::Store(gs::Error::Malformed)); }
                    w.put(&buf[..got])?;
                    off += got as u64;
                }
                w.put(b"\n")?;
            }
            w.put(b"end\n")?;
            w.flush()?;
        }
        let _ = fs.delete(out);
        fs.rename(partial, name).map_err(SaveError::Store)?;
        let _ = fs.delete_all(STAGING);
        Ok(total)
    }
}

/// Sequential writes into a sized file, a whole number of blocks at a time, as `fs` takes them.
struct Blocks<'f, 'a, 'p> {
    fs: &'f mut gs::fs::Fs<'a>,
    file: &'p str,
    chunk: [u8; gs::fs::IO_CHUNK],
    held: usize,
    at: u64,
}

impl Blocks<'_, '_, '_> {
    fn put(&mut self, mut b: &[u8]) -> Result<(), SaveError> {
        while !b.is_empty() {
            let take = b.len().min(self.chunk.len() - self.held);
            self.chunk[self.held..self.held + take].copy_from_slice(&b[..take]);
            self.held += take;
            b = &b[take..];
            if self.held == self.chunk.len() {
                self.fs.write_at(self.file, self.at, &self.chunk).map_err(SaveError::Store)?;
                self.at += self.held as u64;
                self.held = 0;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SaveError> {
        if self.held > 0 {
            self.fs.write_at(self.file, self.at, &self.chunk[..self.held]).map_err(SaveError::Store)?;
            self.at += self.held as u64;
            self.held = 0;
        }
        Ok(())
    }
}

impl Source for Recorder<'_, '_> {
    fn get_sized(&mut self, path: &str, on_head: &mut dyn FnMut(&Head) -> bool, sink: &mut dyn FnMut(&[u8]) -> bool)
        -> Result<Head, Fail> {
        let mut name = [0u8; FS_PATH_MAX];
        let number = self.staged;
        let nl = staged_path(number, &mut name);
        self.staged += 1;
        let file = core::str::from_utf8(&name[..nl]).unwrap_or("");
        let fs = core::cell::RefCell::new(gs::fs::Fs::new(self.ctx));
        let failed: core::cell::Cell<Option<SaveError>> = core::cell::Cell::new(None);
        let size: core::cell::Cell<u64> = core::cell::Cell::new(0);
        let mut chunk = [0u8; gs::fs::IO_CHUNK];
        let (mut held, mut written) = (0usize, 0u64);
        let head = self.inner.get_sized(path, &mut |h| {
            if !on_head(h) { return false; }
            match h.length {
                Some(0) => {}
                Some(n) => {
                    if let Err(e) = fs.borrow_mut().create_sized(file, n) { failed.set(Some(SaveError::Store(e))); }
                    size.set(n);
                }
                None => failed.set(Some(SaveError::NoLength)),
            }
            true
        }, &mut |b: &[u8]| {
            if failed.get().is_none() {
                let mut rest = b;
                while !rest.is_empty() {
                    let take = rest.len().min(chunk.len() - held);
                    chunk[held..held + take].copy_from_slice(&rest[..take]);
                    held += take;
                    rest = &rest[take..];
                    if held == chunk.len() {
                        if let Err(e) = fs.borrow_mut().write_at(file, written, &chunk) { failed.set(Some(SaveError::Store(e))); break; }
                        written += held as u64;
                        held = 0;
                    }
                }
            }
            sink(b)
        })?;
        if head.status == 200 && failed.get().is_none() {
            if held > 0 {
                if let Err(e) = fs.borrow_mut().write_at(file, written, &chunk[..held]) { failed.set(Some(SaveError::Store(e))); }
                written += held as u64;
            }
            if failed.get().is_none() && written == size.get() {
                if let Err(e) = self.record(path, false, number, written) { failed.set(Some(e)); }
            }
        }
        // A copy that could not be kept does not change the answer: the fetch goes on, and making the file says why.
        if self.failed.is_none() { self.failed = failed.get(); }
        Ok(head)
    }
}
