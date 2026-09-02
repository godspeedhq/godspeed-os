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
//! It holds `PCI_CFG`, which grants exactly one operation: READ one configuration register. It
//! cannot write configuration space at all, and it cannot leave the hardware selector pointing
//! anywhere, because selecting and reading are one indivisible kernel operation rather than two it
//! could be interrupted between.
//!
//! READ-ONLY IS A PERMANENT BOUNDARY, not a starting point. Configuration space is where every BAR
//! and every command register lives, so write authority over it is write authority over every device
//! on the bus - and the target is chosen by data rather than by the interface, so there is no
//! narrower form of it to grant. Enumeration is inherently read-only, so it takes the read half and
//! only that. If discovery ever needs mutation - destructive BAR sizing is the obvious case - that
//! wants a different mechanism designed and justified on its own terms, never this one widened
//! because widening is convenient.
//!
//! # Bounded, like everything else
//!
//! Fixed arrays, no heap (§26.6.1). The walk is bounded by construction and the result table has a
//! fixed size which the log reports if it fills.

#![no_std]
#![no_main]

use godspeed_sdk::{Message, ServiceContext};

/// Buses to walk, at most.
///
/// A BOUND, NOT A TOPOLOGY. How many buses a machine really has is the machine's business and the
/// walk discovers it - the kernel refuses a bus the bridge does not forward, and `scan` stops there.
/// This only keeps the loop finite if that never happens, so it is set at what any board here could
/// plausibly carry rather than the 256 the spec allows (§26.6).
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
/// THE SELECTOR ENCODING IS THIS SERVICE'S KNOWLEDGE, not the kernel's - and there are two of them,
/// which is the clearest demonstration of why that knowledge belongs here. Both platforms reach
/// configuration space through an index/data register pair; only the encoding differs:
///
/// ```text
/// x86    bus<<16 | dev<<11 | func<<8      (mechanism #1, through CF8/CFC)
/// Pi 4   bus<<20 | dev<<15 | func<<12     (through the root complex config window)
/// ```
///
/// The kernel performs the access and enforces which registers may be reached. It never learns
/// either encoding - so a third platform with a third layout needs no kernel change at all, which is
/// the whole claim D2 makes.
///
/// `None` means the KERNEL REFUSED: either this service does not hold `PCI_CFG`, or the access is
/// one the machine will not admit. An ABSENT DEVICE IS NOT A REFUSAL - the bus floats high and the
/// read returns 0xFFFF_FFFF, which is data for the caller to interpret.
fn cfg_read(ctx: &ServiceContext, bus: u8, dev: u8, func: u8, offset: u8) -> Option<u32> {
    #[cfg(target_arch = "x86_64")]
    let sel = ((bus as u32) << 16) | ((dev as u32) << 11) | ((func as u32) << 8);
    #[cfg(target_arch = "aarch64")]
    let sel = ((bus as u32) << 20) | ((dev as u32) << 15) | ((func as u32) << 12);
    ctx.pci_cfg_read(sel, offset as u16)
}

/// How many device slots to probe on a bus.
///
/// A PCI ROOT BUS can carry 32 devices. A bus BEHIND A BRIDGE, on any machine either of these ports
/// runs on, is the far end of a PCIe link - and a PCIe link is point-to-point, so only device 0 can
/// exist there. Probing 31 slots that cannot be occupied is not merely wasted work: a config read to
/// a device the fabric has no route to is an unsupported request, which some root complexes answer
/// with an abort rather than the all-ones a PC host bridge synthesizes.
///
/// LIMITATION, and the claim that used to sit here was FALSIFIED by the first machine that tested it.
///
/// It said "no machine this runs on" carries devices 1-31 behind a bridge. The Wyse (Intel Gemini
/// Lake) does: its kernel scan - which walks all 256 buses by 32 slots by 8 functions - records 15
/// devices where this walk reports 14. One device sits somewhere this does not look, and the honest
/// statement is that I asserted a fact about every machine from a sample of two.
///
/// The gap is knowingly left open rather than closed blind. Widening the walk means probing slots that
/// cannot be occupied on a point-to-point PCIe link, and on the Pi 4 an out-of-range config read is an
/// SError that halts the machine, not a harmless all-ones. The kernel's admissibility check refuses
/// out-of-RANGE buses, and its own scan reads bus 1 device 1 safely - so widening is probably safe -
/// but "probably safe" is not the bar for a change whose failure mode is a dead board, and this is a
/// REPORTER with no clients, so the cost of the gap today is one unlisted device in a log.
///
/// To close it: probe all 32 slots per admitted bus, and prove it on the Pi 4 before believing it.
fn slots_on(bus: u8) -> u8 {
    if bus == 0 { MAX_DEV } else { 1 }
}

/// Walk the buses and fill `out`; returns how many devices were recorded.
///
/// Functions past 0 are probed only when a PRESENT function 0 says the device is single-function
/// (bit 7 of offset 0x0E) - which keeps the walk from doing eight times the work for the
/// single-function devices that are most of any bus. When function 0 is ABSENT that check cannot run,
/// so every function is probed: a hidden function 0 above a live function is a real chipset pattern,
/// not a malformed device (see `scan`).
///
/// THE WALK STOPS WHERE THE KERNEL STOPS IT. `MAX_BUS` is a bound, not a topology: how many buses
/// actually exist is a property of the machine, and the kernel - which programmed the bridge bus
/// range - is what knows it. A refusal partway through therefore means "the bus range ends here",
/// and is reported as such rather than treated as a failure. Only a refusal on the VERY FIRST read
/// is a real problem, because nothing has succeeded yet to say the path works at all.
fn scan(ctx: &ServiceContext, out: &mut [Found; MAX_FOUND]) -> usize {
    let mut n = 0usize;
    let mut any_ok = false;
    for bus in 0..MAX_BUS {
        for dev in 0..slots_on(bus) {
            for func in 0..MAX_FUNC {
                let id = match cfg_read(ctx, bus, dev, func, 0x00) {
                    Some(v) => {
                        any_ok = true;
                        v
                    }
                    None => {
                        if any_ok {
                            ctx.log_fmt(format_args!(
                                "hw-enumerator: bus {} not admitted - the machine bus range ends below it, stopping here",
                                bus));
                        } else {
                            ctx.log("hw-enumerator: FIRST config read refused - no PCI_CFG capability? nothing enumerated");
                        }
                        return n;
                    }
                };
                if id == 0xFFFF_FFFF || id == 0 {
                    // ABSENT FUNCTION 0 DOES NOT MEAN AN ABSENT DEVICE, which is what this used to
                    // assume - it broke out of the function loop, so a device whose function 0 is
                    // hidden was invisible entirely.
                    //
                    // The Wyse (Intel Gemini Lake) has exactly that: `00:0d.2`, a serial-bus
                    // controller with no function 0 above it. The kernel's scan probes all eight
                    // functions unconditionally and recorded 15 devices; this walk recorded 14, and
                    // the missing one was found only by printing both lists and diffing them.
                    //
                    // So: keep probing. The cost is seven extra config reads for a slot that really
                    // is empty - a few hundred port operations across a whole bus, once, at boot -
                    // against a device the machine has and the report does not. The multifunction
                    // check below still short-circuits the common case, a PRESENT single-function
                    // device, which is what that optimisation was actually for.
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
    // MUST answer, and say what came back, before believing anything the walk reports.
    //
    // 00:00.0 serves on both ports, for different reasons that happen to agree: on a PC it is the
    // host bridge, and on the Pi 4 it is the root complex own config header. Either way something is
    // there on a healthy machine, so all-ones or a refusal is a real signal rather than a quirk of
    // which board this is.
    match cfg_read(&ctx, 0, 0, 0, 0x00) {
        Some(v) => ctx.log_fmt(format_args!(
            "hw-enumerator: probe 00:00.0 vendor/device = {:#010x} (0xffffffff = nothing there, 0 = read returned nothing)", v)),
        None => ctx.log("hw-enumerator: probe 00:00.0 REFUSED by the kernel"),
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
