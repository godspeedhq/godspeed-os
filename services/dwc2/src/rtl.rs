// SPDX-License-Identifier: GPL-2.0-only
//! Realtek RTL8188CUS / RTL8192CU USB WiFi - **the host's side: bind the dongle, answer for it.**
//!
//! Since U1 (`docs/wifi-usb.md`) the driver is the `wifi-usb` service, and this file is what the USB host
//! owes it: the dongle matched by VID:PID and bound as THE radio, and `godspeed_wifi::usbfn` served for
//! that one device - who it is, and its control transfers - and for nothing else on the bus. The chip's
//! registers, firmware and 802.11 are `wifi-usb`'s; this file builds no Realtek request of its own except
//! milestone 1's two reads at bind, kept because they are the line a board log is checked against.
//!
//! The host TELLS the driver when the binding changes (`usbfn::NOTE_RADIO`, `notify_driver`), so the
//! driver blocks rather than asking on a timer (U1b).
//!
//! What follows is milestone 1's account, as it was written. Its "likely end state" - a separate driver
//! service and a narrow USB-transfer protocol in `dwc2` - is what U1 built; "not built now" is history.
//!
//! `docs/wifi.md` is the design and `utilities/56_wifi.md` the command surface. This file is the very
//! first step of the Pi 2 (soft-MAC) path that document defers as phase 6, and it deliberately claims
//! nothing beyond its title: it reads two registers over the vendor control interface and prints what
//! came back. No PHY init, no firmware, no scan, no association, no crypto.
//!
//! **WHY THAT IS THE RIGHT FIRST INCREMENT.** Everything above it is large - the PHY/RF register tables,
//! IQ calibration, a ~16 KB firmware blob, an 802.11 management state machine, and WPA2-PSK host-side in
//! a tree with no cryptography - and all of it is wasted if register access does not work. One readable
//! register separates "we can talk to this chip" from "we cannot", and that question has one answer
//! which no amount of design settles.
//!
//! **WHY IT LIVES IN `dwc2` RATHER THAN IN ITS OWN SERVICE**, which is a real architectural choice and
//! not an oversight. On this board `dwc2` owns the USB bus: there is no protocol by which another
//! service could issue a control transfer, and inventing a bus-passthrough surface to host one driver
//! is the speculative abstraction 26.2 forbids. The precedent is already here - the LAN9514's ethernet
//! function is `net.rs`, inside this service, matched by VID:PID for exactly the same reason this is
//! (the device reports class 0xff, so there is no class to match on).
//!
//! That said, the honest note for later: a radio is a great deal larger than `smsc95xx`, and when the
//! 802.11 MAC and a supplicant exist they do NOT belong in the service that also owns the keyboard and
//! the disk. The likely end state is a `wifi-driver` service plus a narrow USB-transfer protocol in
//! `dwc2`, and `docs/wifi.md` section 2's split is what it would follow. Not built now, because one
//! register read does not justify a new IPC surface.
//!
//! **REGISTER ACCESS.** The RTL8192CU family exposes its whole register file through a single vendor
//! control request rather than through MMIO: `bRequest = 0x05`, `wValue` = the register offset, and the
//! direction in `bmRequestType` (`0xC0` read, `0x40` write). Reimplemented from the documented behaviour
//! of that interface (rtlwifi's `_usbctrl_vendorreq` and rtl8xxxu's equivalent are the reference for
//! WHAT the silicon wants, per 26.14); no code copied, and the model here is ours.

use godspeed_sdk::{CapHandle, Dma, Message, Mmio, ServiceContext};
use godspeed as gs;
use gs::driver::{delay, wait::Budget};
use godspeed_wifi::usbfn;

use crate::chan::{self, Target};

/// The dongle on the Pi 2's hub, confirmed on hardware twice - `bugs/3` recorded it via this driver and
/// the T630's `xhci` read the same pair on 2026-09-27.
pub const VID: u16 = 0x0bda;
pub const PID: u16 = 0x8176;

/// The vendor request every register access rides on.
const VENDOR_REQ: u8 = 0x05;
/// Device-to-host, type vendor, recipient device.
const DIR_IN: u8 = 0xC0;

/// `REG_SYS_CFG` - carries the chip version and vendor bits. The canonical "is this part alive and
/// which part is it" read for this family.
const REG_SYS_CFG: u16 = 0x00F0;
/// `REG_SYS_ISO_CTRL` - the isolation control at offset 0, read as a second, independent sample.
const REG_SYS_ISO_CTRL: u16 = 0x0000;

/// Read a 32-bit register over the vendor control interface.
///
/// RETRIED, for the reason recorded at the enumeration retry in `hub.rs`: this controller sequences
/// transfers in software, so a single XACTERR is a transient on a contended bus rather than a verdict,
/// and the SAME dongle needed a retry to enumerate on 2026-09-27. Bounded and reported (26.6).
fn read32(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, reg: u16) -> Option<u32> {
    const TRIES: u32 = 4;
    let setup = [
        DIR_IN,
        VENDOR_REQ,
        (reg & 0xFF) as u8,
        ((reg >> 8) & 0xFF) as u8,
        0,
        0,
        4,
        0,
    ];
    for _ in 0..TRIES {
        let mut buf = [0u8; 4];
        if chan::control(ctx, mmio, dma, t, &setup, &mut buf, true, 4) {
            return Some(u32::from_le_bytes(buf));
        }
        delay::hold(ctx, Budget::ms(5));
    }
    None
}

/// Tell the radio's driver its binding changed (`usbfn::NOTE_RADIO`): `try_send`, never blocking on a driver
/// that is behind, and reacquired by name once if the cap is stale - `wifi-usb` is spawned after this
/// service, so at the first bind it may not have been in the name map yet. Said in the log when it cannot be
/// delivered at all: the driver then learns at its next start, which is the bound on how stale it can be.
pub fn notify_driver(ctx: &ServiceContext) {
    if !tell(ctx) {
        ctx.log("dwc2-svc: could not tell wifi-usb the radio's binding changed (not running, or its queue is full)");
    }
}

/// The same notice, once, at the end of this service's boot enumeration - bound or not. On a respawn it is
/// the only way the driver learns the dongle is gone if it did not come back: nothing was bound, so nothing
/// else would say. QUIET when it cannot be delivered, because on a first boot the driver is not running
/// yet - it is spawned after this service - and its own `OP_INFO` at start covers that case.
pub fn notify_driver_at_start(ctx: &ServiceContext) {
    let _ = tell(ctx);
}

fn tell(ctx: &ServiceContext) -> bool {
    let msg = Message::from_bytes(&[usbfn::NOTE_RADIO]);
    gs::ipc::try_send(ctx, DRIVER, &msg).is_ok()
        || (gs::cap::reacquire(ctx, DRIVER) && gs::ipc::try_send(ctx, DRIVER, &msg).is_ok())
}

/// The radio's driver, the one service this host tells about the radio.
const DRIVER: &str = "wifi-usb";

/// Control transfers tried before the host says FAILED. The reason `read32` gives: this controller sequences
/// transfers in software, so one XACTERR is a transient on a contended bus, not a verdict.
const CONTROL_TRIES: u32 = 4;

/// Serve one `usbfn` request for the bound radio - `radio` is its target, VID and PID, or `None` when none is
/// bound - and answer on `reply`, which this reclaims. Every op is answered, a request for a radio that is
/// not here included, so a client is never left to time out against a clean log.
pub fn serve(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, radio: Option<&(Target, u16, u16)>,
    msg: &Message, reply: CapHandle,
) {
    let p = msg.payload_bytes();
    let op = p.first().copied().unwrap_or(0);
    let mut out = [0u8; 2 + usbfn::CONTROL_MAX];
    out[0] = op;
    let n = match (op, radio) {
        (_, None) => {
            out[1] = usbfn::ST_NO_DEVICE;
            2
        }
        (usbfn::OP_INFO, Some((_, vid, pid))) => {
            out[1] = usbfn::ST_OK;
            out[2..4].copy_from_slice(&vid.to_le_bytes());
            out[4..6].copy_from_slice(&pid.to_le_bytes());
            6
        }
        (usbfn::OP_CONTROL, Some((t, _, _))) => control(ctx, mmio, dma, t, p, &mut out),
        _ => {
            out[1] = usbfn::ST_BAD_REQUEST;
            2
        }
    };
    let _ = ctx.try_send_by_handle(reply, &Message::from_bytes(&out[..n]));
    ctx.remove_cap(reply);
}

/// One control transfer: `p` is `[op, setup(8), data out...]`. Fills `out[1..]` and returns its length.
fn control(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, p: &[u8], out: &mut [u8]) -> usize {
    if p.len() < 9 {
        out[1] = usbfn::ST_BAD_REQUEST;
        return 2;
    }
    let mut setup = [0u8; 8];
    setup.copy_from_slice(&p[1..9]);
    let len = u16::from_le_bytes([setup[6], setup[7]]) as usize;
    let data_in = setup[0] & 0x80 != 0;
    if len > usbfn::CONTROL_MAX || (!data_in && p.len() < 9 + len) {
        out[1] = usbfn::ST_BAD_REQUEST;
        return 2;
    }
    let mut buf = [0u8; usbfn::CONTROL_MAX];
    if !data_in {
        buf[..len].copy_from_slice(&p[9..9 + len]);
    }
    for _ in 0..CONTROL_TRIES {
        if chan::control(ctx, mmio, dma, t, &setup, &mut buf, data_in, len) {
            out[1] = usbfn::ST_OK;
            if data_in {
                out[2..2 + len].copy_from_slice(&buf[..len]);
                return 2 + len;
            }
            return 2;
        }
        delay::hold(ctx, Budget::ms(5));
    }
    out[1] = usbfn::ST_FAILED;
    2
}

/// Milestone 1: prove the chip answers. Returns true only if BOTH reads came back with a value that
/// could be a register rather than a bus failure.
///
/// **WHAT IS AND IS NOT DECODED.** The raw values are printed and nothing is interpreted, deliberately.
/// The version and vendor bits of `SYS_CFG` are decodable, but doing it from memory rather than from the
/// datasheet would put a confident wrong number in a boot log - and a wrong decode is worse than no
/// decode, because the next reader trusts it. The bits get named when the datasheet is in hand.
///
/// What the reads DO establish, which is the whole point of the increment:
///   * the vendor control interface works in both directions of setup,
///   * the chip is powered and clocked enough to answer,
///   * and the two offsets differ, so the answer is a register file rather than one latched value.
///
/// `0x00000000` and `0xFFFFFFFF` are both treated as failures: a floating bus and a dead chip read as
/// those, and accepting either would let this report success on a device that never replied. That is
/// the instrument-that-agrees-with-you failure this project keeps finding, so it is refused up front.
pub fn probe(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target) -> bool {
    let cfg = read32(ctx, mmio, dma, t, REG_SYS_CFG);
    let iso = read32(ctx, mmio, dma, t, REG_SYS_ISO_CTRL);

    match (cfg, iso) {
        (Some(c), Some(i)) => {
            ctx.log_fmt(format_args!(
                "dwc2-svc: RTL8188CUS at {:04x}:{:04x} - SYS_CFG(0xF0)={:#010x} ISO_CTRL(0x00)={:#010x}",
                VID, PID, c, i
            ));
            let plausible = |v: u32| v != 0 && v != 0xFFFF_FFFF;
            if plausible(c) && plausible(i) {
                ctx.log("dwc2-svc: RTL8188CUS register reads OK - the chip answers (milestone 1 of the \
                         WiFi work; no PHY, no firmware, no association - docs/wifi.md)");
                true
            } else {
                // Values that cannot be a register file. Said plainly rather than counted as a pass:
                // a floating bus reads as all-zeros or all-ones and would otherwise look like success.
                ctx.log("dwc2-svc: RTL8188CUS answered, but with all-zeros or all-ones - that is a bus \
                         or power fault, NOT a register file");
                false
            }
        }
        _ => {
            ctx.log_fmt(format_args!(
                "dwc2-svc: RTL8188CUS at {:04x}:{:04x} did not answer a vendor register read after 4 \
                 tries - no register access, so nothing above it can be attempted",
                VID, PID
            ));
            false
        }
    }
}
