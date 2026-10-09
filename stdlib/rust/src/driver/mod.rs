// SPDX-License-Identifier: Apache-2.0
//! `gs::driver` - the device-neutral mechanisms a driver is built from.
//!
//! # What belongs here, and what never does
//!
//! A driver describes a device. This module holds what is left when the device is taken away: the
//! mechanisms every driver needs and no driver should have to reinvent. The question that decides
//! membership is the one `docs/driver-library.md` opens with - **does this part describe the device,
//! or a reusable Godspeed mechanism?** A register sequence, a command set, a firmware's quirks and a
//! chip's recovery dance describe the device and stay in its driver. How long to wait for a register,
//! how to say the wait gave up, how long to hold still when no register can say, and how to wait for an
//! interrupt without losing a request, describe neither - so they are here (`wait`, `delay`, `irq`).
//!
//! **There are no device CLASSES here, and there will not be.** No driver::wifi, driver::audio or
//! driver::usb: a class API is a guess about what every device of that kind will need, made before
//! most of them exist. A family of devices that genuinely shares a protocol gets a domain library of
//! its own outside the standard library (`sdk/wifi` is the first), built on this module.
//!
//! # How a mechanism gets in
//!
//! By being REPEATED, not by being imagined. Each module here was found written out by hand in more
//! than one driver, differently each time, and then moved here and the drivers converted to it - except `irq`, whose originals (`xhci`, `ehci`,
//! `dwc2`) are not converted yet; its users so far are the two audio drivers. A
//! second, independent kind of driver using it naturally is the test of whether it is general; one
//! that has to be bent to fit it means the abstraction is shaped like the driver it came from.
//!
//! # Unsafe
//!
//! None, as everywhere in this crate (`#![deny(unsafe_code)]`). A driver that seems to need `unsafe`
//! is asking a question - **which safe mechanism is missing from here?** - and the answer belongs in
//! this module or the SDK's audited layer (CLAUDE.md 18.1), never in the driver.
//!
//! # What it does not change
//!
//! The kernel. Nothing here is a new kernel responsibility or a new syscall; these are the mechanisms
//! the kernel already offers, put in the one shape a driver should reach for.

pub mod delay;
pub mod irq;
pub mod wait;
