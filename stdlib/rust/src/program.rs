// SPDX-License-Identifier: Apache-2.0
//! A program that does one job and ends: a `utility`.
//!
//! A service runs until it is stopped. A utility is started by the shell for one command line, is handed
//! its arguments, writes its output, and ends with a status - and the shell shows that output, pipes it
//! onward, and reports the status. This module is the whole of what that needs:
//!
//! ```ignore
//! use godspeed::{self as gs, ServiceContext};
//!
//! #[allow(unsafe_code)]
//! #[no_mangle]
//! pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
//!     let mut buf = [0u8; gs::program::ARGS_MAX];
//!     let args = gs::program::args(&ctx, &mut buf);
//!     let mut out = gs::program::Output::new(&ctx);
//!     let _ = core::fmt::Write::write_fmt(&mut out, format_args!("hello, {}\n", if args.is_empty() { "world" } else { args }));
//!     out.finish(0)
//! }
//! ```
//!
//! **How it reaches the shell.** The shell starts an installed utility through the supervisor wired to send
//! to the shell (its first peer), sends the command line's arguments as one message and then the end byte,
//! and reads everything the program sends until the end byte - the same protocol every pipe stage speaks
//! (`docs/pipes.md`), which is why a utility's output can be piped like any built-in's. [`Output::finish`]
//! sends the end byte and then ends the program with its status (the `Exit` syscall, CLAUDE.md 14.5
//! amendment), so the shell can say whether it worked.

use core::fmt;

use crate::ipc::{self, Message};
use crate::ServiceContext;

/// The end of a stream: the byte every pipe stage sends last.
pub const EOT: u8 = 0x04;
/// The longest argument line a program is handed: one message.
pub const ARGS_MAX: usize = 4096;
/// How long a program waits for its arguments before it takes the line as empty. The shell sends them as soon
/// as the program has its endpoint, so this is only reached when nothing started it with any.
pub const ARGS_SECS: i64 = 10;

/// The arguments this program was started with, as one line - or empty when it was given none.
///
/// The first message on this program's endpoint, read once; the end byte that follows it is taken too.
pub fn args<'b>(ctx: &ServiceContext, buf: &'b mut [u8; ARGS_MAX]) -> &'b str {
    let Some(m) = ipc::recv_within(ctx, ARGS_SECS) else { return "" };
    let p = m.payload_bytes();
    if p == [EOT] { return ""; }
    let n = p.len().min(buf.len());
    buf[..n].copy_from_slice(&p[..n]);
    // Its end byte, so it is not read later as input.
    if let Some(e) = ipc::recv_within(ctx, 1) {
        if e.payload_bytes() != [EOT] {
            // Not the end byte: there was more than one message of arguments, which this does not take.
            ctx.log("program: more arguments than one message - only the first was taken");
        }
    }
    let line = core::str::from_utf8(&buf[..n]).unwrap_or("");
    line.trim()
}

/// What a program writes, sent to the shell a message at a time.
///
/// Implements [`fmt::Write`], so `write!` works. Bounded: one message is buffered and sent when full, so a
/// program can write any amount without holding it all.
pub struct Output<'c> {
    ctx: &'c ServiceContext,
    buf: [u8; ARGS_MAX],
    n: usize,
    /// Whether the shell stopped taking it: a send failed, so later writes are dropped and [`finish`] reports
    /// a failure (Output::finish) rather than claiming the output arrived.
    lost: bool,
}

impl<'c> Output<'c> {
    /// Output to whoever started this program: its first peer, the shell.
    pub fn new(ctx: &'c ServiceContext) -> Output<'c> {
        Output { ctx, buf: [0; ARGS_MAX], n: 0, lost: false }
    }

    /// Send what is buffered now.
    pub fn flush(&mut self) {
        if self.n == 0 || self.lost { self.n = 0; return; }
        let sent = ipc::peer_at(self.ctx, 0)
            .map(|down| ipc::send_to(self.ctx, down, &Message::from_bytes(&self.buf[..self.n])).is_ok())
            .unwrap_or(false);
        if !sent { self.lost = true; }
        self.n = 0;
    }

    /// Send the rest and the end byte, then end the program with `status` - 0 for success. A status of 0
    /// becomes 1 when the output could not be delivered, so the shell is never told it worked when it did not.
    pub fn finish(mut self, status: u32) -> ! {
        self.flush();
        let ended = ipc::peer_at(self.ctx, 0)
            .map(|down| ipc::send_to(self.ctx, down, &Message::from_bytes(&[EOT])).is_ok())
            .unwrap_or(false);
        let status = if status == 0 && (self.lost || !ended) { 1 } else { status };
        exit(self.ctx, status)
    }
}

impl fmt::Write for Output<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut b = s.as_bytes();
        while !b.is_empty() {
            if self.n == self.buf.len() { self.flush(); }
            let take = b.len().min(self.buf.len() - self.n);
            self.buf[self.n..self.n + take].copy_from_slice(&b[..take]);
            self.n += take;
            b = &b[take..];
        }
        Ok(())
    }
}

/// End this program now, with `status`. A program that writes output ends through [`Output::finish`] instead,
/// so the shell receives the end of its output first.
pub fn exit(ctx: &ServiceContext, status: u32) -> ! {
    ctx.exit(status)
}
