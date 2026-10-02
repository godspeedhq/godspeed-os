// SPDX-License-Identifier: GPL-2.0-only
//! The chip-independent half of a WiFi driver, shared by every radio GodspeedOS drives.
//!
//! # Why this exists
//!
//! The Pi 4's Broadcom CYW43455 was the first radio, and its driver grew everything a station needs in
//! one service. Two more radios are on the bench: the VisionFive 2 Lite's AICSemi AIC8800D80 (SDIO,
//! full-MAC, like the Broadcom) and the Pi 2's Realtek RTL8188CUS USB dongle (soft-MAC). What differs
//! between them is the bus, the firmware upload and the language the firmware speaks. What does NOT
//! differ is everything a station does above that - and that is what lives here, written once:
//!
//! - [`crypto`]: SHA-1, HMAC, PBKDF2, the 802.11 PRF, AES-128 and the RFC 3394 key unwrap, each checked
//!   against its published vector at start.
//! - [`eapol`]: the WPA2 four-way handshake and the group-key rekey, run by the HOST on every radio.
//! - [`keyfile`]: the credentials a join earned, kept in `/wifi.keys` across a restart.
//! - [`sdio`]: the SDIO card protocol and the [`sdio::SdioHost`] trait every SDIO controller implements,
//!   so the radios' code runs on the Pi 4's Arasan and the VisionFive 2's DesignWare host alike.
//! - [`wire`]: the request/reply vocabulary between a radio driver and its clients (the shell's `wifi`,
//!   `nic-driver`), so the shell and the driver read ONE definition rather than two copies of it.
//!
//! # Why it is not in the SDK
//!
//! The SDK (`sdk/rust`) is the operating system's interface: syscalls, capabilities, IPC, and the audited
//! `unsafe` that wraps registers and DMA for every service. 802.11 and WPA2 are not that, and putting them
//! there would grow the surface every service links, and the audited layer with it, by a domain only the
//! radio drivers use. This is an ordinary library beside it, with no `unsafe` at all.
//!
//! It is a crate rather than one service with vendor backends because the radios will not all live in one
//! service: on the Pi 2 a USB device's driver sits behind the `dwc2` host service (the `smsc95xx` does),
//! and a library serves both shapes.
#![no_std]
#![deny(unsafe_code)]

pub mod crypto;
pub mod eapol;
pub mod keyfile;
pub mod sdio;
pub mod wire;
