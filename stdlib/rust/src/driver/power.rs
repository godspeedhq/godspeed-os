// SPDX-License-Identifier: Apache-2.0
//! A driver's POWER: cutting and restoring the power of the device it was granted.
//!
//! # Why this exists
//!
//! A device that has stopped answering can need its power cut to come back - a chip whose firmware has
//! trapped will not boot a new one from any host-side reset (`docs/wifi.md` 45-47). `wifi-driver` reached
//! past the standard library for it, with the SDK's `ctx.device_power`; this is the same call, in the
//! shape the promise covers (`docs/driver-library.md`, step 5).
//!
//! # What it is, and is not
//!
//! The kernel's `DevicePower` (CLAUDE.md 12.3, amendment 2026-10-01): it drives the pin the arch layer
//! names for the device THIS driver was granted, and only where the machine can. Everywhere else it
//! answers `false` and the kernel says why on the serial log. How long to hold the device off and how
//! long to wait after restoring it are the device's, and stay in its driver.
//!
//! ```ignore
//! if gs::driver::power::set(&ctx, false) {
//!     gs::driver::delay::hold_parked(&ctx, Budget::ms(50));   // the device's own hold-off
//!     let _on = gs::driver::power::set(&ctx, true);
//! }
//! ```

#[cfg(not(test))]
use godspeed_sdk::service_context::ServiceContext;

/// Cut (`false`) or restore (`true`) the power of this driver's device. `true` only when the kernel
/// read the power pin back at the level asked for; `false` where the machine cannot power the device,
/// or the driver holds no `DEVICE_POWER`.
#[cfg(not(test))]
#[inline(always)]
#[must_use = "the device's power did NOT change if this is false"]
pub fn set(ctx: &ServiceContext, on: bool) -> bool {
    ctx.device_power(on)
}
