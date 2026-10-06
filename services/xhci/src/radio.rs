// SPDX-License-Identifier: GPL-2.0-only
//! The USB WiFi dongle, behind `xhci` (U2, `docs/wifi-usb.md` 7 for the design, 25 for U2a). This host binds it by VID:PID and serves
//! `godspeed_wifi::usbfn` for it to `wifi-usb` - the protocol `dwc2` serves on the Pi 2, answered the same
//! way, so the dongle's driver cannot tell the hosts apart.
//!
//! **U2a (2026-10-06): who it is, and its control transfers, both directions.** That is everything
//! `wifi-usb`'s bring-up asks of a host up to and including the firmware, the MAC, baseband and RF tables
//! and the channel (R1 to R3a). The bulk IN that carries received frames is U2b, the bulk OUT that carries
//! sent ones U2c; until then those two answer `ST_FAILED`, which `wifi-usb` reports.
//!
//! **Where it lives in the arena.** In the device slice it was enumerated into, kept as a keyboard's is:
//! the EP0 ring is the slice's EP0 page, and a control transfer's data stage uses the slice's report page,
//! which a dongle has no interrupt endpoint to use. No new arena, so no kernel change.
//!
//! **Matched by slot.** A control completion is taken as this slot's transfer event. That is exact while
//! EP0 is the only endpoint this host drives on the dongle; U2b's armed bulk IN on the same slot is where
//! matching must take the endpoint too (the transfer event's bits 20:16), and that card says so.
//!
//! A completion belonging to someone else - a keystroke - is filed in the caller's `EvMail`, as the disk
//! path files it, so the caller re-arms that endpoint.

use godspeed as gs;
use godspeed_sdk::{Dma, Message, Mmio, ServiceContext};
use godspeed_sdk::service_context::usbdev;
use godspeed_wifi::usbfn;
use gs::driver::wait::{self, Budget};

use crate::msc::POLL_GRANULARITY;
use crate::{
    device_ctx_off, ep0_tr_off, next_event, report_off, reset_endpoint, EvMail, TRB_DATA_STAGE, TRB_LINK,
    TRB_SETUP_STAGE, TRB_SIZE, TRB_STATUS_STAGE, TRB_TRANSFER_EVENT,
};

/// The dongle this host binds: a Realtek RTL8188CUS, the one `wifi-usb` drives - the same pair `dwc2` binds.
pub const VID: u16 = 0x0bda;
pub const PID: u16 = 0x8176;

/// Whether a device descriptor's ID word (VID in its low half, PID in its high) is the dongle's.
pub fn is_radio(ids: u32) -> bool {
    ids & 0xFFFF == VID as u32 && ids >> 16 == PID as u32
}

/// Where the runtime control transfers begin on the dongle's EP0 ring: past the enumeration's own - the
/// device descriptor at 0, the configuration descriptor at 48, Set Configuration at 96, ending at 128.
pub const EP0_RUNTIME_START: usize = 128;

/// The bound dongle: its controller slot, its DMA slice, where it is, and its EP0 ring's producer cursor.
pub struct Radio {
    pub slot: u32,
    pub dev_idx: usize,
    /// The root port it is on (or behind), for the log and the binding's identity.
    pub port: u32,
    pub ids: u32,
    /// This bind's number on this host (`usbdev::Report::gen`). Every enumeration pass rebinds the dongle,
    /// so it counts passes that found it; set by the caller once the pass is over.
    pub gen: u32,
    /// The hub it is behind and its port there, or `0, 0` on a root port: where an unplug is read when the
    /// root port stays connected (the Pi 4's VL805 hub).
    pub hub_slot: u32,
    pub hub_port: u32,
    cur: usize,
    pcs: u32,
    /// Endpoint repairs this pass, bounded: the command ring is one page per pass, and a dongle that
    /// keeps failing is re-enumerated rather than repaired forever (26.6).
    repairs: u32,
}

impl Radio {
    pub fn new(slot: u32, dev_idx: usize, port: u32, ids: u32) -> Self {
        Radio { slot, dev_idx, port, ids, gen: 0, hub_slot: 0, hub_port: 0, cur: EP0_RUNTIME_START, pcs: 1, repairs: 0 }
    }

    pub fn vid(&self) -> u16 {
        self.ids as u16
    }

    pub fn pid(&self) -> u16 {
        (self.ids >> 16) as u16
    }
}

/// One EP0 ring's bytes - a page, the slice's.
const RING_BYTES: usize = 0x1000;
/// How long one control transfer may take. A register access is a few hundred microseconds; the bound is
/// for a dongle that stopped answering, and it sits well inside `wifi-usb`'s two-second wait on the host.
const CONTROL_MS: u64 = 500;
/// Completions for other slots tolerated while waiting for ours - an event storm must not livelock the wait.
const MAX_UNRELATED: u32 = 4096;
/// Tries for `OP_CONTROL`, as `dwc2`'s `CONTROL_TRIES`: a transient on the bus is retried; `OP_CONTROL_ONCE`
/// gets one, for a transfer that must not reach the device twice (a firmware block).
const CONTROL_TRIES: u32 = 4;
/// Endpoint repairs allowed in one pass before the dongle is given up on until the next enumeration.
const MAX_REPAIRS: u32 = 8;

/// The controller state a radio request needs, gathered so the calls stay readable.
pub struct Hc<'a> {
    pub dma: &'a Dma,
    pub mmio: &'a Mmio,
    pub dboff: usize,
    pub ir0: usize,
    pub ctx_size: usize,
}

/// One control transfer on the dongle's EP0 (Setup, an optional data stage either way, Status), at the
/// ring's cursor. `data` is the OUT payload or, for an IN, where the answer is copied. The completion code
/// (1 success, 13 short packet), or `None` when none came within `CONTROL_MS`.
#[allow(clippy::too_many_arguments)]
fn control_once(
    ctx: &ServiceContext, hc: &Hc, r: &mut Radio, setup: &[u8; 8], data: &mut [u8], len: usize, data_in: bool,
    ev_idx: &mut usize, ev_cycle: &mut u32, eaten: &mut EvMail,
) -> Option<u32> {
    let buf = report_off(r.dev_idx);
    if !data_in {
        for (i, b) in data[..len].iter().enumerate() {
            hc.dma.write8(buf + i, *b);
        }
    }
    // Three TRBs and the Link that may follow them must fit before the page ends; otherwise the Link goes
    // at the cursor, the controller follows it to the page's start and flips its cycle with ours.
    let ring = ep0_tr_off(r.dev_idx);
    if r.cur + 4 * TRB_SIZE > RING_BYTES {
        let bp = hc.dma.phys_at(ring);
        hc.dma.write32(ring + r.cur, bp as u32);
        hc.dma.write32(ring + r.cur + 4, (bp >> 32) as u32);
        hc.dma.write32(ring + r.cur + 8, 0);
        hc.dma.write32(ring + r.cur + 12, (TRB_LINK << 10) | (1 << 1) | r.pcs);
        r.cur = 0;
        r.pcs ^= 1;
    }
    let t = ring + r.cur;
    // Setup: the 8-byte packet inline (IDT, bit 6), TRT 0 no data, 2 OUT, 3 IN.
    let trt = if len == 0 { 0 } else if data_in { 3 } else { 2 };
    hc.dma.write32(t, u32::from_le_bytes([setup[0], setup[1], setup[2], setup[3]]));
    hc.dma.write32(t + 4, u32::from_le_bytes([setup[4], setup[5], setup[6], setup[7]]));
    hc.dma.write32(t + 8, 8);
    hc.dma.write32(t + 12, r.pcs | (1 << 6) | (TRB_SETUP_STAGE << 10) | (trt << 16));
    let mut off = t + TRB_SIZE;
    if len > 0 {
        let dp = hc.dma.phys_at(buf);
        hc.dma.write32(off, dp as u32);
        hc.dma.write32(off + 4, (dp >> 32) as u32);
        hc.dma.write32(off + 8, len as u32);
        hc.dma.write32(off + 12, r.pcs | (TRB_DATA_STAGE << 10) | ((data_in as u32) << 16));
        off += TRB_SIZE;
    }
    // Status: the direction opposite the data stage, IN when there is none; Interrupt On Completion.
    let status_in = len == 0 || !data_in;
    hc.dma.write32(off, 0);
    hc.dma.write32(off + 4, 0);
    hc.dma.write32(off + 8, 0);
    hc.dma.write32(off + 12, r.pcs | (1 << 5) | (TRB_STATUS_STAGE << 10) | ((status_in as u32) << 16));
    r.cur = off + TRB_SIZE - ring;
    hc.mmio.write32(hc.dboff + r.slot as usize * 4, 1);

    let mut deadline = wait::Deadline::start(ctx, Budget::ms(CONTROL_MS));
    let mut unrelated = 0u32;
    if let Some(cc) = eaten.take(r.slot) {
        return finish(hc, r, data, len, data_in, cc);
    }
    loop {
        match next_event(hc.dma, hc.mmio, hc.ir0, ev_idx, ev_cycle, POLL_GRANULARITY) {
            Some((TRB_TRANSFER_EVENT, cc, sid)) if sid == r.slot => return finish(hc, r, data, len, data_in, cc),
            Some((TRB_TRANSFER_EVENT, cc, sid)) => {
                eaten.put(sid, cc);
                unrelated += 1;
                if unrelated >= MAX_UNRELATED {
                    return None;
                }
            }
            Some(_) | None => {}
        }
        if deadline.expired() {
            return None;
        }
    }
}

/// An IN transfer's answer copied out of the data page, and the completion code returned.
fn finish(hc: &Hc, r: &Radio, data: &mut [u8], len: usize, data_in: bool, cc: u32) -> Option<u32> {
    if data_in && (cc == 1 || cc == 13) {
        let buf = report_off(r.dev_idx);
        for (i, b) in data[..len].iter_mut().enumerate() {
            *b = hc.dma.read8(buf + i);
        }
    }
    Some(cc)
}

/// After a failed transfer EP0 may be halted (a STALL, a transaction error): clear the ring so no stale
/// TRB can run, repair the endpoint and point it back at the ring's start, and move the cursor there. As
/// the hub path repairs its own EP0. `false` when the repair failed or the bound is spent - the caller
/// then asks for a re-enumeration.
#[allow(clippy::too_many_arguments)]
fn repair(
    ctx: &ServiceContext, hc: &Hc, r: &mut Radio, ev_idx: &mut usize, ev_cycle: &mut u32, cmd_idx: &mut usize,
) -> bool {
    if r.repairs >= MAX_REPAIRS {
        return false;
    }
    r.repairs += 1;
    let ring = ep0_tr_off(r.dev_idx);
    for off in (0..RING_BYTES).step_by(4) {
        hc.dma.write32(ring + off, 0);
    }
    if !reset_endpoint(
        ctx, hc.dma, hc.mmio, hc.dboff, hc.ir0, r.slot, 1, ring, device_ctx_off(r.dev_idx), hc.ctx_size,
        ev_idx, ev_cycle, cmd_idx,
    ) {
        return false;
    }
    r.cur = 0;
    r.pcs = 1;
    true
}

/// `OP_CONTROL` / `OP_CONTROL_ONCE`, as `dwc2` answers them: `p` is `[op, setup(8), data out...]`, the
/// reply `[op, status, data in...]` into `out`. `Err(())` asks the caller to re-enumerate: the endpoint
/// could not be repaired.
#[allow(clippy::too_many_arguments)]
fn control(
    ctx: &ServiceContext, hc: &Hc, r: &mut Radio, p: &[u8], tries: u32, out: &mut [u8],
    ev_idx: &mut usize, ev_cycle: &mut u32, cmd_idx: &mut usize, eaten: &mut EvMail,
) -> Result<usize, ()> {
    if p.len() < 9 {
        out[1] = usbfn::ST_BAD_REQUEST;
        return Ok(2);
    }
    let mut setup = [0u8; 8];
    setup.copy_from_slice(&p[1..9]);
    let len = u16::from_le_bytes([setup[6], setup[7]]) as usize;
    let data_in = setup[0] & 0x80 != 0;
    if len > usbfn::CONTROL_MAX || (!data_in && p.len() < 9 + len) {
        out[1] = usbfn::ST_BAD_REQUEST;
        return Ok(2);
    }
    let mut buf = [0u8; usbfn::CONTROL_MAX];
    if !data_in {
        buf[..len].copy_from_slice(&p[9..9 + len]);
    }
    for _ in 0..tries {
        let at = r.cur;
        let got = control_once(ctx, hc, r, &setup, &mut buf, len, data_in, ev_idx, ev_cycle, eaten);
        // MEASURED, not guessed: on the Pi 4 (2026-10-06) the first transfer after U1's reads left EP0 in
        // the Error state, which the T630 never showed. The completion code, the request and where on the
        // ring its TD sat say whether the controller rejected a TRB or the device refused the request.
        // The first few per pass only (`repairs` is per pass).
        if !matches!(got, Some(1) | Some(13)) && r.repairs < 3 {
            ctx.log_fmt(format_args!(
                "xhci: the WiFi dongle's control transfer failed - cc={} (0 = no event within {} ms), setup={:02x?}, {} {} byte(s), TD at ring offset {:#x} pcs={}",
                got.unwrap_or(0), CONTROL_MS, setup, if data_in { "IN" } else { "OUT" }, len, at, r.pcs));
        }
        match got {
            Some(1) | Some(13) => {
                out[1] = usbfn::ST_OK;
                if data_in {
                    out[2..2 + len].copy_from_slice(&buf[..len]);
                    return Ok(2 + len);
                }
                return Ok(2);
            }
            // Failed or unanswered: EP0 may be halted, and the next transfer would never run.
            _ => {
                if !repair(ctx, hc, r, ev_idx, ev_cycle, cmd_idx) {
                    ctx.log_fmt(format_args!(
                        "xhci: the WiFi dongle on port {} (slot {}) failed a control transfer and its EP0 could not be repaired ({} repairs this pass)",
                        r.port, r.slot, r.repairs));
                    out[1] = usbfn::ST_FAILED;
                    return Err(());
                }
            }
        }
    }
    out[1] = usbfn::ST_FAILED;
    Ok(2)
}

/// What serving one message came to.
pub enum Served {
    /// Not a radio request; the caller's other servers look at it.
    NotOurs,
    /// Answered (or, for `OP_SYNC`, deliberately not).
    Done,
    /// Answered `ST_FAILED`, and the dongle's EP0 could not be repaired: re-enumerate.
    Reenumerate,
}

/// Serve `msg` if it is a radio request (`usbfn::OP_INFO` to `OP_SYNC`), for the bound `radio` or none.
#[allow(clippy::too_many_arguments)]
pub fn serve(
    ctx: &ServiceContext, hc: &Hc, radio: Option<&mut Radio>, msg: &Message,
    ev_idx: &mut usize, ev_cycle: &mut u32, cmd_idx: &mut usize, eaten: &mut EvMail,
) -> Served {
    let p = msg.payload_bytes();
    // The supervisor asking for this host's device report again (`usbdev::ASK`): no reply capability, by
    // design - the answer is the report itself.
    if p == [usbdev::ASK] {
        report_device(ctx, radio.as_deref());
        return Served::Done;
    }
    let op = p.first().copied().unwrap_or(0);
    if !(usbfn::OP_INFO..=usbfn::OP_SYNC).contains(&op) {
        return Served::NotOurs;
    }
    let Some(reply) = gs::ipc::take_sent_cap(ctx) else { return Served::Done };
    // `OP_SYNC` is never answered (`usbfn::OP_SYNC`); this host sends no notice that could stand in for
    // an answer except `NOTE_RADIO`, so a named binding notice is simply told again.
    if op == usbfn::OP_SYNC {
        gs::cap::remove(ctx, reply);
        if p.get(1).copied() == Some(usbfn::NOTE_RADIO) {
            notify_driver(ctx);
        }
        return Served::Done;
    }
    let mut out = [0u8; 2 + usbfn::CONTROL_MAX];
    out[0] = op;
    let mut verdict = Served::Done;
    let n = match (op, radio) {
        (_, None) => {
            out[1] = usbfn::ST_NO_DEVICE;
            2
        }
        (usbfn::OP_INFO, Some(r)) => {
            out[1] = usbfn::ST_OK;
            out[2..4].copy_from_slice(&r.vid().to_le_bytes());
            out[4..6].copy_from_slice(&r.pid().to_le_bytes());
            6
        }
        (usbfn::OP_CONTROL, Some(r)) | (usbfn::OP_CONTROL_ONCE, Some(r)) => {
            let tries = if op == usbfn::OP_CONTROL { CONTROL_TRIES } else { 1 };
            match control(ctx, hc, r, p, tries, &mut out, ev_idx, ev_cycle, cmd_idx, eaten) {
                Ok(n) => n,
                Err(()) => {
                    verdict = Served::Reenumerate;
                    2
                }
            }
        }
        // U2b and U2c: the bulk endpoints are not configured yet on this host.
        (_, Some(_)) => {
            out[1] = usbfn::ST_FAILED;
            2
        }
    };
    let _ = gs::ipc::reply(ctx, reply, &Message::from_bytes(&out[..n]));
    verdict
}

/// A radio request that arrived where no radio can be bound - the driver's idle paths, with nothing
/// enumerated: answered `ST_NO_DEVICE` rather than dropped, so `wifi-usb` hears "no dongle" at once instead
/// of waiting out its deadline on a host that will never answer. `true` when `msg` was one.
pub fn answer_absent(ctx: &ServiceContext, msg: &Message) -> bool {
    let p = msg.payload_bytes();
    // The drains this serves run only where nothing is bound, so "not attached" is the truth there.
    if p == [usbdev::ASK] {
        report_device(ctx, None);
        return true;
    }
    let op = p.first().copied().unwrap_or(0);
    if !(usbfn::OP_INFO..=usbfn::OP_SYNC).contains(&op) {
        return false;
    }
    if let Some(reply) = gs::ipc::take_sent_cap(ctx) {
        if op == usbfn::OP_SYNC {
            gs::cap::remove(ctx, reply);
        } else {
            let _ = gs::ipc::reply(ctx, reply, &Message::from_bytes(&[op, usbfn::ST_NO_DEVICE]));
        }
    }
    true
}

/// The radio's driver, the one service this host tells about the radio.
const DRIVER: &str = "wifi-usb";

/// Tell `wifi-usb` the radio's binding changed (`usbfn::NOTE_RADIO`): `try_send`, never blocking on a driver
/// that is behind, reacquired by name once if the cap is stale - either may be spawned or respawned after the other.
/// Quiet when it cannot be delivered: on a board where `wifi-usb` is not built there is nobody to tell, and
/// where it is, its own `OP_INFO` at start covers a notice it missed.
pub fn notify_driver(ctx: &ServiceContext) {
    let msg = Message::from_bytes(&[usbfn::NOTE_RADIO]);
    let _ = gs::ipc::try_send(ctx, DRIVER, &msg).is_ok()
        || (gs::cap::reacquire(ctx, DRIVER) && gs::ipc::try_send(ctx, DRIVER, &msg).is_ok());
}

/// The radio's binding changed, or this host's first pass ended: told to the driver (`notify_driver`) and
/// reported to the supervisor (`report_device`), which starts the driver when the dongle is attached and
/// stops it when it is not (`docs/usb-device-drivers.md`) - what `dwc2` does on the Pi 2.
pub fn announce(ctx: &ServiceContext, radio: Option<&Radio>) {
    notify_driver(ctx);
    report_device(ctx, radio);
}

/// This host's report on the dongle, to the supervisor (`usbdev`): its whole state, not a change, so an
/// `ASK` is answered by sending it again. `try_send`, reacquired by name once - the supervisor is
/// restartable (6.2) - and never waited on (8.9). Loud when a dongle's report cannot be delivered: it then
/// has no driver and nothing else will say so. Quiet when "nothing attached" cannot be: that is every boot
/// on a board that does not embed the dongle's driver (the Pi 4, the VisionFive), where this host is given
/// no supervisor peer and there is nothing to start.
pub fn report_device(ctx: &ServiceContext, radio: Option<&Radio>) {
    let r = match radio {
        Some(r) => usbdev::Report { present: true, gen: r.gen, vid: r.vid(), pid: r.pid() },
        None => usbdev::Report { present: false, gen: 0, vid: 0, pid: 0 },
    };
    let msg = Message::from_bytes(&usbdev::encode(&r));
    let sent = gs::ipc::try_send(ctx, SUPERVISOR, &msg).is_ok()
        || (gs::cap::reacquire(ctx, SUPERVISOR) && gs::ipc::try_send(ctx, SUPERVISOR, &msg).is_ok());
    if !sent && r.present {
        ctx.log("xhci: could not report the WiFi dongle to the supervisor - its driver will not be started until the next report");
    }
}

/// Where the reports go: the supervisor decides which driver a device gets.
const SUPERVISOR: &str = "supervisor";
