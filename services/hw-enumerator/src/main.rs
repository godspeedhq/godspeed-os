// SPDX-License-Identifier: GPL-2.0-only
//! `hw-enumerator` - hardware DISCOVERY in userspace (step D2).
//!
//! # What this service is for
//!
//! The kernel knows how to PERFORM an authorised port operation. It does not know what the operation
//! MEANS. This service holds that meaning:
//!
//! ```text
//! kernel:         "Given a valid capability, I permit this operation."
//! hw-enumerator:  "I know 0xCF8 selects a PCI configuration register, how to walk a bus,
//!                  what a class code identifies, and where a device's BARs are."
//! ```
//!
//! Every PCI fact in this file - the address format, the enumeration order, the meaning of offset
//! 0x08 or 0x10, the absent-device sentinel - is hardware SEMANTICS. It used to live in
//! `kernel/src/arch/x86_64/pci.rs` and does not belong in ring 0 (§26.10, §4.4).
//!
//! # What it is NOT
//!
//! A REPORTER, not a manager. It discovers what is present and where its resources are. It does not
//! own devices, drive them, choose drivers, or make lifecycle policy - those belong to the supervisor
//! and to the drivers, and letting this service grow into them is the failure the design explicitly
//! guards against (docs/service-ownership.md, "The service is a REPORTER, and must stay one").
//!
//! # Its authority, and why it is shaped that way
//!
//! It holds `PCI_CFG`: a 32-bit WRITE to 0xCF8 and a 32-bit READ of 0xCFC-0xCFF. Nothing else, and
//! the write side of the DATA port is permanently absent.
//!
//! That asymmetry is the design, not an oversight. CF8 selects which register CFC reaches, so
//! authority to write CFC is authority to write ANY configuration register of ANY device on the bus:
//! every BAR, every command register. There is no narrower form of it at port granularity, because
//! the target is chosen by data rather than by the interface. Enumeration is inherently read-only, so
//! it takes the read half. If discovery ever needs mutation - destructive BAR sizing is the obvious
//! case - that wants a different mechanism designed on its own terms, not this capability widened
//! because widening is convenient.
//!
//! # Bounded, like everything else
//!
//! Fixed arrays, no heap (§26.6.1). The walk is bounded by construction and the result table has a
//! fixed size which the log reports if it fills.

#![no_std]
#![no_main]

use godspeed_sdk::{Message, ServiceContext};

/// PCI configuration ADDRESS port. Writing it selects which register the DATA port reads.
const CONFIG_ADDRESS: u16 = 0xCF8;
/// PCI configuration DATA port.
const CONFIG_DATA: u16 = 0xCFC;

/// Buses to walk. The spec allows 256; every board this runs on uses a handful, and walking 256
/// costs 256x the config reads to find nothing. Bounded at what the hardware actually is (§26.2).
const MAX_BUS: u8 = 4;
/// Devices per bus and functions per device - fixed by the PCI spec, not choices.
const MAX_DEV: u8 = 32;
const MAX_FUNC: u8 = 8;

/// Devices this service will report. Fixed, no heap; a full table SAYS SO rather than truncating in
/// silence (invariant 12).
const MAX_FOUND: usize = 32;

/// One device as the bus describes it - the service's output shape.
///
/// Deliberately facts only: no driver name, no "should this be used", nothing that would make this a
/// manager rather than a reporter.
#[derive(Clone, Copy)]
struct Found {
    bdf: u32,
    class_code: u32,
    vendor: u16,
    device: u16,
    bars: [u32; 6],
    irq_line: u8,
}

impl Found {
    const fn empty() -> Self {
        Found { bdf: 0, class_code: 0, vendor: 0, device: 0, bars: [0; 6], irq_line: 0 }
    }
}

/// Read one 32-bit configuration register.
///
/// THE ADDRESS FORMAT IS THIS SERVICE'S KNOWLEDGE, not the kernel's - and there are two of them,
/// which is the clearest demonstration of why that knowledge belongs here. Both platforms expose
/// configuration space through an INDEX/DATA pair; only the encoding differs:
///
/// ```text
/// x86 (CF8/CFC ports)          bit31 enable | bus<<16 | dev<<11 | func<<8 | off&0xFC
/// Pi 4 (memory-mapped window)  bus<<20 | dev<<15 | func<<12,  then read DATA + off
/// ```
///
/// The kernel performs the access and enforces which register may be touched. It never learns either
/// encoding - so a third platform with a third layout needs no kernel change at all, which is the
/// whole claim D2 makes.
///
/// `None` means the KERNEL REFUSED - a missing capability, or a target outside the grant. An ABSENT
/// DEVICE IS NOT AN ERROR: the bus floats high and the read returns 0xFFFF_FFFF, which is data for
/// the caller to interpret rather than a failure.
fn cfg_read(ctx: &ServiceContext, bus: u8, dev: u8, func: u8, offset: u8) -> Option<u32> {
    #[cfg(target_arch = "x86_64")]
    {
        let addr = 0x8000_0000u32
            | ((bus as u32) << 16)
            | ((dev as u32) << 11)
            | ((func as u32) << 8)
            | ((offset as u32) & 0xFC);
        if !ctx.port_out32(CONFIG_ADDRESS, addr) {
            return None;
        }
        ctx.port_in32(CONFIG_DATA)
    }
    #[cfg(target_arch = "aarch64")]
    {
        // Bus 0 is the root complex itself, whose registers are NOT behind the DATA window - they
        // are the bridge's own control block, sharing pages with things like its reset control. This
        // service does not read them: a root complex is not a device a driver binds to, and reaching
        // it would mean widening the grant to cover registers that can reset the bus. Reported as
        // absent, which is the truth as far as this service's authority extends.
        if bus == 0 {
            return Some(0xFFFF_FFFF);
        }
        let sel = ((bus as u32) << 20) | ((dev as u32) << 15) | ((func as u32) << 12);
        if !ctx.port_out32(0, sel) {
            return None;
        }
        ctx.port_in32((offset as u16) & 0xFC)
    }
}

/// Walk the bus and fill `out`; returns how many devices were recorded.
///
/// Functions past 0 are probed only when the header type says they exist (bit 7 of offset 0x0E) -
/// the standard rule, and it keeps the walk from doing eight times the work for the single-function
/// devices that are most of any bus.
fn scan(ctx: &ServiceContext, out: &mut [Found; MAX_FOUND]) -> usize {
    let mut n = 0usize;
    for bus in 0..MAX_BUS {
        for dev in 0..MAX_DEV {
            for func in 0..MAX_FUNC {
                let id = match cfg_read(ctx, bus, dev, func, 0x00) {
                    Some(v) => v,
                    None => {
                        // The kernel refused. Say so ONCE and stop: continuing would issue thousands
                        // more refused syscalls and bury the reason (§26.7 - reported, not swallowed).
                        ctx.log("hw-enumerator: config read REFUSED by the kernel - no PCI_CFG capability?");
                        return n;
                    }
                };
                if id == 0xFFFF_FFFF || id == 0 {
                    if func == 0 {
                        break; // no function 0 means no device here at all
                    }
                    continue;
                }

                let class = cfg_read(ctx, bus, dev, func, 0x08).unwrap_or(0) >> 8;
                let irq = (cfg_read(ctx, bus, dev, func, 0x3C).unwrap_or(0) & 0xFF) as u8;
                let mut bars = [0u32; 6];
                for (i, b) in bars.iter_mut().enumerate() {
                    *b = cfg_read(ctx, bus, dev, func, 0x10 + (i as u8) * 4).unwrap_or(0);
                }

                if n >= MAX_FOUND {
                    ctx.log_fmt(format_args!(
                        "hw-enumerator: table full at {} - further devices NOT reported", MAX_FOUND));
                    return n;
                }
                out[n] = Found {
                    bdf: ((bus as u32) << 8) | ((dev as u32) << 3) | func as u32,
                    class_code: class,
                    vendor: (id & 0xFFFF) as u16,
                    device: (id >> 16) as u16,
                    bars,
                    irq_line: irq,
                };
                n += 1;

                if func == 0 {
                    let hdr = cfg_read(ctx, bus, dev, 0, 0x0C).unwrap_or(0);
                    if (hdr >> 16) & 0x80 == 0 {
                        break;
                    }
                }
            }
        }
    }
    n
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("hw-enumerator: starting - PCI discovery in USERSPACE (step D2)");

    // GROUND TRUTH before the walk. An empty result is ambiguous on its own - it means either "this
    // machine has no devices" or "the config path is broken and every read returns nothing" - and
    // those look identical in a log unless something is known to be there. So read one register that
    // must answer, and say what came back, before believing anything the walk reports.
    //
    // WHICH device that is differs by platform, and the difference is not incidental:
    //   x86    00:00.0 is the host bridge. Present on every PC, and reached through the same
    //          CF8/CFC path as everything else, so it tests the whole path.
    //   Pi 4   bus 0 is the ROOT COMPLEX, whose registers sit outside the config window and are
    //          deliberately outside this service's grant (see `cfg_read`) - so it always reads
    //          absent here and would be a canary that fails on a healthy machine. The first real
    //          device is behind the bridge on bus 1, and that is what tests the index/data pair.
    #[cfg(target_arch = "x86_64")]
    let (pb, pd, pf, what) = (0u8, 0u8, 0u8, "host bridge");
    #[cfg(target_arch = "aarch64")]
    let (pb, pd, pf, what) = (1u8, 0u8, 0u8, "first device behind the bridge");
    match cfg_read(&ctx, pb, pd, pf, 0x00) {
        Some(v) => ctx.log_fmt(format_args!(
            "hw-enumerator: probe {:02x}:{:02x}.{} ({}) vendor/device = {:#010x} (0xffffffff = nothing there, 0 = read returned nothing)",
            pb, pd, pf, what, v)),
        None => ctx.log_fmt(format_args!(
            "hw-enumerator: probe {:02x}:{:02x}.{} REFUSED by the kernel", pb, pd, pf)),
    }

    let mut found = [Found::empty(); MAX_FOUND];
    let n = scan(&ctx, &mut found);

    // Report what the bus said, one line per device: RAW FACTS for the reader to interpret
    // (utilities/0_conventions.md rule 7). The kernel's own boot scan prints the same devices, so the
    // two can be compared line for line - and that comparison IS the verification story for this
    // service. Two independent walks of one bus must agree before anything relies on this one.
    ctx.log_fmt(format_args!("hw-enumerator: {} device(s) found by USERSPACE enumeration", n));
    for f in found.iter().take(n) {
        ctx.log_fmt(format_args!(
            "hw-enumerator: {:02x}:{:02x}.{} class {:#08x} vendor {:#06x} device {:#06x} bar0 {:#010x} irq {}",
            (f.bdf >> 8) & 0xFF,
            (f.bdf >> 3) & 0x1F,
            f.bdf & 0x7,
            f.class_code,
            f.vendor,
            f.device,
            f.bars[0],
            f.irq_line));
    }

    // Answer questions about what was found. A reporter answers; it does not push.
    //   op 1        -> device count
    //   op 2 + idx  -> that device's facts
    loop {
        let msg = ctx.recv();
        // The caller's one-shot reply cap. No cap means nobody is waiting for an answer, so there is
        // nothing to do but carry on - and NOT reply into the void.
        let Some(reply_cap) = ctx.take_pending_cap() else { continue };
        let p = msg.payload_bytes();
        let reply = match (p.first().copied(), p.get(1).copied()) {
            (Some(1), _) => Message::from_bytes(&(n as u32).to_le_bytes()),
            (Some(2), Some(i)) if (i as usize) < n => {
                let f = found[i as usize];
                let mut b = [0u8; 17];
                b[0..4].copy_from_slice(&f.bdf.to_le_bytes());
                b[4..8].copy_from_slice(&f.class_code.to_le_bytes());
                b[8..10].copy_from_slice(&f.vendor.to_le_bytes());
                b[10..12].copy_from_slice(&f.device.to_le_bytes());
                b[12..16].copy_from_slice(&f.bars[0].to_le_bytes());
                b[16] = f.irq_line;
                Message::from_bytes(&b)
            }
            // An unknown op is ANSWERED, not ignored: a caller blocked on a reply that never comes is
            // the hang nothing above the kernel may cause.
            _ => Message::from_bytes(b"?"),
        };
        let _ = ctx.try_send_by_handle(reply_cap, &reply);
        // Reclaim the slot every time, on every path - a cap left behind on each request fills the
        // table over a long run (§26.6).
        ctx.remove_cap(reply_cap);
    }
}
