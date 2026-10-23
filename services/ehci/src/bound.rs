// SPDX-License-Identifier: GPL-2.0-only
//! The ONE device this host binds for a driver of its own (2026-10-23): what `xhci` and `dwc2` do for a USB device
//! an installed driver package names (`hw_usb`, `docs/developer-services-design.md` 13.1), done on the USB 2.0
//! controller - the T630's back sockets, behind its rate-matching hub, or a device straight on the root port that had
//! one at boot (the first such port, which is then watched for a replug; no other root port is).
//!
//! A device whose first interface is vendor-specific (class 0xFF) and that is not a boot keyboard or mouse - which
//! this host drives itself - is addressed, configured, its bulk endpoints found, and reported to the supervisor
//! (`usbdev`, as host `HOST_EHCI`), which starts the package that names it with this host as its peer. The package
//! asks for the device's transfers in the function protocol (`godspeed_sdk::usbfn`, through `gs::driver::bus`), and
//! they are answered here: who it is and where, control transfers on its default endpoint, and its bulk endpoints.
//!
//! **What is different from `xhci`, recorded rather than hidden (26.7).** This host sends no notices (`NOTE_BULK_IN`,
//! `NOTE_RADIO`): a bulk IN is asked for, and the host waits up to `BULK_IN_WAIT` for the device to answer and says
//! what came, nothing included - a driver asks again. So the WiFi dongle's driver, which waits to be told, is not
//! served here, and the supervisor does not start `wifi-usb` for a dongle on these sockets (its peer is `xhci`).
//! `OP_REPLUG` is refused, as `xhci` refuses it. One bound device at a time, as on the other hosts.
//!
//! Every transfer is the controller's own async schedule, one queue head at a time, exactly as `control` in
//! `main.rs` has always run: the bulk transfer's queue head and descriptor live in their own part of the arena, so a
//! transfer never disturbs the keyboard's. While keyboards are polled, the schedule is pointed back at their ring
//! after each transfer (`point_async`).

use core::cell::Cell;

use godspeed as gs;
use godspeed::driver::wait::{self, Budget};
use godspeed::driver::{delay, Registers};
use godspeed_sdk::service_context::usbdev;
use godspeed_sdk::usbfn;
use godspeed_sdk::{Message, ServiceContext};

use crate::{await_hw, control_data, wait as wait_reg, Ep, Parts, CMD_ASE, CMD_IAAD, DATA_BUF, OP_ASYNCLISTADDR,
            OP_CTRLDSSEGMENT, OP_USBCMD, OP_USBSTS, QTD_ACTIVE, QTD_ERRMASK, QTD_HALTED, RECOVERY, STS_ASS, STS_IAA};

/// The address a bound device is given. Clear of the hub (1) and the keyboard and mouse (2 and 3, `scan_devices`),
/// so binding never moves a device the HID path addressed.
pub const BOUND_ADDR: u8 = 100;

/// How long a bulk IN waits for the device before it says nothing came. A device with nothing to send NAKs; this
/// host has no notice to send when it has something, so the driver's next ask is how it learns.
const BULK_IN_WAIT: Budget = Budget::ms(20);
/// How long a bulk OUT may take: a device NAKing an OUT is busy, and the driver's request to this host is bounded at
/// 2 s (`gs::driver::bus`), which this stays well inside.
const BULK_OUT_WAIT: Budget = Budget::ms(1_000);
/// The most of a configuration descriptor read: the control part's data buffer (`main.rs`, `DATA_BUF` to its end).
const CONFIG_MAX: usize = 0x200;

// The bulk part of the arena (`Parts::bulk`): one queue head and one transfer descriptor, then a page for an IN and
// a page for an OUT, each page-aligned so a transfer up to 4 KiB needs only the descriptor's first buffer pointer.
const BULK_QH: usize = 0x000;
const BULK_QTD: usize = 0x040;
const BULK_IN_BUF: usize = 0x1000;
const BULK_OUT_BUF: usize = 0x2000;
/// The bulk part's size: the structures' page and the two buffers.
pub const BULK_BYTES: usize = 0x3000;
const _: () = assert!(usbfn::BULK_IN_MAX <= 0x1000 && usbfn::BULK_OUT_MAX <= 0x1000);

/// The hardware the requests are served on.
pub struct Hw<'a> {
    pub mmio: &'a Registers,
    pub dma: &'a Parts,
    pub op: usize,
    pub said: &'a Cell<bool>,
}

/// The bound device: who it is, how it is reached, and its bulk endpoints' data toggles.
#[derive(Clone, Copy)]
pub struct Bound {
    pub vid: u16,
    pub pid: u16,
    pub gen: u32,
    /// Its default endpoint at `BOUND_ADDR`, with its speed and, below a hub, the hub's translator.
    ep0: Ep,
    /// The root port it is on, from 1, and the hub port it is behind (0 when it is on the root port itself).
    pub root: u8,
    pub hub_port: u8,
    /// Its first interface's bulk IN (endpoint address, 0 for none) and bulk OUTs, in descriptor order.
    in_addr: u8,
    in_mps: u16,
    outs: [u8; 4],
    out_mps: [u16; 4],
    n_out: u8,
    in_toggle: u32,
    out_toggle: [u32; 4],
}

/// This host's one binding, and what it last told the supervisor.
pub struct Host {
    pub dev: Option<Bound>,
    /// The gen last reported, `Some(None)` for "nothing bound", `None` before the first report.
    last: Option<Option<u32>>,
    /// Bindings made, and a seed from the clock so a restarted host's gens differ from the dead one's.
    binds: u32,
    seed: u32,
    report_said: bool,
}

impl Host {
    pub fn new(ctx: &ServiceContext) -> Host {
        Host { dev: None, last: None, binds: 0, seed: wait::ticks(ctx) as u32, report_said: false }
    }

    /// The next binding's number (`usbdev::Report::gen`).
    pub fn next_gen(&mut self) -> u32 {
        self.binds = self.binds.wrapping_add(1);
        self.seed.rotate_left(8) ^ self.binds
    }

    /// The bound device left: forgotten, and said.
    pub fn unbind(&mut self, ctx: &ServiceContext) {
        if let Some(b) = self.dev.take() {
            ctx.log_fmt(format_args!("ehci: the USB device {:04x}:{:04x} bound for a driver of its own is gone", b.vid, b.pid));
        }
    }

    /// Tell the supervisor what is bound, when that differs from what it was last told.
    pub fn report_if_changed(&mut self, ctx: &ServiceContext) {
        if self.last != Some(self.dev.map(|b| b.gen)) {
            self.report(ctx);
        }
    }

    /// This host's report, the whole state rather than a change, so `usbdev::ASK` is answered by sending it again.
    /// `try_send`, reacquired by name once - the supervisor is restartable (6.2) - and never waited on (8.9).
    pub fn report(&mut self, ctx: &ServiceContext) {
        let r = match self.dev {
            Some(b) => usbdev::Report { present: true, gen: b.gen, vid: b.vid, pid: b.pid, host: usbdev::HOST_EHCI },
            None => usbdev::Report { present: false, gen: 0, vid: 0, pid: 0, host: usbdev::HOST_EHCI },
        };
        let msg = Message::from_bytes(&usbdev::encode(&r));
        let sent = gs::ipc::try_send(ctx, SUPERVISOR, &msg).is_ok()
            || (gs::cap::reacquire(ctx, SUPERVISOR) && gs::ipc::try_send(ctx, SUPERVISOR, &msg).is_ok());
        if sent {
            self.last = Some(self.dev.map(|b| b.gen));
        } else if !self.report_said {
            // Once: a board whose supervisor starts no driver for a USB device gives this host no peer to report to.
            self.report_said = true;
            ctx.log("ehci: could not report the device bound for a driver of its own to the supervisor - no driver will be started for it until a report gets through");
        }
    }

    /// Serve one message: the supervisor's `usbdev::ASK`, or a driver's `usbfn` request, answered on its reply
    /// capability, which this reclaims. Anything else is not this host's to answer and is dropped, as it always was.
    /// `hw` is `None` where this host has no arena (every request is then `ST_NO_DEVICE`); `ring` is the keyboard
    /// ring's address while one is polled, to point the schedule back at after a transfer.
    pub fn serve(&mut self, ctx: &ServiceContext, hw: Option<&Hw>, ring: Option<u32>, msg: &Message) {
        let p = msg.payload_bytes();
        if p == [usbdev::ASK] {
            self.report(ctx);
            return;
        }
        let op = p.first().copied().unwrap_or(0);
        // The function protocol's range, `OP_INFO` to `OP_REPLUG` less the supervisor's ask: what
        // `godspeed_wifi::usbfn::is_request` says, written out because this host does not link `sdk/wifi`.
        if !(usbfn::OP_INFO..=usbfn::OP_REPLUG).contains(&op) || op == usbdev::ASK {
            return;
        }
        let Some(reply) = gs::ipc::take_sent_cap(ctx) else { return };
        if op == usbfn::OP_SYNC {
            // Never answered (`usbfn::OP_SYNC`); this host sends no notice that one could name.
            gs::cap::remove(ctx, reply);
            return;
        }
        let mut out = [0u8; 2 + usbfn::BULK_IN_MAX];
        out[0] = op;
        let mut moved = false;
        let n = match (op, self.dev.as_mut(), hw) {
            (_, None, _) | (_, _, None) => {
                out[1] = usbfn::ST_NO_DEVICE;
                2
            }
            (usbfn::OP_INFO, Some(b), _) => info(b, &mut out),
            (usbfn::OP_CONTROL | usbfn::OP_CONTROL_ONCE, Some(b), Some(hw)) => {
                moved = true;
                control_req(ctx, hw, b, p, if op == usbfn::OP_CONTROL { CONTROL_TRIES } else { 1 }, &mut out)
            }
            (usbfn::OP_BULK_OUT, Some(b), Some(hw)) => {
                moved = true;
                out[1] = bulk_out_req(ctx, hw, b, p);
                2
            }
            (usbfn::OP_BULK_IN, Some(b), Some(hw)) => {
                moved = true;
                bulk_in_req(ctx, hw, b, &mut out)
            }
            // `OP_REPLUG`, and anything else: this host resets no port for a driver (as `xhci`).
            _ => {
                out[1] = usbfn::ST_BAD_REQUEST;
                2
            }
        };
        if let (true, Some(hw), Some(ring)) = (moved, hw, ring) {
            point_async(ctx, hw.mmio, hw.op, ring);
        }
        let _ = gs::ipc::reply(ctx, reply, &Message::from_bytes(&out[..n]));
    }
}

/// Where the reports go: the supervisor decides which driver a device gets.
const SUPERVISOR: &str = "supervisor";

/// Control transfers tried before the host says FAILED, for `OP_CONTROL`. A split transaction through this hub's
/// translator is intermittently flaky (`control_retry` in `main.rs`); `OP_CONTROL_ONCE` gets one.
const CONTROL_TRIES: u32 = 3;

/// Would this host bind a device whose first interface is class `class` for a driver of its own? Vendor-specific
/// only: a device of a standard class has, or will have, a driver of its own kind. The `bind-any-class` feature binds
/// any class but a hub or a HID, for `osdev test driver-packages` - QEMU's EHCI has no vendor-specific high-speed device, and
/// its mass-storage device moves real bulk transfers - and is never built into an image.
pub fn bindable(class: u8) -> bool {
    class == 0xFF || (cfg!(feature = "bind-any-class") && class != 0x09 && class != 0x03)
}

/// Read the device at address 0 on `ep0` (its speed and packet size known), and if its first interface is one this
/// host binds, address it at `BOUND_ADDR`, configure it, and find its bulk endpoints. `None`, said, when it is not
/// bindable or a step fails - a device not bound is left where it was.
pub fn try_bind(ctx: &ServiceContext, hw: &Hw, ep0: Ep, root: u8, hub_port: u8, gen: u32) -> Option<Bound> {
    let (mmio, dma, op, said) = (hw.mmio, hw.dma, hw.op, hw.said);
    let ctl = |ep: &Ep, setup: [u8; 8], len: usize, in_dir: bool, tries: u32| -> Option<usize> {
        for _ in 0..tries {
            if let Some(n) = control_data(ctx, mmio, dma, op, said, ep, &setup, len, in_dir, &[]) {
                return Some(n);
            }
            delay::hold_parked(ctx, RECOVERY);
        }
        None
    };
    let at = Where(root, hub_port);
    ctl(&ep0, [0x80, 0x06, 0x00, 0x01, 0, 0, 18, 0], 18, true, 5)?;
    let vid = dma.ctl.read16(DATA_BUF + 8);
    let pid = dma.ctl.read16(DATA_BUF + 10);
    // The configuration descriptor: nine bytes for its length, then all of it that fits.
    ctl(&ep0, [0x80, 0x06, 0x00, 0x02, 0, 0, 9, 0], 9, true, 5)?;
    let total = (dma.ctl.read16(DATA_BUF + 2) as usize).clamp(9, CONFIG_MAX);
    let [tlo, thi] = (total as u16).to_le_bytes();
    let got = ctl(&ep0, [0x80, 0x06, 0x00, 0x02, 0, 0, tlo, thi], total, true, 5)?;
    let cfg = dma.ctl.read8(DATA_BUF + 5);
    let mut b = Bound {
        vid, pid, gen, ep0: Ep { addr: BOUND_ADDR, ..ep0 }, root, hub_port,
        in_addr: 0, in_mps: 0, outs: [0; 4], out_mps: [0; 4], n_out: 0, in_toggle: 0, out_toggle: [0; 4],
    };
    // The first interface (alternate setting 0), and the bulk endpoints that follow it until the next interface.
    let (mut class, mut seen_iface, mut o) = (None, false, 0usize);
    while o + 2 <= got {
        let len = dma.ctl.read8(DATA_BUF + o) as usize;
        if len < 2 {
            break;
        }
        match dma.ctl.read8(DATA_BUF + o + 1) {
            0x04 if o + 9 <= got => {
                if seen_iface {
                    break;
                }
                if dma.ctl.read8(DATA_BUF + o + 3) == 0 {
                    seen_iface = true;
                    class = Some(dma.ctl.read8(DATA_BUF + o + 5));
                }
            }
            0x05 if seen_iface && o + 7 <= got && dma.ctl.read8(DATA_BUF + o + 3) & 0x03 == 0x02 => {
                let addr = dma.ctl.read8(DATA_BUF + o + 2);
                let mps = dma.ctl.read16(DATA_BUF + o + 4) & 0x7FF;
                if addr & 0x80 != 0 {
                    if b.in_addr == 0 {
                        b.in_addr = addr;
                        b.in_mps = mps;
                    }
                } else if (b.n_out as usize) < b.outs.len() {
                    b.outs[b.n_out as usize] = addr;
                    b.out_mps[b.n_out as usize] = mps;
                    b.n_out += 1;
                }
            }
            _ => {}
        }
        o += len;
    }
    let class = class.unwrap_or(0);
    if !bindable(class) {
        ctx.log_fmt(format_args!(
            "ehci: USB device {:04x}:{:04x} on {} - interface class {:#04x}, not one this host binds for a driver of its own",
            vid, pid, at, class));
        return None;
    }
    if ctl(&ep0, [0x00, 0x05, BOUND_ADDR, 0, 0, 0, 0, 0], 0, false, 5).is_none() {
        ctx.log_fmt(format_args!("ehci: USB device {:04x}:{:04x} on {} did not take Set_Address - not bound", vid, pid, at));
        return None;
    }
    delay::hold_parked(ctx, RECOVERY); // SetAddress recovery (>= 2 ms)
    if ctl(&b.ep0, [0x00, 0x09, cfg, 0, 0, 0, 0, 0], 0, false, 5).is_none() {
        ctx.log_fmt(format_args!("ehci: USB device {:04x}:{:04x} on {} did not take Set_Configuration - not bound", vid, pid, at));
        return None;
    }
    ctx.log_fmt(format_args!(
        "ehci: USB device {:04x}:{:04x} on {} bound for a driver of its own - {} speed, address {}, interface class {:#04x}, bulk IN {:#04x}, {} bulk OUT",
        vid, pid, at, match ep0.speed { 2 => "high", 1 => "low", _ => "full" }, BOUND_ADDR, class, b.in_addr, b.n_out));
    Some(b)
}

/// Where a device is, for a log line: `root port 1`, or `root port 1 > hub port 3`.
struct Where(u8, u8);

impl core::fmt::Display for Where {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        if self.1 == 0 { write!(f, "root port {}", self.0) } else { write!(f, "root port {} > hub port {}", self.0, self.1) }
    }
}

/// `OP_INFO`: who, which binding, and where (`usbfn::INFO_WHERE_AT`), as `xhci` answers it.
fn info(b: &Bound, out: &mut [u8]) -> usize {
    out[1] = usbfn::ST_OK;
    out[2..4].copy_from_slice(&b.vid.to_le_bytes());
    out[4..6].copy_from_slice(&b.pid.to_le_bytes());
    out[usbfn::INFO_GEN_AT..usbfn::INFO_GEN_AT + 4].copy_from_slice(&b.gen.to_le_bytes());
    let w = usbfn::INFO_WHERE_AT;
    out[w] = b.root;
    out[w + 1] = b.hub_port;
    out[w + 2] = b.ep0.addr;
    out[w + 3] = b.in_addr;
    out[w + 4..w + 6].copy_from_slice(&b.in_mps.to_le_bytes());
    out[w + 6] = b.n_out;
    out[w + 7..w + 7 + b.n_out as usize].copy_from_slice(&b.outs[..b.n_out as usize]);
    w + 7 + b.n_out as usize
}

/// `OP_CONTROL` / `OP_CONTROL_ONCE`: `p` is `[op, setup(8), data out...]`. Fills `out[1..]`; returns its length.
fn control_req(ctx: &ServiceContext, hw: &Hw, b: &mut Bound, p: &[u8], tries: u32, out: &mut [u8]) -> usize {
    if p.len() < 9 {
        out[1] = usbfn::ST_BAD_REQUEST;
        return 2;
    }
    let mut setup = [0u8; 8];
    setup.copy_from_slice(&p[1..9]);
    let in_dir = setup[0] & 0x80 != 0;
    let len = u16::from_le_bytes([setup[6], setup[7]]) as usize;
    let data = &p[9..];
    if len > usbfn::CONTROL_MAX || (!in_dir && data.len() != len) {
        out[1] = usbfn::ST_BAD_REQUEST;
        return 2;
    }
    for _ in 0..tries {
        if let Some(n) = control_data(ctx, hw.mmio, hw.dma, hw.op, hw.said, &b.ep0, &setup, len, in_dir, data) {
            out[1] = usbfn::ST_OK;
            if !in_dir {
                cleared_halt(b, &setup);
                return 2;
            }
            let n = n.min(len);
            for (i, o) in out[2..2 + n].iter_mut().enumerate() {
                *o = hw.dma.ctl.read8(DATA_BUF + i);
            }
            return 2 + n;
        }
        delay::hold_parked(ctx, RECOVERY);
    }
    out[1] = usbfn::ST_FAILED;
    2
}

/// A `CLEAR_FEATURE(ENDPOINT_HALT)` the driver sent to one of the bulk endpoints resets that endpoint's data toggle on
/// the device (USB 2.0 9.4.5), so this host's record of it starts at DATA0 again, or the next transfer is ignored.
fn cleared_halt(b: &mut Bound, setup: &[u8; 8]) {
    if setup[0] != 0x02 || setup[1] != 0x01 || setup[2] != 0 || setup[3] != 0 {
        return;
    }
    let ep = setup[4];
    if ep != 0 && ep == b.in_addr {
        b.in_toggle = 0;
    }
    for i in 0..b.n_out as usize {
        if b.outs[i] == ep {
            b.out_toggle[i] = 0;
        }
    }
}

/// `OP_BULK_OUT`: `p` is `[op, out, transfer...]`. The status.
fn bulk_out_req(ctx: &ServiceContext, hw: &Hw, b: &mut Bound, p: &[u8]) -> u8 {
    let (Some(&idx), Some(data)) = (p.get(1), p.get(2..)) else { return usbfn::ST_BAD_REQUEST };
    let i = idx as usize;
    if i >= b.n_out as usize || data.is_empty() || data.len() > usbfn::BULK_OUT_MAX {
        return usbfn::ST_BAD_REQUEST;
    }
    for (k, &v) in data.iter().enumerate() {
        hw.dma.bulk.write8(BULK_OUT_BUF + k, v);
    }
    match bulk(ctx, hw, b, b.outs[i], b.out_mps[i], b.out_toggle[i], false, data.len(), BULK_OUT_WAIT) {
        Ok((n, t)) if n == data.len() => {
            b.out_toggle[i] = t;
            usbfn::ST_OK
        }
        Ok((_, t)) => {
            b.out_toggle[i] = t;
            usbfn::ST_FAILED
        }
        Err(()) => usbfn::ST_FAILED,
    }
}

/// `OP_BULK_IN`: what the device sends within `BULK_IN_WAIT`, nothing included. Fills `out[1..]`.
fn bulk_in_req(ctx: &ServiceContext, hw: &Hw, b: &mut Bound, out: &mut [u8]) -> usize {
    if b.in_addr == 0 {
        out[1] = usbfn::ST_FAILED;
        return 2;
    }
    match bulk(ctx, hw, b, b.in_addr, b.in_mps, b.in_toggle, true, usbfn::BULK_IN_MAX, BULK_IN_WAIT) {
        Ok((n, t)) => {
            b.in_toggle = t;
            out[1] = usbfn::ST_OK;
            for (i, o) in out[2..2 + n].iter_mut().enumerate() {
                *o = hw.dma.bulk.read8(BULK_IN_BUF + i);
            }
            2 + n
        }
        Err(()) => {
            out[1] = usbfn::ST_FAILED;
            2
        }
    }
}

/// One bulk transfer of up to `len` bytes on endpoint `ep_addr` (its direction in bit 7) of the bound device, with
/// data toggle `toggle`, given `budget` to finish. `Ok((bytes moved, next toggle))`; a transfer the device has not
/// finished in time is stopped and what moved is counted, so an IN with nothing to send is `Ok((0, toggle))`. `Err`
/// when the endpoint halted or the bus failed it.
///
/// The queue head is built fresh for each transfer with the toggle taken from the descriptor (DTC), as `control`
/// builds its own, so nothing about the endpoint lives in the controller between transfers: the toggle is this
/// driver's, read back from the descriptor the controller retired - or, for one stopped part way, from the queue
/// head's overlay, which is where the controller keeps a transfer in progress.
fn bulk(ctx: &ServiceContext, hw: &Hw, b: &Bound, ep_addr: u8, mps: u16, toggle: u32, in_dir: bool, len: usize,
        budget: Budget) -> Result<(usize, u32), ()> {
    let (mmio, op, part) = (hw.mmio, hw.op, &hw.dma.bulk);
    for off in (0..0x80).step_by(4) {
        part.write32(off, 0);
    }
    let qh = part.phys_at(BULK_QH) as u32;
    let qtd = part.phys_at(BULK_QTD) as u32;
    let buf = part.phys_at(if in_dir { BULK_IN_BUF } else { BULK_OUT_BUF }) as u32;
    let ep = &b.ep0;
    // Endpoint characteristics: address, endpoint number, speed, DTC (toggle from the descriptor), head of the
    // reclamation list, packet size. C stays 0 - it is for a control endpoint - and NakCnt reload 0 keeps a NAKing
    // endpoint retried until the deadline. Capabilities: Mult 1, and below a hub its translator, as `control` sets it.
    part.write32(BULK_QH, (qh & !0x1F) | (1 << 1));
    part.write32(BULK_QH + 0x04, (ep.addr as u32 & 0x7F) | ((ep_addr as u32 & 0x0F) << 8) | (ep.speed << 12)
        | (1 << 14) | (1 << 15) | ((mps as u32 & 0x7FF) << 16));
    part.write32(BULK_QH + 0x08, (1 << 30) | ((ep.hub_addr as u32 & 0x7F) << 16) | ((ep.port as u32 & 0x7F) << 23));
    part.write32(BULK_QH + 0x10, qtd & !0x1F);
    part.write32(BULK_QH + 0x14, 1);
    let pid = if in_dir { 1u32 } else { 0 };
    part.write32(BULK_QTD, 1);
    part.write32(BULK_QTD + 0x04, 1);
    part.write32(BULK_QTD + 0x08,
        QTD_ACTIVE | (pid << 8) | (3 << 10) | (1 << 15) | ((len as u32) << 16) | ((toggle & 1) << 31));
    // One buffer pointer: every transfer fits in its page (`BULK_BYTES`), so the other four stay 0, never pointing
    // past this part.
    part.write32(BULK_QTD + 0x0C, buf);
    point_async(ctx, mmio, op, qh);
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_IAAD);
    wait_reg(ctx, mmio, op + OP_USBSTS, STS_IAA, true);
    mmio.write32(op + OP_USBSTS, STS_IAA);
    let done = await_hw(ctx, budget, || part.read32(BULK_QTD + 0x08) & QTD_ACTIVE == 0);
    if done {
        let tok = part.read32(BULK_QTD + 0x08);
        if tok & (QTD_HALTED | QTD_ERRMASK) != 0 {
            return Err(());
        }
        return Ok((len - ((tok >> 16) & 0x7FFF) as usize, tok >> 31));
    }
    // Not finished in time: stop the schedule, so the controller lets go of the transfer, then count what moved from
    // the overlay - where the transfer in progress is kept - and leave the descriptor inactive.
    stop_async(ctx, mmio, op);
    let current = part.read32(BULK_QH + 0x0C) & !0x1F;
    let over = part.read32(BULK_QH + 0x18);
    part.write32(BULK_QTD + 0x08, part.read32(BULK_QTD + 0x08) & !QTD_ACTIVE);
    if current != qtd & !0x1F || over & QTD_ACTIVE == 0 {
        return Ok((0, toggle));
    }
    if over & (QTD_HALTED | QTD_ERRMASK) != 0 {
        return Err(());
    }
    Ok((len - ((over >> 16) & 0x7FFF) as usize, over >> 31))
}

/// Stop the async schedule and wait until the controller says it has.
fn stop_async(ctx: &ServiceContext, mmio: &Registers, op: usize) {
    if mmio.read32(op + OP_USBSTS) & STS_ASS != 0 {
        mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) & !CMD_ASE);
        wait_reg(ctx, mmio, op + OP_USBSTS, STS_ASS, false);
    }
}

/// Point the async schedule at the queue head at physical `qh` and run it. `ASYNCLISTADDR` may change only while the
/// schedule is stopped, so it is stopped first - the same steps `control` and `poll_devices` take.
pub fn point_async(ctx: &ServiceContext, mmio: &Registers, op: usize, qh: u32) {
    stop_async(ctx, mmio, op);
    mmio.write32(op + OP_CTRLDSSEGMENT, 0);
    mmio.write32(op + OP_ASYNCLISTADDR, qh & !0x1F);
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_ASE);
    wait_reg(ctx, mmio, op + OP_USBSTS, STS_ASS, true);
}

/// A dead end with nothing to bind on - no arena, no controller, nothing on a port: answer the supervisor's ask and
/// every request (`ST_NO_DEVICE`) for good, polled rather than blocked, as `idle_draining` drained (`main.rs`).
pub fn idle(ctx: &ServiceContext, host: &mut Host) -> ! {
    host.report_if_changed(ctx);
    loop {
        while let Some(m) = gs::ipc::try_recv(ctx) {
            host.serve(ctx, None, None, &m);
        }
        gs::task::sleep_quantum(ctx);
    }
}
