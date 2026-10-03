// SPDX-License-Identifier: GPL-2.0-only
//! What the audio driver and the shell's `audio` utility say to each other (`docs/audio.md`).
//!
//! # Why it is a crate
//!
//! The driver writes these bytes and the shell reads them, so they are ONE definition read by both - the
//! shape `sdk/wifi`'s `wire` gave the radio after the shell's hand-kept mirror of it drifted. It is not in
//! the SDK, which is the operating system's interface, and not in `gs::driver`, which holds no device
//! classes (`docs/driver-library.md`); it is a family's protocol, beside them.
//!
//! No `unsafe`, no dependencies, and `no_std`: it is constants and byte layouts.
#![no_std]
#![deny(unsafe_code)]

pub mod wire;
