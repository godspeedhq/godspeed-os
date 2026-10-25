// SPDX-License-Identifier: Apache-2.0
//! A driver's INPUT: handing what the user typed to the console, and saying input is up.
//!
//! # Why this exists
//!
//! A keyboard driver's job ends at the console's input ring: each byte it decodes is pushed there and
//! reaches the shell exactly as a serial keystroke does. `xhci`, `ehci` and `dwc2` each reached past the
//! standard library for it, with the SDK's `ctx.console_push`; this is the same call, in the shape the
//! promise covers (`docs/driver-library.md`, step 5).
//!
//! # What it is, and is not
//!
//! Authority the kernel granted at spawn, `CONSOLE_PUSH`, and nothing more. Without it both calls do
//! nothing, which is the SDK's behaviour unchanged. A keyboard's keystrokes are commands, so a holder is
//! inside the shell's trust perimeter (CLAUDE.md 6.4, SEC-2); this module adds no authority to that.
//! What a key MEANS - the HID usage table, auto-repeat - is the device's and stays out of here.
//!
//! ```ignore
//! gs::driver::input::push(&ctx, b'a');
//! gs::driver::input::ready(&ctx); // once, when setup is finished either way
//! ```

#[cfg(not(test))]
use godspeed_sdk::service_context::ServiceContext;

/// Push one byte into the console's input ring, as if it were typed on the serial console. Does nothing
/// for a driver that was not granted `CONSOLE_PUSH`.
#[cfg(not(test))]
#[inline(always)]
pub fn push(ctx: &ServiceContext, byte: u8) {
    ctx.console_push(byte)
}

/// Say that this driver has just printed a notice over the console (a device plugged in or out), so the
/// program reading it can put its line back: pushes `NOTICE_SIGNAL` (`godspeed_sdk::hid`), which no key
/// produces. Not an Enter, which would answer a question or run a half-typed line. Does nothing without
/// `CONSOLE_PUSH`.
#[cfg(not(test))]
#[inline(always)]
pub fn notice_shown(ctx: &ServiceContext) {
    ctx.console_push(godspeed_sdk::hid::NOTICE_SIGNAL)
}

/// Say that input setup is finished - the end-of-boot signal the shell watches to clear the boot screen.
/// Called in every terminal path of a keyboard driver's setup, found or not. Does nothing without
/// `CONSOLE_PUSH`.
#[cfg(not(test))]
#[inline(always)]
pub fn ready(ctx: &ServiceContext) {
    ctx.signal_input_ready()
}
