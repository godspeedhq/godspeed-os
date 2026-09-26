// SPDX-License-Identifier: Apache-2.0
//! Read a file and print it, using the standard library.
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
//! read below can work at all. And that a failure is a fact rather than a nuisance - which is why
//! the error arm below prints what happened instead of retrying until something looks fine.

#![deny(unsafe_code)]
#![no_std]
#![no_main]

// ONE import line, and it does not name the SDK. It used to: `ServiceContext` was reachable only
// from `godspeed_sdk`, while the documentation told an ordinary program it would not need that
// crate. The Stranger Test caught the contradiction - a weak model guessed the SDK path and flagged
// it as the thing most likely to stop its program compiling - so the library re-exports it now.
use godspeed::{self as gs, Error, ServiceContext};

/// Where the text comes from. `selfcheck` writes this file, so the program has something to find
/// on a running machine.
const PATH: &str = "/sc/a.txt";

#[allow(unsafe_code)] // the exported entry symbol; see `stdlib/rust/src/lib.rs` on why `fn main` is not available yet
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("stdlib-hello: starting");

    // The filesystem handle. This grants NOTHING - it borrows the context, and the context can only
    // reach what the contract asked for. A program without `ipc_send = ["fs"]` gets this same
    // handle and every call through it fails with `Unreachable`.
    let mut disk = gs::fs::Fs::new(&ctx);

    // The caller owns the buffer. There is no heap on this machine (CLAUDE.md §26.6.1), so the
    // maximum this program can use is readable right here rather than hidden in an allocator.
    let mut buf = [0u8; 4096];

    match disk.read_into(PATH, &mut buf) {
        Ok(n) => {
            let text = core::str::from_utf8(&buf[..n]).unwrap_or("<not valid utf-8>");
            gs::io::println_fmt(&ctx, format_args!("{} ({} bytes):", PATH, n));
            gs::io::println(&ctx, text);
        }

        // A file that is not there is not a malfunction, and is worth telling apart from one.
        Err(Error::NotFound) => {
            gs::io::println_fmt(&ctx, format_args!("{} is not there - run `selfcheck` to create it", PATH));
        }

        // Everything else prints what actually happened. `gs::io::report` phrases it, so every program
        // says the same thing about the same failure - including the one that matters most, where
        // the operation MAY have completed and the honest answer is to say so.
        Err(e) => {
            gs::io::report(&ctx, "read", e);

            // A read is idempotent, so retrying any no-answer failure is safe here. `retry_is_safe`
            // is asked rather than assumed, because for a WRITE the answer would be different and
            // this is the habit worth forming.
            if e.retry_is_safe() {
                gs::io::println(&ctx, "read: nothing happened, so this one is safe to try again");
            }
        }
    }

    ctx.log("stdlib-hello: done");
    loop {
        gs::task::yield_now(&ctx);
    }
}
