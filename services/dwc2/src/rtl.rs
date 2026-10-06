// SPDX-License-Identifier: GPL-2.0-only
//! Realtek RTL8188CUS / RTL8192CU USB WiFi - **the host's side: bind the dongle, answer for it.**
//!
//! Since U1 (`docs/wifi-usb.md`) the driver is the `wifi-usb` service, and this file is what the USB host
//! owes it: the dongle matched by VID:PID and bound as THE radio, and `godspeed_wifi::usbfn` served for
//! that one device - who it is, and its control transfers - and for nothing else on the bus. The chip's
//! registers, firmware and 802.11 are `wifi-usb`'s; this file builds no Realtek request of its own except
//! milestone 1's two reads at bind, kept because they are the line a board log is checked against.
//!
//! The host TELLS the driver when the binding changes (`usbfn::NOTE_RADIO`, `announce`), so the
//! driver blocks rather than asking on a timer (U1b).
//!
//! **Receive (R3b):** one bulk IN on `CH_RADIO_RX`, armed in the background once `wifi-usb` first asks
//! `OP_BULK_IN`, taken on the USB interrupt (`service`), held until collected, with `usbfn::NOTE_BULK_IN`
//! sent to say so. It stands aside for every other non-periodic transfer (`chan::program_ping`) and is put
//! back from the packet it reached, with its toggle.
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

use godspeed_sdk::{Dma, Message, Mmio, ServiceContext};
use godspeed_sdk::service_context::usbdev;
use godspeed as gs;
use gs::cap::Cap;
use gs::driver::{delay, wait::{self, Budget}};
use godspeed_wifi::usbfn;

use crate::chan::{self, Target, CH_RADIO_RX};
use crate::regs::{
    GAHBCFG, GAHBCFG_GLBLINTRMSK, GINTMSK, GINTMSK_HCHINT, GINTSTS, HAINTMSK, HCINT_CHHLTD, HCINT_STALL,
    HCINT_XFERCOMPL,
};

/// THE BOUND RADIO: who it is, and its receive. `ep_in` is 0 when the dongle could not be configured or
/// offered no high-speed bulk IN - control transfers are then still served, and `OP_BULK_IN` says FAILED.
pub struct Radio {
    pub t: Target,
    pub vid: u16,
    pub pid: u16,
    /// This bind's number on this host (`usbdev::Report::gen`): the same dongle bound again reads as a new
    /// binding, not as the one before it.
    pub gen: u32,
    ep_in: u8,
    in_mps: u16,
    rx: Rx,
    /// The IN endpoint's data toggle, read back from the channel after every halt - the hardware owns it,
    /// and a halt that does not carry it destroys the next packet (`net::tx` records the cost of that).
    pid_in: u32,
    /// A `NOTE_BULK_IN` the driver's queue refused, sent again on a later pass: nothing is armed until the
    /// held transfer is collected, so a notice lost for good would stop receive for good.
    note_owed: bool,
    /// Notices the driver took in place of an answer and named in an `OP_SYNC` (`usbfn::OP_SYNC`), sent
    /// again once it has been quiet for `DRIVER_QUIET_MS` - so they reach its serve loop, not its next call.
    sync_bulk: bool,
    sync_radio: bool,
    /// When the driver last asked anything, for that quiet.
    last_req: Option<wait::Since>,
    /// Consecutive transaction errors on the IN, for `RX_ERROR_TRIES`.
    errs_run: u32,
    pub stats: RxStats,
    /// The bulk OUT endpoints, in configuration-descriptor order (`usbfn::OP_BULK_OUT`'s `out`), their
    /// packet size, and each one's data toggle - carried forward from the channel after every transfer, as
    /// the disk's are (`msc::bulk_xfer`).
    outs: [u8; MAX_OUT],
    n_out: usize,
    out_mps: u16,
    pid_out: [u32; MAX_OUT],
    pub tx: TxStats,
}

/// What has been sent to the radio, for the heartbeat.
#[derive(Default)]
pub struct TxStats {
    pub frames: u32,
    pub bytes: u32,
    pub failed: u32,
}

/// The most bulk OUT endpoints kept: Linux's `RTL8XXXU_OUT_ENDPOINTS` is 6 for its whole family; the
/// RTL8188CUS has two or three, so three is the bound here, and a fourth is not addressable.
const MAX_OUT: usize = 3;
/// Where a frame to send is staged: after the radio's receive area, inside the 64 KiB arena.
const RADIO_TX_OFF: usize = 0x5000;
const _: () = assert!(RADIO_TX_OFF >= RADIO_RX_OFF + usbfn::BULK_IN_MAX);
const _: () = assert!(RADIO_TX_OFF + usbfn::BULK_OUT_MAX <= 64 * 1024);
/// How long one frame may take to go out: a bulk OUT the chip NAKs is it pacing us (its transmit FIFO full),
/// and a frame is milliseconds; past this the frame is reported not sent.
const RADIO_TX_BUDGET_MS: u64 = 200;

/// The radio's endpoints, as `configure` found them.
struct Eps {
    ep_in: u8,
    in_mps: u16,
    outs: [u8; MAX_OUT],
    n_out: usize,
    out_mps: u16,
}

/// Where the radio's receive is. `Armed { at }`: the IN is programmed to land at `RADIO_RX_OFF + at`, `at` being
/// what a transfer that was stood aside had already received. `Held(n)`: a transfer of `n` bytes is waiting
/// for `wifi-usb` to collect it, and the IN is NOT armed.
#[derive(Clone, Copy, PartialEq)]
enum Rx {
    Off,
    Armed { at: usize },
    Held(usize),
}

/// What the radio's receive has done, for the heartbeat report. Counts, never reset: a rate is two reports.
#[derive(Default, Clone, Copy)]
pub struct RxStats {
    pub transfers: u32,
    pub bytes: u32,
    /// Times the IN stood aside for another transfer and was put back (`chan::program_ping`).
    pub asides: u32,
    pub errors: u32,
    pub last_err: u32,
    pub notes_late: u32,
    /// `OP_SYNC`s: notices the driver received in place of an answer (it has no reply mailbox).
    pub syncs: u32,
}

/// Where the radio's transfers land: after the NIC's receive burst, inside the 64 KiB arena. Derived and
/// asserted, as `net.rs` does its own, because an overlap here is a device writing into another's buffer.
const RADIO_RX_OFF: usize = 0x4000;
const _: () = assert!(RADIO_RX_OFF >= crate::net::RX_OFF + crate::net::RX_BURST);
const _: () = assert!(RADIO_RX_OFF + usbfn::BULK_IN_MAX <= 64 * 1024);

/// Transaction errors in a row on the IN before what it had received is dropped and it starts again: the
/// figure `chan::stage` takes from Linux's `dwc2_release_channel` (the third error fails the transfer).
const RX_ERROR_TRIES: u32 = 3;

/// The descriptor kinds and the bulk transfer type, as USB 2.0 chapter 9 numbers them.
const DESC_CONFIG: u8 = 0x02;
const DESC_ENDPOINT: u8 = 0x05;
const EP_TYPE_BULK: u8 = 0x02;
/// A high-speed bulk endpoint's packet size, the only one USB 2.0 allows it. A smaller one means the dongle
/// came up at full speed behind the hub, which needs split transactions this driver does not run for a bulk
/// IN - so receive stays off and says why, rather than half-working.
const HS_BULK_MPS: u16 = 512;

/// Bind the dongle as the radio: configured, as Linux's USB core configures a device before a driver sees
/// it - this host never did, and control transfers worked without it, but a bulk endpoint exists only in a
/// configured device - then milestone 1's two reads, then its bulk IN found. Bound whatever happens, so
/// `wifi-usb` reaches its registers and says itself what failed.
pub fn bind(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, vid: u16, pid: u16, gen: u32) -> Radio {
    stop(ctx, mmio);
    let e = configure(ctx, mmio, dma, t)
        .unwrap_or(Eps { ep_in: 0, in_mps: 0, outs: [0; MAX_OUT], n_out: 0, out_mps: 0 });
    let _ = probe(ctx, mmio, dma, t);
    Radio {
        t: *t, vid, pid, gen, ep_in: e.ep_in, in_mps: e.in_mps, rx: Rx::Off, pid_in: chan::PID_DATA0, note_owed: false,
        sync_bulk: false, sync_radio: false, last_req: None,
        errs_run: 0, stats: RxStats::default(),
        outs: e.outs, n_out: e.n_out, out_mps: e.out_mps, pid_out: [chan::PID_DATA0; MAX_OUT],
        tx: TxStats::default(),
    }
}

/// Read the configuration descriptor, find the bulk IN endpoint and the bulk OUTs, and SET_CONFIGURATION.
/// `None`, said, on any failure; an `ep_in` of 0 when it configured but has no high-speed bulk IN.
fn configure(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target) -> Option<Eps> {
    const TRIES: u32 = 4;
    let ctl = |setup: &[u8; 8], buf: &mut [u8], data_in: bool, len: usize| -> bool {
        (0..TRIES).any(|i| {
            if i > 0 {
                delay::hold(ctx, Budget::ms(5));
            }
            chan::control(ctx, mmio, dma, t, setup, buf, data_in, len)
        })
    };
    let mut head = [0u8; 9];
    if !ctl(&[0x80, 0x06, 0, DESC_CONFIG, 0, 0, 9, 0], &mut head, true, 9) {
        ctx.log("dwc2-svc: RTL8188CUS - the configuration descriptor did not come back; no receive");
        return None;
    }
    let want = (u16::from_le_bytes([head[2], head[3]]) as usize).min(chan::DATA_LEN);
    let cfg_val = head[5];
    let mut full = [0u8; chan::DATA_LEN];
    if !ctl(&[0x80, 0x06, 0, DESC_CONFIG, 0, 0, (want & 0xFF) as u8, (want >> 8) as u8], &mut full, true, want) {
        ctx.log("dwc2-svc: RTL8188CUS - the configuration descriptor did not come back; no receive");
        return None;
    }
    let mut none: [u8; 0] = [];
    if !ctl(&[0x00, 0x09, cfg_val, 0, 0, 0, 0, 0], &mut none, false, 0) {
        ctx.log_fmt(format_args!("dwc2-svc: RTL8188CUS - SET_CONFIGURATION {} FAILED; no receive", cfg_val));
        return None;
    }
    let (ep, mps) = bulk_in(&full, want).unwrap_or((0, 0));
    let mut e = Eps { ep_in: 0, in_mps: 0, outs: [0; MAX_OUT], n_out: 0, out_mps: 0 };
    bulk_outs(&full, want, &mut e);
    // A bulk OUT that is not 512 bytes is a full-speed dongle behind the hub, which needs splits this host
    // does not run for bulk, exactly as for the IN below: none is kept, and transmit is off, said.
    if e.n_out > 0 && e.out_mps != HS_BULK_MPS {
        ctx.log_fmt(format_args!(
            "dwc2-svc: RTL8188CUS bulk OUT is {} bytes, not high-speed - no transmit", e.out_mps));
        e.n_out = 0;
    }
    if ep == 0 || mps != HS_BULK_MPS {
        ctx.log_fmt(format_args!(
            "dwc2-svc: RTL8188CUS configured ({}) but no high-speed bulk IN (endpoint {}, {} bytes) - control only, no receive",
            cfg_val, ep, mps));
        return Some(e);
    }
    e.ep_in = ep;
    e.in_mps = mps;
    ctx.log_fmt(format_args!(
        "dwc2-svc: RTL8188CUS configured ({}) - bulk IN endpoint {}, {} bytes, received on channel {}; {} bulk OUT ({:?}), sent on channel {}",
        cfg_val, ep, mps, CH_RADIO_RX, e.n_out, &e.outs[..e.n_out], chan::CH_BULK));
    Some(e)
}

/// The bulk OUT endpoints, in the order the configuration descriptor lists them - `rtl8xxxu_parse_usb`'s
/// `out_ep[j++]` - up to `MAX_OUT`, with the packet size of the first. Every length is the device's.
fn bulk_outs(buf: &[u8], total: usize, e: &mut Eps) {
    let mut i = 0usize;
    while i + 2 <= total {
        let len = buf[i] as usize;
        if len < 2 || i + len > total {
            return;
        }
        if buf[i + 1] == DESC_ENDPOINT && len >= 7 && buf[i + 3] & 0x03 == EP_TYPE_BULK && buf[i + 2] & 0x80 == 0 {
            if e.n_out < MAX_OUT {
                if e.n_out == 0 {
                    e.out_mps = u16::from_le_bytes([buf[i + 4], buf[i + 5]]) & 0x07FF;
                }
                e.outs[e.n_out] = buf[i + 2] & 0x0F;
                e.n_out += 1;
            }
        }
        i += len;
    }
}

/// The first bulk IN endpoint in a configuration descriptor, and its packet size - Linux's
/// `rtl8xxxu_parse_usb` takes the one bulk IN the interface has. Every length is the device's, so a
/// descriptor too short to step over ends the walk.
fn bulk_in(buf: &[u8], total: usize) -> Option<(u8, u16)> {
    let mut i = 0usize;
    while i + 2 <= total {
        let len = buf[i] as usize;
        if len < 2 || i + len > total {
            return None;
        }
        if buf[i + 1] == DESC_ENDPOINT && len >= 7 && buf[i + 3] & 0x03 == EP_TYPE_BULK && buf[i + 2] & 0x80 != 0 {
            return Some((buf[i + 2] & 0x0F, u16::from_le_bytes([buf[i + 4], buf[i + 5]]) & 0x07FF));
        }
        i += len;
    }
    None
}

/// Take the radio's channel down: halted, its interrupt masked, its status cleared. Before a binding and
/// after a removal, so no IN is left armed at a device that has gone - or at the next one at its address.
pub fn stop(ctx: &ServiceContext, mmio: &Mmio) {
    chan::halt(ctx, mmio, CH_RADIO_RX);
    chan::release(mmio, CH_RADIO_RX);
    mmio.write32(HAINTMSK, mmio.read32(HAINTMSK) & !(1 << CH_RADIO_RX));
}

/// Arm the IN to land at `RADIO_RX_OFF + at`, for the rest of `BULK_IN_MAX`, and let it raise the interrupt.
fn arm(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, r: &mut Radio, at: usize) {
    let t = Target { addr: r.t.addr, mps: r.in_mps, low_speed: false };
    chan::program(ctx, mmio, &t, CH_RADIO_RX, true, r.pid_in, (usbfn::BULK_IN_MAX - at) as u32,
                  dma.phys_at(RADIO_RX_OFF + at) as u32, r.ep_in as u32, 2, 0);
    // The halt alone: a completion halts the channel too, so it is the one event that covers all of them.
    // HAINTMSK picks which channels may raise the core's HCHINT; the NIC's receive sets its own bit.
    mmio.write32(chan::hcintmsk_at(CH_RADIO_RX), HCINT_CHHLTD);
    mmio.write32(HAINTMSK, mmio.read32(HAINTMSK) | (1 << CH_RADIO_RX));
    // The core's line, as `net::arm_in` enables it - HCHINT alone, and the global enable - for a board where
    // the NIC never armed. Tested rather than flagged: the registers are the truth of whether it is on.
    if mmio.read32(GINTMSK) & GINTMSK_HCHINT == 0 {
        mmio.write32(GINTSTS, GINTMSK_HCHINT);
        mmio.write32(GINTMSK, mmio.read32(GINTMSK) | GINTMSK_HCHINT);
    }
    if mmio.read32(GAHBCFG) & GAHBCFG_GLBLINTRMSK == 0 {
        mmio.write32(GAHBCFG, mmio.read32(GAHBCFG) | GAHBCFG_GLBLINTRMSK);
    }
    r.rx = Rx::Armed { at };
}

/// Look at the radio's IN and act on a halt: a completed transfer is HELD and `wifi-usb` told; one stood
/// aside for another transfer (`chan::program_ping`) is armed again from where it stopped; a transaction
/// error is re-run from there too, up to `RX_ERROR_TRIES` in a row; a STALL stops receive, said. Called on
/// the USB interrupt and once a pass. `true` when it found a halt to retire - what lets the interrupt
/// handler know the line was this channel's.
pub fn service(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, r: &mut Radio) -> bool {
    // Notices named in an `OP_SYNC`, again, once the driver is between requests.
    if (r.sync_bulk || r.sync_radio)
        && r.last_req.as_ref().map_or(true, |t| t.passed(ctx, Budget::ms(DRIVER_QUIET_MS)))
    {
        if r.sync_radio {
            r.sync_radio = !tell(ctx);
        }
        if r.sync_bulk {
            r.sync_bulk = false;
            r.note_owed = true;
        }
    }
    if r.note_owed && matches!(r.rx, Rx::Held(_)) {
        r.note_owed = !tell_bulk(ctx);
    }
    let at = match r.rx {
        Rx::Armed { at } => at,
        _ => return false,
    };
    let hcint = mmio.read32(chan::hcint_at(CH_RADIO_RX));
    if hcint & HCINT_CHHLTD == 0 {
        return false;
    }
    mmio.write32(chan::hcint_at(CH_RADIO_RX), hcint);
    r.pid_in = chan::pid_from_hctsiz(mmio, CH_RADIO_RX);
    let left = (mmio.read32(chan::hctsiz_at(CH_RADIO_RX)) & 0x7_FFFF) as usize;
    let got = (at + (usbfn::BULK_IN_MAX - at).saturating_sub(left)).min(usbfn::BULK_IN_MAX);
    if hcint & HCINT_XFERCOMPL != 0 {
        r.errs_run = 0;
        if got == 0 {
            arm(ctx, mmio, dma, r, 0);
            return true;
        }
        r.stats.transfers = r.stats.transfers.wrapping_add(1);
        r.stats.bytes = r.stats.bytes.wrapping_add(got as u32);
        r.rx = Rx::Held(got);
        if !tell_bulk(ctx) {
            r.note_owed = true;
            r.stats.notes_late = r.stats.notes_late.wrapping_add(1);
        }
    } else if hcint & HCINT_STALL != 0 {
        r.stats.errors = r.stats.errors.wrapping_add(1);
        r.stats.last_err = hcint;
        r.rx = Rx::Off;
        ctx.log_fmt(format_args!(
            "dwc2-svc: the radio's bulk IN STALLED (HCINT={:#010x}) - receive stopped until wifi-usb asks again", hcint));
    } else if hcint & !(HCINT_CHHLTD | crate::regs::HCINT_NAK | crate::regs::HCINT_ACK) != 0 {
        r.stats.errors = r.stats.errors.wrapping_add(1);
        r.stats.last_err = hcint;
        r.errs_run += 1;
        let from = if r.errs_run >= RX_ERROR_TRIES { r.errs_run = 0; 0 } else { got };
        arm(ctx, mmio, dma, r, from);
    } else {
        r.stats.asides = r.stats.asides.wrapping_add(1);
        arm(ctx, mmio, dma, r, got);
    }
    true
}

/// `OP_BULK_IN`: the held transfer, if any, and the IN armed again. Answers on `reply` itself, with a
/// buffer of its own, so `serve`'s frame stays the size of a control transfer.
fn bulk_in_request(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, r: &mut Radio, reply: Cap) {
    let mut out = [0u8; 2 + usbfn::BULK_IN_MAX];
    out[0] = usbfn::OP_BULK_IN;
    let mut n = 0usize;
    if r.ep_in == 0 {
        out[1] = usbfn::ST_FAILED;
    } else {
        // A completion the interrupt has not been handled for yet is taken now rather than a pass later.
        let _ = service(ctx, mmio, dma, r);
        if let Rx::Held(len) = r.rx {
            for (i, b) in out[2..2 + len].iter_mut().enumerate() {
                *b = dma.read8(RADIO_RX_OFF + i);
            }
            n = len;
            r.note_owed = false;
        }
        if !matches!(r.rx, Rx::Armed { .. }) {
            arm(ctx, mmio, dma, r, 0);
        }
        out[1] = usbfn::ST_OK;
    }
    let _ = gs::ipc::reply(ctx, reply, &Message::from_bytes(&out[..2 + n]));
}

/// The heartbeat's line about the radio's receive.
/// `OP_BULK_OUT`: `p` is `[op, out, transfer...]`. Staged in the arena and sent on the bulk channel, the
/// disk's, by the same transfer the disk uses - one at a time, which is all one service can make - with the
/// endpoint's toggle carried forward. The radio's IN stands aside for it as for any bulk transfer
/// (`chan::program_ping`). A status byte.
fn bulk_out(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, r: &mut Radio, p: &[u8]) -> u8 {
    let idx = p.get(1).copied().unwrap_or(u8::MAX) as usize;
    let data = p.get(2..).unwrap_or(&[]);
    if idx >= r.n_out || data.is_empty() || data.len() > usbfn::BULK_OUT_MAX {
        return usbfn::ST_BAD_REQUEST;
    }
    for (i, &b) in data.iter().enumerate() {
        dma.write8(RADIO_TX_OFF + i, b);
    }
    let sent = crate::msc::bulk_xfer(ctx, mmio, &r.t, r.out_mps, false, r.outs[idx],
                                     dma.phys_at(RADIO_TX_OFF) as u32, data.len() as u32, RADIO_TX_BUDGET_MS,
                                     &mut r.pid_out[idx]);
    match sent {
        Ok(n) if n as usize == data.len() => {
            r.tx.frames = r.tx.frames.wrapping_add(1);
            r.tx.bytes = r.tx.bytes.wrapping_add(n);
            usbfn::ST_OK
        }
        _ => {
            r.tx.failed = r.tx.failed.wrapping_add(1);
            usbfn::ST_FAILED
        }
    }
}

pub fn report(ctx: &ServiceContext, r: &Radio) {
    let s = &r.stats;
    let state = match r.rx {
        Rx::Off if r.ep_in == 0 => "no bulk IN",
        Rx::Off => "not started",
        Rx::Armed { .. } => "armed",
        Rx::Held(_) => "held for wifi-usb",
    };
    ctx.log_fmt(format_args!(
        "dwc2-svc: radio rx - {} transfers {} bytes, {} asides, {} errors (last HCINT={:#010x}), {} notices late, {} taken as answers (OP_SYNC); {}; tx - {} frames {} bytes, {} failed",
        s.transfers, s.bytes, s.asides, s.errors, s.last_err, s.notes_late, s.syncs, state, r.tx.frames, r.tx.bytes, r.tx.failed));
}

fn tell_bulk(ctx: &ServiceContext) -> bool {
    let msg = Message::from_bytes(&[usbfn::NOTE_BULK_IN]);
    gs::ipc::try_send(ctx, DRIVER, &msg).is_ok()
        || (gs::cap::reacquire(ctx, DRIVER) && gs::ipc::try_send(ctx, DRIVER, &msg).is_ok())
}

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

/// The radio's binding changed, or the boot enumeration ended: said to the supervisor (`usbdev`), which
/// starts the dongle's driver when one is present and stops it when none is (`docs/usb-device-drivers.md`),
/// and told to the driver (`usbfn::NOTE_RADIO`) in case it is already running and should ask `OP_INFO`.
///
/// The driver's notice is QUIET when it cannot be delivered: the driver is started BY this report, so at a
/// bind it is normally not running yet, and it asks `OP_INFO` itself when it starts. The report is LOUD,
/// because without it the dongle has no driver and nothing else will say so.
pub fn announce(ctx: &ServiceContext, radio: Option<&Radio>) {
    let _ = tell(ctx);
    report(ctx, radio);
}

/// This host's report on the radio, to the supervisor: the whole state, not a change, so the supervisor's
/// `usbdev::ASK` is answered by sending it again. `try_send`, reacquired by name once - the supervisor is
/// restartable (6.2) - and never waited on (8.9).
pub fn report(ctx: &ServiceContext, radio: Option<&Radio>) {
    let r = match radio {
        Some(r) => usbdev::Report { present: true, gen: r.gen, vid: r.vid, pid: r.pid },
        None => usbdev::Report { present: false, gen: 0, vid: 0, pid: 0 },
    };
    let msg = Message::from_bytes(&usbdev::encode(&r));
    let sent = gs::ipc::try_send(ctx, SUPERVISOR, &msg).is_ok()
        || (gs::cap::reacquire(ctx, SUPERVISOR) && gs::ipc::try_send(ctx, SUPERVISOR, &msg).is_ok());
    if !sent {
        ctx.log("dwc2-svc: could not report the WiFi dongle to the supervisor - its driver will not be started or stopped until the next report");
    }
}

/// Where the reports go: the supervisor decides which driver a device gets.
const SUPERVISOR: &str = "supervisor";

fn tell(ctx: &ServiceContext) -> bool {
    let msg = Message::from_bytes(&[usbfn::NOTE_RADIO]);
    gs::ipc::try_send(ctx, DRIVER, &msg).is_ok()
        || (gs::cap::reacquire(ctx, DRIVER) && gs::ipc::try_send(ctx, DRIVER, &msg).is_ok())
}

/// The radio's driver, the one service this host tells about the radio.
const DRIVER: &str = "wifi-usb";

/// How long the driver must have asked nothing before a notice it named in an `OP_SYNC` is sent again:
/// long enough that it is back in its serve loop rather than mid-sequence, short enough that a held
/// receive is not kept waiting. A bring-up's requests come well under a millisecond apart.
const DRIVER_QUIET_MS: u64 = 5;

/// Control transfers tried before the host says FAILED. The reason `read32` gives: this controller sequences
/// transfers in software, so one XACTERR is a transient on a contended bus, not a verdict. `OP_CONTROL_ONCE`
/// gets one: a transfer the client must not have reach the device twice, whose failure the client handles.
const CONTROL_TRIES: u32 = 4;

/// Serve one `usbfn` request for the bound radio - `None` when none is bound - and answer on `reply`, which
/// this reclaims. Every op is answered, a request for a radio that is not here included, so a client is
/// never left to time out against a clean log.
pub fn serve(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, radio: Option<&mut Radio>, msg: &Message, reply: Cap,
) {
    let p = msg.payload_bytes();
    let op = p.first().copied().unwrap_or(0);
    let mut radio = radio;
    if let Some(r) = radio.as_mut() {
        r.last_req = Some(wait::Since::now(ctx));
    }
    // `OP_SYNC`: never answered (`usbfn::OP_SYNC`) - the reply capability given back, the named notice
    // owed. With no radio bound, a binding notice is told at once: the driver's requests are all answered
    // `ST_NO_DEVICE` from here, so it is not mid-sequence for long.
    if op == usbfn::OP_SYNC {
        gs::cap::remove(ctx, reply);
        match (p.get(1).copied(), radio) {
            (Some(usbfn::NOTE_BULK_IN), Some(r)) => {
                r.stats.syncs = r.stats.syncs.wrapping_add(1);
                r.sync_bulk = true;
            }
            (Some(usbfn::NOTE_RADIO), Some(r)) => {
                r.stats.syncs = r.stats.syncs.wrapping_add(1);
                r.sync_radio = true;
            }
            (Some(usbfn::NOTE_RADIO), None) => {
                let _ = tell(ctx);
            }
            _ => {}
        }
        return;
    }
    let radio = match (op, radio) {
        (usbfn::OP_BULK_IN, Some(r)) => return bulk_in_request(ctx, mmio, dma, r, reply),
        (usbfn::OP_BULK_OUT, Some(r)) => {
            let st = bulk_out(ctx, mmio, dma, r, p);
            let _ = gs::ipc::reply(ctx, reply, &Message::from_bytes(&[op, st]));
            return;
        }
        (_, r) => r,
    };
    let mut out = [0u8; 2 + usbfn::CONTROL_MAX];
    out[0] = op;
    let n = match (op, radio) {
        (_, None) => {
            out[1] = usbfn::ST_NO_DEVICE;
            2
        }
        (usbfn::OP_INFO, Some(r)) => {
            out[1] = usbfn::ST_OK;
            out[2..4].copy_from_slice(&r.vid.to_le_bytes());
            out[4..6].copy_from_slice(&r.pid.to_le_bytes());
            6
        }
        (usbfn::OP_CONTROL, Some(r)) => control(ctx, mmio, dma, &r.t, p, CONTROL_TRIES, &mut out),
        (usbfn::OP_CONTROL_ONCE, Some(r)) => control(ctx, mmio, dma, &r.t, p, 1, &mut out),
        _ => {
            out[1] = usbfn::ST_BAD_REQUEST;
            2
        }
    };
    let _ = gs::ipc::reply(ctx, reply, &Message::from_bytes(&out[..n]));
}

/// One control transfer: `p` is `[op, setup(8), data out...]`. Fills `out[1..]` and returns its length.
fn control(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, p: &[u8], tries: u32, out: &mut [u8]) -> usize {
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
    for i in 0..tries {
        if i > 0 {
            delay::hold(ctx, Budget::ms(5));
        }
        if chan::control(ctx, mmio, dma, t, &setup, &mut buf, data_in, len) {
            out[1] = usbfn::ST_OK;
            if data_in {
                out[2..2 + len].copy_from_slice(&buf[..len]);
                return 2 + len;
            }
            return 2;
        }
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
