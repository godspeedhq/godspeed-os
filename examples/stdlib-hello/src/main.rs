// SPDX-License-Identifier: Apache-2.0
//! Write a file, read it back and check it, using the standard library.
//!
//! This is the whole program. Read `contracts/stdlib-hello.toml` next to it and you know
//! everything this thing can do - there is nothing else, because there is no ambient authority to
//! fall back on.
//!
//! # What you do NOT need to know to read this
//!
//! The filesystem opcode numbers. The request framing (`[tag, op, path_len, path, ...]`). That a
//! reply carries a tag so a late answer to an earlier question is not mistaken for this one. That a
//! file larger than one IPC message has to be streamed in pieces. That a restarted service leaves
//! your capability stale forever and you are the one obliged to notice (§14.3).
//!
//! All of that is real and none of it is gone. It is in `stdlib/rust`, written once.
//!
//! # What you DO still need to know, because pretending otherwise would be a lie
//!
//! That authority is granted, not taken: the `ipc_send = ["fs"]` line in the contract is why the
//! write and the read below can work at all. And that a failure is a fact rather than a nuisance - which is why
//! the error arm below prints what happened instead of retrying until something looks fine.

#![deny(unsafe_code)]
#![no_std]
#![no_main]

// ONE import line, and it does not name the SDK. It used to: `ServiceContext` was reachable only
// from `godspeed_sdk`, while the documentation told an ordinary program it would not need that
// crate. The Stranger Test caught the contradiction - a weak model guessed the SDK path and flagged
// it as the thing most likely to stop its program compiling - so the library re-exports it now.
use godspeed::{self as gs, Error, ServiceContext};

/// The file this program writes and reads back. At 8000 bytes it is larger than one IPC message
/// (`gs::fs::IO_CHUNK`, 3556 bytes), so `write` and `read_into` each stream it in three pieces - and
/// the program cannot tell, which is the point of a library.
const PATH: &str = "/stdlib-hello.txt";
const SIZE: usize = 8000;

#[allow(unsafe_code)] // the exported entry symbol; see `stdlib/rust/src/lib.rs` on why `fn main` is not available yet
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("stdlib-hello: starting");

    // The filesystem handle. This grants NOTHING - it borrows the context, and the context can only
    // reach what the spawn request granted (the contract is its reviewable statement). A program
    // without `ipc_send = ["fs"]` gets this same handle and every call through it fails with
    // `Unreachable`.
    let mut disk = gs::fs::Fs::new(&ctx);

    // The caller owns the buffers. There is no heap on this machine (CLAUDE.md 26.6.1), so the most
    // this program can use is readable right here rather than hidden in an allocator.
    let mut text = [0u8; SIZE];
    for (i, b) in text.iter_mut().enumerate() {
        *b = if i % 64 == 63 { b'\n' } else { b'a' + (i % 26) as u8 };
    }

    match disk.write(PATH, &text) {
        Ok(()) => gs::io::println_fmt(&ctx, format_args!("wrote {} ({} bytes)", PATH, SIZE)),
        Err(e) => {
            // `gs::io::report` phrases it, so every program says the same thing about the same
            // failure - including the one that matters most, where the write MAY have happened.
            gs::io::report(&ctx, "write", e);
            // A write CHANGES STATE. When no answer came - or `fs` died holding the request - the
            // honest reply is that the file may be there, and writing it again is not the way to
            // find out. (A plain failure is not that case: `fs` answered, and said no.)
            if matches!(e, Error::OutcomeUnknown | Error::PeerDied) {
                gs::io::println(&ctx, "write: it may have happened - read the file to find out, do not write it again");
            }
            finish(&ctx);
        }
    }

    let mut back = [0u8; SIZE + 1]; // one spare byte, so a file LONGER than written is seen
    match disk.read_into(PATH, &mut back) {
        Ok(n) if n == SIZE && back[..n] == text[..] => {
            gs::io::println_fmt(&ctx, format_args!("read {} back: {} bytes, every one as written", PATH, n));
        }
        Ok(n) => {
            gs::io::println_fmt(&ctx, format_args!("read {} back: {} bytes, NOT what was written", PATH, n));
        }
        // A file that is not there right after writing it is a fact worth saying plainly.
        Err(Error::NotFound) => gs::io::println_fmt(&ctx, format_args!("{} is not there after writing it", PATH)),
        Err(e) => {
            gs::io::report(&ctx, "read", e);
            // A read is idempotent, so retrying it IS safe - and still asked, because for the write
            // above the answer was different and this is the habit worth forming.
            if e.retry_is_safe() {
                gs::io::println(&ctx, "read: nothing happened, so this one is safe to try again");
            }
        }
    }
    finish(&ctx);
}

fn finish(ctx: &ServiceContext) -> ! {
    ctx.log("stdlib-hello: done");
    loop {
        gs::task::yield_now(ctx);
    }
}
