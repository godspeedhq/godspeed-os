// SPDX-License-Identifier: Apache-2.0
//! Writing to the screen.
//!
//! The thinnest module here, and deliberately so. `ServiceContext` already has a clean console
//! surface; this exists to give it the name a programmer reaches for first, and to put the
//! authority note somewhere they will read it.
//!
//! # Authority
//!
//! Printing needs the `console_push` capability, granted by the contract. A task without it can
//! call these and nothing appears - the same as any other ungranted operation, and NOT something
//! this library can fix by trying harder. If your output is missing, check the contract before
//! checking the code.
//!
//! There is deliberately no `print!` macro that reaches a global stdout. A global would be ambient
//! authority wearing a familiar name (invariant 1), and the whole point of passing `ctx` is that
//! the answer to "what can this task reach" is its contract plus the arguments it was handed.

use godspeed_sdk::service_context::ServiceContext;

/// Write a line to the console.
///
/// **Does not block** in any sense a caller needs to plan for: it hands bytes to the console
/// service and returns. **Authority:** `console_push`.
pub fn println(ctx: &ServiceContext, s: &str) {
    ctx.console_writeln(s);
}

/// Write without a trailing newline.
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
pub fn println_fmt(ctx: &ServiceContext, args: core::fmt::Arguments) {
    ctx.console_writeln_fmt(args);
}

/// Write a formatted fragment, without the newline.
pub fn print_fmt(ctx: &ServiceContext, args: core::fmt::Arguments) {
    ctx.console_write_fmt(args);
}

/// Report a failed operation in one line, in the house style: `<what>: <why>`.
///
/// Exists because every utility writes this by hand and they do not agree on the wording. Takes the
/// error rather than a string so the phrasing stays in one place and stays honest - in particular
/// [`crate::Error::OutcomeUnknown`] prints as a warning that the operation MAY have happened,
/// which is the fact a user most needs and the one a hand-written message usually drops.
pub fn report(ctx: &ServiceContext, what: &str, e: crate::Error) {
    ctx.console_writeln_fmt(format_args!("{}: {}", what, e.as_str()));
}
