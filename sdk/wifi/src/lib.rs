// SPDX-License-Identifier: GPL-2.0-only
//! The chip-independent half of a WiFi driver, shared by every radio GodspeedOS drives.
//!
//! # Why this exists
//!
//! The Pi 4's Broadcom CYW43455 was the first radio, and its driver grew everything a station needs in
//! one service. Two more radios followed: the VisionFive 2 Lite's AICSemi AIC8800D80 (SDIO, full-MAC,
//! like the Broadcom) and a Realtek RTL8188CUS USB dongle (soft-MAC), behind `dwc2` or `xhci`. What differs
//! between them is the bus, the firmware upload and the language the firmware speaks. What does NOT
//! differ is everything a station does above that - and that is what lives here, written once:
//!
//! - [`crypto`]: SHA-1, HMAC, PBKDF2, the 802.11 PRF, AES-128 and the RFC 3394 key unwrap, each checked
//!   against its published vector at start.
//! - [`eapol`]: EAPOL-Key frames - recognised, signed, verified, unwrapped.
//! - [`supplicant`]: the WPA2 four-way handshake and the group-key rekey, run by the HOST on every radio,
//!   over the [`supplicant::KeyPath`] each radio supplies.
//! - [`keyfile`]: the credentials a join earned, kept in `/wifi.keys` across a restart.
//! - [`data`]: an 802.11 data frame and the ethernet frame it carries, both ways, for every radio whose
//!   host sees raw frames.
//! - [`mgmt`]: a beacon or probe response read from the raw 802.11 frame - BSSID, capability, SSID and
//!   channel - for every radio whose firmware forwards the frames rather than digesting them.
//! - [`station`]: the [`station::Station`] trait - what the serve loop asks of a radio - and the types it
//!   speaks in; [`bss`] (a scan's networks and their wire records) and [`rxq`] (received frames) with it.
//! - [`serve`]: THE serve loop - what the shell's `wifi` and `nic-driver`'s frame ops are answered by,
//!   the scan cache, the credential table, auto-join - run by every radio service over its `Station`.
//!   It lived in `wifi-driver` until the USB dongle's driver, a second service, needed it (2026-10-06).
//! - [`sdio`]: the SDIO card protocol and the [`sdio::SdioHost`] trait every SDIO controller implements,
//!   so the radios' code runs on the Pi 4's Arasan and the VisionFive 2's DesignWare host alike.
//! - [`wire`]: the request/reply vocabulary between a radio driver and the shell's `wifi`, so the shell
//!   and the driver read ONE definition rather than two copies of it.
//! - [`usbfn`]: what a USB host service answers for the one device it has bound as a radio, and what
//!   `wifi-usb` asks - again one definition for both sides.
//!
//! # Why it is not in the SDK
//!
//! The SDK (`sdk/rust`) is the operating system's interface: syscalls, capabilities, IPC, and the audited
//! `unsafe` that wraps registers and DMA for every service. 802.11 and WPA2 are not that, and putting them
//! there would grow the surface every service links, and the audited layer with it, by a domain only the
//! radio drivers use. This is an ordinary library beside it, with no `unsafe` at all.
//!
//! It is a crate rather than one service with vendor backends because the radios will not all live in one
//! service: the USB dongle has its own, `wifi-usb`, reaching the chip through its USB host service (`dwc2`, or `xhci`)
//! (`docs/wifi-usb.md`), and a library serves both shapes.
//!
//! # Known gap
//!
//! Log lines in this crate were prefixed `wifi-driver:`, the only radio service until `wifi-usb`. The
//! second caller arrived with R4 (2026-10-06), and every module it reaches now takes the service's name:
//! [`serve`], [`keyfile`] and [`crypto::selftest`] at R4, [`supplicant`] and [`eapol`] at R5c. [`sdio`] still
//! says `wifi-driver:`, and is right to - only `wifi-driver` drives an SDIO bus.
#![no_std]
#![deny(unsafe_code)]

pub mod bss;
pub mod crypto;
pub mod data;
pub mod eapol;
pub mod keyfile;
pub mod mgmt;
pub mod rxq;
pub mod serve;
pub mod sdio;
pub mod station;
pub mod supplicant;
pub mod usbfn;
pub mod wire;
