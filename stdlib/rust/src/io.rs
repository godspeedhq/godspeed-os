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
//! printer whose contract did not ask for `console_push`. Nobody had run it. `stdlib-hello` declares
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

/// Write a line to the console.
///
/// **Does not block** in any sense a caller needs to plan for: it hands bytes to the console
/// service and returns. **Authority:** `log_write`.
pub fn println(ctx: &ServiceContext, s: &str) {
    ctx.console_writeln(s);
}

/// Write without a trailing newline.
/// **Authority:** `log_write`.
pub fn print(ctx: &ServiceContext, s: &str) {
    ctx.console_write(s);
}

/// Write a formatted line.
///
/// Formatting is bounded and allocates nothing: it renders through a fixed stack buffer
/// (CLAUDE.md §26.6.1 is explicit that `format_args!` is the sanctioned bounded tool, and that
/// hand-rolling digit formatting to avoid a heap it never touches is the mistake).
///
/// ```ignore
/// io::println_fmt(ctx, format_args!("read {} bytes from {}", n, path));
/// ```
/// **Authority:** `log_write`.
pub fn println_fmt(ctx: &ServiceContext, args: core::fmt::Arguments) {
    ctx.console_writeln_fmt(args);
}

/// Write a formatted fragment, without the newline.
/// **Authority:** `log_write`.
pub fn print_fmt(ctx: &ServiceContext, args: core::fmt::Arguments) {
    ctx.console_write_fmt(args);
}

/// Report a failed operation in one line, in the house style: `<what>: <why>`.
///
/// Exists because every utility writes this by hand and they do not agree on the wording. Takes the
/// error rather than a string so the phrasing stays in one place and stays honest - in particular
/// [`crate::Error::OutcomeUnknown`] prints as a warning that the operation MAY have happened,
/// which is the fact a user most needs and the one a hand-written message usually drops.
/// **Authority:** `log_write` - this writes to the SCREEN (and serial), not to the kernel log ring.
pub fn report(ctx: &ServiceContext, what: &str, e: crate::Error) {
    ctx.console_writeln_fmt(format_args!("{}: {}", what, e.as_str()));
}
