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
//! It also holds what the drivers share beyond the protocol, found repeated when the second driver (the
//! Pis' PWM jack) arrived: the test tone (`sine`) and the settings file (`settings`); and the system
//! sounds (`sounds`), so both play the same ones.
//!
//! No `unsafe`, and `no_std`.
#![no_std]
#![deny(unsafe_code)]

pub mod settings;
pub mod sine;
pub mod sounds;
pub mod wire;
