//! The WiFi dongle's hub port, brought up again ON ITS OWN - the rest of the bus left as it is.
//!
//! **Why this exists.** Every fault of the dongle - an EP0 that cannot be repaired after its firmware
//! download stops, or its hub port reading disconnected when it drops off the bus under load - was
//! answered by re-enumerating the WHOLE controller: a host reset, every slot gone, the keyboard and the
//! USB disk re-bound from nothing. On the Pi 4, where every socket is behind the one VL805 hub, that took
//! the disk away for seconds at a time while `fs` was mounting, and a third reset in a row lost the stick
//! altogether (`docs/wifi-usb.md` 37). One misbehaving device took the others down with it.
//!
//! So a dongle behind a hub is now released by its own slot (Disable Slot) and, where it is still
//! there, brought back by resetting ITS hub port and addressing and binding it alone - the same steps
//! the full walk takes for that port (`enumerate_one`), done from the poll loop. The keyboard's slot,
//! the disk's slot and the hub's are not touched. A dongle on a ROOT port is still re-enumerated as
//! before: there the port is the controller's, and nothing else shares it.
//!
//! **The hub's EP0 ring is the poll loop's.** The hub-port probes (`hub_port_status`) keep a persistent
//! cursor on it; the requests here ride that same cursor (`hub_request`), so the controller's dequeue
//! and ours stay in step. The full walk writes at fixed offsets from a freshly reset ring, which cannot
//! be done once the probes have moved on.

use godspeed::driver::delay;
use godspeed::driver::wait::{self, Budget};
use godspeed_sdk::{Dma, Mmio, ServiceContext};

use crate::{
    address_downstream, disable_slot, next_event_at, radio, read_config_and_bind, EvMail, SliceAlloc,
    DEV_STRIDE, EP0_RING_BYTES, PORT_RECOVERY_MS, PROBE_BUF_OFF, TRB_DATA_STAGE, TRB_LINK, TRB_SETUP_STAGE, TRB_SIZE,
    TRB_STATUS_STAGE, TRB_TRANSFER_EVENT,
};

/// How long one hub request may take. A hub answers in about a millisecond; this is the loud floor.
const HUB_REQUEST_MS: u64 = 100;

/// Where the dongle was: the hub it is behind, that hub's slice, the root port the hub hangs off, the
/// hub's think time for the transaction translator, and the port on the hub.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct DonglePort {
    pub hub_slot: u32,
    pub hub_dev: usize,
    pub root_port: u32,
    pub ttt: u32,
    pub hub_port: u32,
}

impl DonglePort {
    pub(crate) fn of(r: &radio::Radio) -> Option<Self> {
        (r.hub_slot != 0).then_some(DonglePort {
            hub_slot: r.hub_slot, hub_dev: r.hub_dev, root_port: r.port, ttt: r.hub_ttt, hub_port: r.hub_port,
        })
    }
}

/// What a re-bind found on the port.
pub(crate) enum Rebound {
    /// The dongle, bound and configured as at enumeration.
    Radio(radio::Radio),
    /// Something that is not the dongle, released again: the caller re-enumerates so it is bound the
    /// ordinary way.
    NotTheDongle,
    /// Nothing could be brought up - the port did not enable, or the device did not answer. Logged here.
    Failed,
}

/// Release the dongle's slot and slice. Nothing else on the bus is touched.
#[allow(clippy::too_many_arguments)]
pub(crate) fn release(
    ctx: &ServiceContext, dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize, r: radio::Radio,
    sa: &mut SliceAlloc, ev_idx: &mut usize, ev_cycle: &mut u32, cmd_idx: &mut usize,
) {
    disable_slot(ctx, dma, mmio, dboff, ir0, r.slot, ev_idx, ev_cycle, cmd_idx);
    sa.free(r.dev_idx);
}

/// One control request to a hub over its EP0, on the poll loop's cursor `cur`/`pcs` for that ring.
/// `wlen` 0 is a request with no data stage (a Set or Clear Feature) and answers `Some(0)`; 4 is a
/// GET_STATUS and answers the port status word. `None` when the hub did not complete it in time or
/// completed it with an error. Matched on the TRB pointer, as the probes are: a completion for an
/// earlier, abandoned request on this ring is passed over, never taken as this one's.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hub_request(
    ctx: &ServiceContext, dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize, hub_slot: u32, hub_dev: usize,
    cur: &mut usize, pcs: &mut u32, ev_idx: &mut usize, ev_cycle: &mut u32, eaten: &mut EvMail,
    bmreq: u32, breq: u32, wval: u32, widx: u32, wlen: u32,
) -> Option<u16> {
    let base = crate::ep0_tr_off(hub_dev);
    // Wrapped as `hub_port_status` wraps it: a Link at the cursor, back to the base, Toggle Cycle set.
    if *cur + 3 * TRB_SIZE >= EP0_RING_BYTES {
        let bp = dma.phys_at(base);
        dma.write32(base + *cur, bp as u32);
        dma.write32(base + *cur + 4, (bp >> 32) as u32);
        dma.write32(base + *cur + 8, 0);
        dma.write32(base + *cur + 12, (TRB_LINK << 10) | (1 << 1) | *pcs);
        *cur = 0;
        *pcs ^= 1;
    }
    // A filed answer for this hub is a probe's that gave up; it is not this request's.
    let _ = eaten.take(hub_slot);
    let c = *pcs;
    let tr = base + *cur;
    let data = wlen > 0;
    dma.write32(tr, bmreq | (breq << 8) | (wval << 16));
    dma.write32(tr + 4, widx | (wlen << 16));
    dma.write32(tr + 8, 8);
    dma.write32(tr + 12, c | (1 << 6) | (TRB_SETUP_STAGE << 10) | (if data { 3 } else { 0 } << 16));
    let mut st = tr + TRB_SIZE;
    if data {
        let dp = dma.phys_at(PROBE_BUF_OFF);
        dma.write32(st, dp as u32);
        dma.write32(st + 4, (dp >> 32) as u32);
        dma.write32(st + 8, wlen);
        dma.write32(st + 12, c | (TRB_DATA_STAGE << 10) | (1 << 16));
        st += TRB_SIZE;
    }
    // Status: OUT after an IN data stage, IN when there is none. IOC, so it is the TRB that completes.
    dma.write32(st, 0);
    dma.write32(st + 4, 0);
    dma.write32(st + 8, 0);
    dma.write32(st + 12, c | (1 << 5) | (TRB_STATUS_STAGE << 10) | (if data { 0 } else { 1 << 16 }));
    let want = dma.phys_at(st);
    *cur = st + TRB_SIZE - base;
    mmio.write32(dboff + hub_slot as usize * 4, 1);
    let mut deadline = wait::Deadline::paced(ctx, Budget::ms(HUB_REQUEST_MS), Budget::ms(1));
    loop {
        match next_event_at(dma, mmio, ir0, ev_idx, ev_cycle, 4_096) {
            Some((TRB_TRANSFER_EVENT, cc, sid, ptr, _, _)) if sid == hub_slot && ptr == want => {
                if cc != 1 && cc != 13 {
                    return None;
                }
                return Some(if data { dma.read16(PROBE_BUF_OFF) } else { 0 });
            }
            // Someone else's transfer - the keyboard's report, the disk's - FILED for its owner, as every
            // consumer of the shared event ring does, so a keystroke is delivered rather than lost.
            Some((TRB_TRANSFER_EVENT, cc, sid, ptr, ep, res)) => eaten.put(sid, ep, cc, res, ptr),
            Some(_) => {}
            None => {}
        }
        if deadline.expired() {
            return None;
        }
        deadline.pause();
    }
}

/// Reset the hub port `at` names, then address and bind what is on it, alone. The steps and their
/// timings are the full walk's for one port (`enumerate_one`): reset, wait for the port to enable,
/// clear the two change bits, the recovery time, the speed, Address Device (three tries), and the
/// configuration read and bind.
///
/// The slice is ZEROED first. The full walk only ever starts from an arena the reset has just zeroed;
/// a slice reused here still holds the last binding's rings, whose TRBs carry the cycle bit a fresh ring
/// starts with, and the controller would run them as new.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rebind(
    ctx: &ServiceContext, dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize, ctx_size: usize,
    at: DonglePort, cur: &mut usize, pcs: &mut u32, sa: &mut SliceAlloc,
    ev_idx: &mut usize, ev_cycle: &mut u32, cmd_idx: &mut usize, eaten: &mut EvMail,
) -> Rebound {
    let hp = at.hub_port;
    let req = |cur: &mut usize, pcs: &mut u32, ev_idx: &mut usize, ev_cycle: &mut u32, eaten: &mut EvMail,
                   bmreq: u32, breq: u32, wval: u32, wlen: u32| {
        hub_request(ctx, dma, mmio, dboff, ir0, at.hub_slot, at.hub_dev, cur, pcs, ev_idx, ev_cycle, eaten,
                    bmreq, breq, wval, hp, wlen)
    };
    // Set_Feature(PORT_RESET), then the port's own word that it is enabled.
    if req(cur, pcs, ev_idx, ev_cycle, eaten, 0x23, 3, 4, 0).is_none() {
        ctx.log_fmt(format_args!("xhci: hub port {} - the hub did not take the port reset; the dongle is not re-bound", hp));
        return Rebound::Failed;
    }
    let mut status = 0u16;
    for _ in 0..12 {
        delay::hold(ctx, Budget::ms(20));
        match req(cur, pcs, ev_idx, ev_cycle, eaten, 0xA3, 0, 0, 4) {
            Some(s) => {
                status = s;
                if s & 0x2 != 0 {
                    break;
                }
            }
            None => break,
        }
    }
    // Clear_Feature(C_PORT_RESET), Clear_Feature(C_PORT_CONNECTION) - both, or the hub keeps reporting
    // the same change (the full walk's reason).
    let _ = req(cur, pcs, ev_idx, ev_cycle, eaten, 0x23, 1, 0x14, 0);
    let _ = req(cur, pcs, ev_idx, ev_cycle, eaten, 0x23, 1, 0x10, 0);
    if status & 0x2 == 0 {
        ctx.log_fmt(format_args!(
            "xhci: hub port {} did not enable after its reset (status {:#06x}) - the dongle is not re-bound", hp, status));
        return Rebound::Failed;
    }
    delay::hold(ctx, Budget::ms(PORT_RECOVERY_MS));
    let pst = req(cur, pcs, ev_idx, ev_cycle, eaten, 0xA3, 0, 0, 4).unwrap_or(status);
    let speed = if pst & (1 << 9) != 0 { 2 } else if pst & (1 << 10) != 0 { 3 } else { 1 };
    let Some(d_idx) = sa.alloc() else {
        ctx.log("xhci: no DMA slice free for the dongle's port - not re-bound");
        return Rebound::Failed;
    };
    let base = crate::device_ctx_off(d_idx);
    for i in (0..DEV_STRIDE).step_by(4) {
        dma.write32(base + i, 0);
    }
    let mut addressed = None;
    for _ in 0..3 {
        addressed = address_downstream(
            ctx, dma, mmio, dboff, ir0, ctx_size, d_idx, hp & 0xF, at.root_port, speed, at.hub_slot, hp, at.ttt,
            ev_idx, ev_cycle, cmd_idx,
        );
        if addressed.is_some() {
            break;
        }
    }
    let Some((dslot, vid, pid, _cls)) = addressed else {
        ctx.log_fmt(format_args!("xhci: hub port {} - Address Device failed three times; the dongle is not re-bound", hp));
        sa.free(d_idx);
        return Rebound::Failed;
    };
    let ids = vid as u32 | (pid as u32) << 16;
    let (hid, disk, dongle, _) = read_config_and_bind(
        ctx, dma, mmio, dboff, ir0, ctx_size, dslot, d_idx, speed, at.root_port, hp & 0xF, at.root_port,
        at.hub_slot, hp, at.ttt, ids, ev_idx, ev_cycle, cmd_idx,
    );
    match dongle {
        Some(mut r) => {
            r.hub_slot = at.hub_slot;
            r.hub_port = hp;
            r.hub_dev = at.hub_dev;
            r.hub_ttt = at.ttt;
            Rebound::Radio(r)
        }
        None => {
            // Something else is on the port now. Released; the caller re-enumerates to bind it properly.
            let _ = (hid, disk);
            ctx.log_fmt(format_args!(
                "xhci: hub port {} now holds {:04x}:{:04x}, not the dongle - released, the bus re-enumerates to bind it", hp, vid, pid));
            disable_slot(ctx, dma, mmio, dboff, ir0, dslot, ev_idx, ev_cycle, cmd_idx);
            sa.free(d_idx);
            Rebound::NotTheDongle
        }
    }
}
