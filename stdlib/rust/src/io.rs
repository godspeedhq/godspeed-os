// SPDX-License-Identifier: Apache-2.0
//! Writing to the screen.
//!
//! # The authority every function here needs is `log_write`
//!
//! Including [`report`]. All of them make the kernel's `ConsoleWrite` call, which writes serial and
//! hands the bytes to the `console` service, and the kernel checks ONE capability before it does:
//! `LOG_WRITE` (`kernel/src/syscall/dispatch.rs`, `handle_console_write`). Every task is minted that
//! capability at spawn, in slot 0, so a program's output reaches the screen whatever else its
//! contract says. Declare `log_write = true` anyway: the contract is the reviewable statement of what
//! a program may do (CLAUDE.md 13.6, 26.9), and printing is something it does.
//!
//! **`console_push` is NOT for printing, and must not be asked for to print.** It is the authority to
//! push bytes into the keyboard's input ring, which the shell reads as typed commands (CLAUDE.md 6.4,
//! SEC-2): a keyboard driver needs it, and nothing else should hold it.
//!
//! **This module said the opposite until 2026-10-09**, in four places and a gate, and the error is
//! worth recording because the whole design depends on authority being stated truthfully. A Stranger
//! Test run concluded from the documentation that a program without `console_push` prints nothing,
//! the documentation was rewritten to say so, and `scripts/contract_check.py` began failing any
//! printer whose contract did not ask for `console_push`. Nobody had run it. `stdlib-hello` declared
//! `console_push` and its spawn row grants it nothing (privilege word 0), and
//! `build/tests/examples_serial.log` holds its `io::report` and `io::println` lines regardless. Moving
//! every service onto this library made the gate fire on four drivers that print a notice - one of
//! which, `net-stack`, refuses keystroke authority in a comment for exactly the SEC-2 reason - and
//! obeying it would have handed them the power to type commands. The rule was false; the gate now
//! checks the true one.
//!
//! The thinnest module here, and deliberately so. `ServiceContext` already has a clean console
//! surface; this exists to give it the name a programmer reaches for first, and to put the
//! authority note somewhere they will read it.
//!
//! # Authority
//!
//! `log_write`, which every task holds. A program whose output is missing has not lost a capability:
//! look instead at whether a full-screen program owns the display (the kernel then sends background
//! output to serial only), or whether the `console` service is up.
//!
//! There is deliberately no `print!` macro that reaches a global stdout. A global would be ambient
//! authority wearing a familiar name (invariant 1), and the whole point of passing `ctx` is that
//! the answer to "what can this task reach" is its contract plus the arguments it was handed.

use godspeed_sdk::service_context::ServiceContext;

/// The most one `ConsoleWrite` carries. The SDK's `console_write` DROPS a longer string outright and
/// its formatted forms TRUNCATE at this length, both silently (CLAUDE.md 3.12 says a failure is loud).
/// Every function here writes in pieces of at most this size instead, so a long line arrives whole.
const CHUNK: usize = 256;

/// Write `s` in pieces the SDK will carry, each cut on a character boundary.
fn write_chunked(ctx: &ServiceContext, mut s: &str) {
    while s.len() > CHUNK {
        let mut cut = CHUNK;
        while !s.is_char_boundary(cut) { cut -= 1; }
        ctx.console_write(&s[..cut]);
        s = &s[cut..];
    }
    if !s.is_empty() { ctx.console_write(s); }
}

/// Renders `format_args!` through one fixed stack buffer, flushing it whenever the next fragment
/// would not fit, so output of any length is written in order and nothing is cut off. Bounded and
/// heap-free (CLAUDE.md 26.6.1).
struct Chunker<'a> {
    ctx: &'a ServiceContext,
    buf: [u8; CHUNK],
    len: usize,
}

impl Chunker<'_> {
    fn flush(&mut self) {
        if self.len > 0 {
            // Only whole `&str` fragments are ever copied in, so the buffer is always valid UTF-8.
            if let Ok(t) = core::str::from_utf8(&self.buf[..self.len]) { self.ctx.console_write(t); }
            self.len = 0;
        }
    }
}

impl core::fmt::Write for Chunker<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        if self.len + s.len() > CHUNK { self.flush(); }
        if s.len() > CHUNK {
            write_chunked(self.ctx, s);
        } else {
            self.buf[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
            self.len += s.len();
        }
        Ok(())
    }
}

fn write_fmt(ctx: &ServiceContext, args: core::fmt::Arguments) {
    let mut w = Chunker { ctx, buf: [0u8; CHUNK], len: 0 };
    let _ = core::fmt::write(&mut w, args);
    w.flush();
}

/// Write a line to the console.
///
/// A line of any length is written whole, in pieces of at most 256 bytes. **It may park the caller
/// briefly**: serial output is synchronous, and when the `console` service's queue is full the kernel
/// parks the writer until there is room (`handle_console_write`). A program printing in a tight loop
/// runs at the speed of the screen. **Authority:** `log_write`.
pub fn println(ctx: &ServiceContext, s: &str) {
    write_chunked(ctx, s);
    ctx.console_write("\n");
}

/// Write without a trailing newline. Any length; may park briefly, as [`println`].
/// **Authority:** `log_write`.
pub fn print(ctx: &ServiceContext, s: &str) {
    write_chunked(ctx, s);
}

/// Write a formatted line.
///
/// Formatting is bounded and allocates nothing: it renders through a fixed stack buffer
/// (CLAUDE.md §26.6.1 is explicit that `format_args!` is the sanctioned bounded tool, and that
/// hand-rolling digit formatting to avoid a heap it never touches is the mistake). Output longer
/// than the buffer is written in pieces, never cut off.
///
/// ```ignore
/// io::println_fmt(ctx, format_args!("read {} bytes from {}", n, path));
/// ```
/// **Authority:** `log_write`.
pub fn println_fmt(ctx: &ServiceContext, args: core::fmt::Arguments) {
    write_fmt(ctx, args);
    ctx.console_write("\n");
}

/// Write a formatted fragment, without the newline.
/// **Authority:** `log_write`.
pub fn print_fmt(ctx: &ServiceContext, args: core::fmt::Arguments) {
    write_fmt(ctx, args);
}

/// Report a failed operation in one line, in the house style: `<what>: <why>`.
///
/// Exists because every utility writes this by hand and they do not agree on the wording. Takes the
/// error rather than a string so the phrasing stays in one place and stays honest - in particular
/// [`crate::Error::OutcomeUnknown`] prints as a warning that the operation MAY have happened,
/// which is the fact a user most needs and the one a hand-written message usually drops.
/// **Authority:** `log_write` - this writes to the SCREEN (and serial), not to the kernel log ring.
pub fn report(ctx: &ServiceContext, what: &str, e: crate::Error) {
    println_fmt(ctx, format_args!("{}: {}", what, e.as_str()));
}
