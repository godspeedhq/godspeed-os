// SPDX-License-Identifier: Apache-2.0
//! A program with the screen and the keyboard to itself while it runs: an `app`.
//!
//! An app is a utility ([`crate::program`]) that, for as long as it runs, OWNS the console: what it prints is the
//! screen, and the keys typed are its, not the shell's. When it ends - by [`App::close`], or by dying - the
//! console goes back to the shell, which draws a fresh prompt. That is the mechanism the built-in full-screen
//! tools (`chaos`, `observe`) already use: the console foreground (syscall 40), released by the kernel when its
//! holder dies, so a crashed app cannot take the keyboard with it.
//!
//! ```ignore
//! use godspeed::{self as gs, ServiceContext};
//!
//! #[allow(unsafe_code)]
//! #[no_mangle]
//! pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
//!     let mut buf = [0u8; gs::program::ARGS_MAX];
//!     let _args = gs::program::args(&ctx, &mut buf);
//!     let app = gs::app::App::open(&ctx);
//!     app.println("hello - press any key");
//!     let _ = app.wait_key();
//!     app.close(0)
//! }
//! ```
//!
//! **What it is given to do this**: the console, which the operator approves at `godspeed install` - "it reads
//! the keyboard while it runs" - because keystrokes are commands (CLAUDE.md 6.4, SEC-2). Nothing else.

use crate::ServiceContext;

/// How often [`App::wait_key`] looks for a key.
const KEY_POLL_MS: u64 = 20;

/// The console, held by this program until [`App::close`].
pub struct App<'c> {
    ctx: &'c ServiceContext,
}

impl<'c> App<'c> {
    /// Take the console: its keys are this program's from now on, they are not echoed, and the screen is cleared.
    pub fn open(ctx: &'c ServiceContext) -> App<'c> {
        ctx.claim_console_foreground();
        ctx.console_echo(false);
        crate::io::print(ctx, "\x1b[2J\x1b[H");
        App { ctx }
    }

    /// Print `s` where the cursor is.
    pub fn print(&self, s: &str) { crate::io::print(self.ctx, s); }

    /// Print `s` and a newline.
    pub fn println(&self, s: &str) { crate::io::println(self.ctx, s); }

    /// Print formatted text.
    pub fn print_fmt(&self, args: core::fmt::Arguments) { crate::io::print_fmt(self.ctx, args); }

    /// Clear the screen and put the cursor at the top left.
    pub fn clear(&self) { crate::io::print(self.ctx, "\x1b[2J\x1b[H"); }

    /// A key, if one was typed: does not wait.
    pub fn key(&self) -> Option<u8> { self.ctx.try_console_read() }

    /// The next key typed: waits for it.
    pub fn wait_key(&self) -> u8 {
        loop {
            if let Some(k) = self.ctx.try_console_read() { return k; }
            crate::task::sleep_ms(self.ctx, KEY_POLL_MS);
        }
    }

    /// Give the console back to the shell and end this program with `status`.
    pub fn close(self, status: u32) -> ! {
        self.ctx.console_echo(true);
        self.ctx.release_console_foreground();
        crate::program::exit(self.ctx, status)
    }
}
