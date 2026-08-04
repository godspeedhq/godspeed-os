// SPDX-License-Identifier: GPL-2.0-only
//! The VL805 xHCI host controller: bringing up a USB keyboard on the Pi 4.
//!
//! ## Where this sits
//!
//! [`super::pcie`] brings the root complex up, finds the controller, and assigns its BAR. Everything
//! from that BAR onwards is here. It ends where [`crate::arch::hid`] begins: this file produces 8-byte
//! HID boot reports; that module turns them into keystrokes, and it is shared with the Pi 2 so the two
//! ports cannot drift apart.
//!
//! ## Why it is in the kernel, and polled
//!
//! Same reason as the Pi 2's DWC2 (`arch/arm/CLAUDE.md`, "Drivers on ARM"): neither ARM port routes
//! device IRQs to userspace yet, so the driver runs in ring 0 and is driven from the core-0 timer tick.
//! That is a **TCB expansion**, and it is the same one §6.4's 2026-07-23 amendment already records for
//! ARM - ring-0 code parsing untrusted device-supplied descriptors, with no IOMMU on this board to
//! confine it. It is recorded rather than papered over; when device IRQs reach userspace this becomes a
//! service and the existing `xhci` service is what it becomes.
//!
//! Polling also removes the MSI question entirely, which matters on a board whose interrupt path from
//! a PCIe endpoint is itself a piece of unwritten work. An 8-byte report every 10 ms is not a
//! throughput problem.
//!
//! ## DMA is not coherent here
//!
//! The BCM2711's PCIe is not I/O-coherent (SEC-28, `arch/CLAUDE.md`). Every structure the controller
//! reads or writes goes through [`mmu::dma_sync`] before the CPU trusts it or the controller is told
//! to look at it. Forgetting one is a driver that works until the line happens to be evicted.
//!
//! ## Every wait is bounded
//!
//! A controller that never answers must not hang the boot (invariant 12). Reset, command completion,
//! port reset and descriptor fetches all count out and report the stage they gave up in, which is what
//! makes a hardware log worth reading.
//!
//! ## Reference
//!
//! The xHCI 1.2 specification for the register and structure layouts, plus Linux's `drivers/usb/host/`
//! for the ordering quirks, read as executable datasheets per the doctrine in `arch/CLAUDE.md`. No code
//! is transliterated; the sequences are.

use core::sync::atomic::{AtomicBool, Ordering};

use super::{mmu, put_dec, put_hex, put_str};
use crate::memory::allocator::alloc_frame;

// --- Capability registers -----------------------------------------------------------------------
const CAP_CAPLENGTH: u64 = 0x00;
const CAP_HCSPARAMS1: u64 = 0x04;
const CAP_HCSPARAMS2: u64 = 0x08;
const CAP_HCCPARAMS1: u64 = 0x10;
const CAP_DBOFF: u64 = 0x14;
const CAP_RTSOFF: u64 = 0x18;

// --- Operational registers (offset from the operational base) ------------------------------------
const OP_USBCMD: u64 = 0x00;
const OP_USBSTS: u64 = 0x04;
const OP_CRCR: u64 = 0x18;
const OP_DCBAAP: u64 = 0x30;
const OP_CONFIG: u64 = 0x38;
const OP_PORTSC_BASE: u64 = 0x400;

const USBCMD_RS: u32 = 1 << 0;
const USBCMD_HCRST: u32 = 1 << 1;
const USBSTS_HCH: u32 = 1 << 0;
const USBSTS_CNR: u32 = 1 << 11;

const PORTSC_CCS: u32 = 1 << 0; // current connect status
const PORTSC_PED: u32 = 1 << 1; // port enabled
const PORTSC_PR: u32 = 1 << 4; // port reset
const PORTSC_PP: u32 = 1 << 9; // port power
const PORTSC_CSC: u32 = 1 << 17; // connect status change
const PORTSC_PRC: u32 = 1 << 21; // port reset change
/// PORTSC bits that are **read-only** (`XHCI_PORT_RO` in Linux): connect status, over-current, the
/// port speed field, and device-removable.
const PORTSC_RO: u32 = (1 << 0) | (1 << 3) | (0xF << 10) | (1 << 30);
/// PORTSC bits that are read-write and must be **preserved** across a modify (`XHCI_PORT_RWS`): the
/// port link state, port power, the indicator control, and the speed-change bits.
const PORTSC_RWS: u32 = (0xF << 5) | (1 << 9) | (0x3 << 14) | (0x7 << 25);

/// Reduce a PORTSC value to something safe to write back.
///
/// **PORTSC cannot be read-modify-written.** Two different classes of bit make a plain write-back
/// destructive, and one of them is silent:
///
/// - The change bits (17..23) are **write-1-to-clear**, so writing a read-back value acknowledges
///   status the driver has not looked at yet.
/// - Bit 1, `PED`, is **write-1-to-DISABLE**. A port that has just been enabled reads back with `PED`
///   set, so writing that value straight back turns the port off again. Nothing reports an error: the
///   `Enable Slot` command still succeeds, because that only allocates a slot, and the failure surfaces
///   one command later as `Address Device` returning **Context State Error** - a code that points at
///   the context, which is the one thing that was correct.
///
/// So the write value is built from scratch: keep the read-only bits and the ones that must survive,
/// drop everything else, then OR in exactly the action intended. This is Linux's
/// `xhci_port_state_to_neutral`, and the reason it exists as a named function there is the same reason
/// it exists here.
fn port_neutral(state: u32) -> u32 {
    state & (PORTSC_RO | PORTSC_RWS)
}

// --- Runtime / interrupter 0 ---------------------------------------------------------------------
const RT_IR0: u64 = 0x20;
const IR_IMAN: u64 = 0x00;
const IR_ERSTSZ: u64 = 0x08;
const IR_ERSTBA: u64 = 0x10;
const IR_ERDP: u64 = 0x18;

// --- TRB types ------------------------------------------------------------------------------------
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_EV_TRANSFER: u32 = 32;
const TRB_EV_CMD_COMPLETE: u32 = 33;

const TRB_CYCLE: u32 = 1 << 0;
const TRB_IOC: u32 = 1 << 5;
const TRB_IDT: u32 = 1 << 6;

/// One Transfer Request Block. 16 bytes, and the layout is the controller's, not ours.
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct Trb {
    param: u64,
    status: u32,
    control: u32,
}

const TRBS_PER_RING: usize = 256;

/// A producer ring: command, or transfer.
struct Ring {
    /// Kernel virtual address of the ring (through the direct map).
    va: u64,
    phys: u64,
    enqueue: usize,
    cycle: u32,
}

impl Ring {
    fn new() -> Option<Ring> {
        let (va, phys) = dma_page()?;
        let r = Ring { va, phys, enqueue: 0, cycle: 1 };
        // The last TRB is a Link back to the start, so the ring is a ring. Without it the controller
        // walks off the end of the page into whatever the allocator handed out next.
        let link = Trb {
            param: phys,
            status: 0,
            control: (TRB_LINK << 10) | (1 << 1) /* toggle cycle */ | r.cycle,
        };
        r.write(TRBS_PER_RING - 1, link);
        Some(r)
    }

    /// Write one TRB, **control dword last**.
    ///
    /// The control dword carries the cycle bit, which is what tells the controller "this entry is
    /// yours now". Writing the struct in one go lets the compiler order the stores however it likes,
    /// so the controller can see a valid cycle bit above a half-written parameter and execute a TRB
    /// that was never finished. Field-by-field with a barrier before the control dword is the ordering
    /// the spec actually requires, and it costs one `dsb`.
    fn write(&self, idx: usize, trb: Trb) {
        // SAFETY: `idx < TRBS_PER_RING` by every caller, and `va` is a page this module owns, mapped
        // through the direct map for the system's lifetime.
        unsafe {
            let p = (self.va + (idx * 16) as u64) as *mut u32;
            p.write_volatile(trb.param as u32);
            p.add(1).write_volatile((trb.param >> 32) as u32);
            p.add(2).write_volatile(trb.status);
            // Publish the first three dwords before the cycle bit that validates them.
            core::arch::asm!("dsb sy", options(nostack));
            p.add(3).write_volatile(trb.control);
        }
        mmu::dma_sync(self.va + (idx * 16) as u64, 16);
    }

    /// Append a TRB and return the physical address it landed at, which is what a completion event
    /// reports back so a reply can be matched to its request.
    fn push(&mut self, param: u64, status: u32, ty: u32, extra: u32) -> u64 {
        let idx = self.enqueue;
        self.write(idx, Trb { param, status, control: (ty << 10) | extra | self.cycle });
        let at = self.phys + (idx * 16) as u64;
        self.enqueue += 1;
        if self.enqueue == TRBS_PER_RING - 1 {
            // Hand the Link TRB the current cycle bit, then flip ours: that is what tells the
            // controller the ring has wrapped rather than run out of work.
            let link = Trb {
                param: self.phys,
                status: 0,
                control: (TRB_LINK << 10) | (1 << 1) | self.cycle,
            };
            self.write(TRBS_PER_RING - 1, link);
            self.enqueue = 0;
            self.cycle ^= 1;
        }
        at
    }
}

/// The whole controller.
struct Xhci {
    /// BAR0, as the kernel names it.
    base: u64,
    op: u64,
    rt: u64,
    db: u64,
    max_ports: u8,
    /// 64-byte contexts rather than 32.
    csz64: bool,

    dcbaa_va: u64,
    cmd: Ring,

    event_va: u64,
    event_phys: u64,
    event_deq: usize,
    event_cycle: u32,

    /// The keyboard, once found.
    kbd: Option<Keyboard>,
}

struct Keyboard {
    slot: u8,
    /// Doorbell target for the interrupt IN endpoint.
    ep_dci: u32,
    ring: Ring,
    /// Where the controller writes each 8-byte report.
    buf_va: u64,
    buf_phys: u64,
    /// Set while a transfer is outstanding, so exactly one is queued at a time.
    pending: bool,
    max_packet: u32,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static mut XHCI: Option<Xhci> = None;
static mut KBD_STATE: KbdState = KbdState {
    last: [0; 6],
    caps: false,
    rep: crate::arch::hid::KeyRepeat::new(),
};

struct KbdState {
    last: [u8; 6],
    caps: bool,
    rep: crate::arch::hid::KeyRepeat,
}

/// Allocate one zeroed page for the controller to share, returning `(kernel va, physical)`.
///
/// Zeroing matters more here than it usually does: the controller reads these as structures with
/// "valid" and "cycle" bits, and a recycled frame's leftovers are a ring full of garbage TRBs the
/// hardware will happily execute.
fn dma_page() -> Option<(u64, u64)> {
    let f = alloc_frame()?;
    let phys = f.phys_addr().0;
    let va = mmu::KERNEL_VA_BASE + phys;
    // SAFETY: a freshly allocated frame, named through the direct map, owned by this module from here.
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096) };
    mmu::dma_sync(va, 4096);
    Some((va, phys))
}

fn rd32(a: u64) -> u32 {
    // SAFETY: a volatile read inside the controller's BAR, which `pcie` mapped and assigned.
    unsafe { (a as *const u32).read_volatile() }
}
fn wr32(a: u64, v: u32) {
    // SAFETY: a volatile write inside the controller's BAR.
    unsafe { (a as *mut u32).write_volatile(v) }
}
/// Write a 64-bit controller register as two 32-bit stores.
///
/// Always two stores, never one 64-bit store: a 64-bit access is only legal when the controller sets
/// AC64, and the low-then-high order is what the spec requires when it is not.
fn wr64(a: u64, v: u64) {
    wr32(a, v as u32);
    wr32(a + 4, (v >> 32) as u32);
}

fn delay_us(us: u64) {
    let hz = super::timer::frequency().max(1);
    let ticks = (hz * us) / 1_000_000;
    let start = super::read_cycle_counter();
    while super::read_cycle_counter().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

/// Bring the controller up and find a keyboard. Returns false (loudly) at the stage that failed.
pub fn init(bar0: u64, bar0_len: u32) -> bool {
    let base = mmu::KERNEL_VA_BASE + bar0;

    // **Check that reads are reaching the controller at all, before deriving anything from them.**
    //
    // A read that does not reach the device does not fault: the BCM2711 root complex completes it with
    // a poison value. `0xDEADDEAD` is what the first bring-up got for every register, and the driver
    // dutifully read `caplen = 0xad` out of it, computed an unaligned operational base, and took an
    // alignment fault - three layers away from the real problem, which was a bridge window that had
    // never been opened. A dword of all-ones is the same story with a different poison.
    //
    // So the first thing checked is not a field but the WHOLE dword, and then the version, which is the
    // one register whose valid values are narrow enough to be a real test.
    let cap0 = rd32(base + CAP_CAPLENGTH);
    if cap0 == 0xDEAD_DEAD || cap0 == 0xFFFF_FFFF || cap0 == 0 {
        put_str(b"xhci: reads are not reaching the controller (first dword ");
        put_hex(cap0 as u64);
        put_str(b") - the bridge window or the BAR is wrong, not the controller\r\n");
        return false;
    }
    let hciversion = (cap0 >> 16) & 0xFFFF;
    if !(0x0090..=0x0130).contains(&hciversion) {
        put_str(b"xhci: implausible HCIVERSION ");
        put_hex(hciversion as u64);
        put_str(b" - this is not an xHCI register block\r\n");
        return false;
    }

    let caplen = (cap0 & 0xFF) as u64;
    // The operational registers are reached by 32-bit accesses to Device memory, which the architecture
    // requires to be naturally aligned. An unaligned `caplen` is therefore not a slow path, it is the
    // alignment fault above - so it is refused here, where the reason is still legible.
    if caplen & 0x3 != 0 {
        put_str(b"xhci: CAPLENGTH ");
        put_hex(caplen);
        put_str(b" is not 4-byte aligned - refusing to derive the operational base from it\r\n");
        return false;
    }
    let hcs1 = rd32(base + CAP_HCSPARAMS1);
    let hcc1 = rd32(base + CAP_HCCPARAMS1);
    let dboff = (rd32(base + CAP_DBOFF) & !0x3) as u64;
    let rtsoff = (rd32(base + CAP_RTSOFF) & !0x1F) as u64;
    let max_slots = (hcs1 & 0xFF) as u8;
    let max_ports = ((hcs1 >> 24) & 0xFF) as u8;
    let csz64 = hcc1 & (1 << 2) != 0;

    // Slots and ports are 8-bit fields, so any value fits and none of them can be rejected on width.
    // 64 is well above what any real controller reports (the VL805 has a handful) and well below the
    // 255 a garbage read produces, which makes it a test rather than a formality.
    if caplen == 0 || caplen > 0x100 || max_ports == 0 || max_slots == 0
        || max_ports > 64 || max_slots > 64
    {
        put_str(b"xhci: capability registers look wrong (caplen ");
        put_hex(caplen);
        put_str(b", hcsparams1 ");
        put_hex(hcs1 as u64);
        put_str(b") - the BAR is probably not mapped where we think\r\n");
        return false;
    }

    // The layout is reported BEFORE it is judged, so a rejection still shows the numbers it judged on.
    // A check that fails without printing its inputs replaces one mystery with another.
    put_str(b"xhci: version ");
    put_hex(hciversion as u64);
    put_str(b", caplen ");
    put_hex(caplen);
    put_str(b", dboff ");
    put_hex(dboff);
    put_str(b", rtsoff ");
    put_hex(rtsoff);
    put_str(b", bar0 ");
    put_hex(bar0_len as u64);
    put_str(b"\r\n");

    // The register block has four regions (capability, operational, runtime, doorbell) and they all
    // live inside BAR0. A doorbell or runtime offset past the end of it means an access that leaves the
    // outbound window - which is not a fault, it is another poison read, and then a controller that
    // never responds to a doorbell it never received.
    let need = (rtsoff.max(dboff) + 0x400).max(caplen + 0x400);
    if need > bar0_len as u64 {
        put_str(b"xhci: register regions do not fit in BAR0 (needs ");
        put_hex(need);
        put_str(b") - the BAR was sized wrong\r\n");
        return false;
    }

    put_str(b"xhci: ");
    put_dec(max_slots as u64);
    put_str(b" slots, ");
    put_dec(max_ports as u64);
    put_str(b" ports, ");
    put_str(if csz64 { b"64-byte contexts\r\n" } else { b"32-byte contexts\r\n" });

    let mut hc = Xhci {
        base,
        op: base + caplen,
        rt: base + rtsoff,
        db: base + dboff,
        max_ports,
        csz64,
        dcbaa_va: 0,
        cmd: match Ring::new() {
            Some(r) => r,
            None => {
                put_str(b"xhci: out of memory building the command ring\r\n");
                return false;
            }
        },
        event_va: 0,
        event_phys: 0,
        event_deq: 0,
        event_cycle: 1,
        kbd: None,
    };

    if !hc.reset() {
        return false;
    }
    if !hc.setup_rings(max_slots) {
        return false;
    }

    // Run.
    wr32(hc.op + OP_USBCMD, rd32(hc.op + OP_USBCMD) | USBCMD_RS);
    let mut n = 0;
    while n < 1000 && rd32(hc.op + OP_USBSTS) & USBSTS_HCH != 0 {
        delay_us(1000);
        n += 1;
    }
    if rd32(hc.op + OP_USBSTS) & USBSTS_HCH != 0 {
        put_str(b"xhci: controller did not leave the halted state\r\n");
        return false;
    }
    put_str(b"xhci: running\r\n");

    hc.scan_ports();

    let found = hc.kbd.is_some();
    // SAFETY: single-threaded boot; written once here, read afterwards only from the timer tick on
    // core 0, which cannot run concurrently with this because interrupts are masked during boot.
    unsafe { XHCI = Some(hc) };
    if found {
        ACTIVE.store(true, Ordering::Release);
        put_str(b"xhci: keyboard ready\r\n");
    } else {
        put_str(b"xhci: no keyboard found on any port - serial input still works\r\n");
    }
    found
}

impl Xhci {
    fn portsc(&self, port: u8) -> u64 {
        self.op + OP_PORTSC_BASE + ((port as u64 - 1) * 0x10)
    }

    /// Halt, then reset, then wait for the controller to say it is ready.
    fn reset(&mut self) -> bool {
        wr32(self.op + OP_USBCMD, rd32(self.op + OP_USBCMD) & !USBCMD_RS);
        let mut n = 0;
        while n < 100 && rd32(self.op + OP_USBSTS) & USBSTS_HCH == 0 {
            delay_us(1000);
            n += 1;
        }
        if rd32(self.op + OP_USBSTS) & USBSTS_HCH == 0 {
            put_str(b"xhci: controller would not halt\r\n");
            return false;
        }

        wr32(self.op + OP_USBCMD, USBCMD_HCRST);
        n = 0;
        // Two conditions, not one: HCRST self-clears when the reset is done, and CNR ("controller not
        // ready") stays set for a while afterwards. Writing registers between the two is the classic
        // way to configure a controller that then ignores everything it was told.
        while n < 1000
            && (rd32(self.op + OP_USBCMD) & USBCMD_HCRST != 0
                || rd32(self.op + OP_USBSTS) & USBSTS_CNR != 0)
        {
            delay_us(1000);
            n += 1;
        }
        if rd32(self.op + OP_USBSTS) & USBSTS_CNR != 0 {
            put_str(b"xhci: still 'controller not ready' 1s after reset\r\n");
            return false;
        }
        put_str(b"xhci: reset complete\r\n");
        true
    }

    fn setup_rings(&mut self, max_slots: u8) -> bool {
        let Some((dcbaa_va, dcbaa_phys)) = dma_page() else {
            put_str(b"xhci: out of memory for the device-context array\r\n");
            return false;
        };
        self.dcbaa_va = dcbaa_va;

        let Some((ev_va, ev_phys)) = dma_page() else {
            put_str(b"xhci: out of memory for the event ring\r\n");
            return false;
        };
        self.event_va = ev_va;
        self.event_phys = ev_phys;

        let Some((erst_va, erst_phys)) = dma_page() else {
            put_str(b"xhci: out of memory for the event-ring segment table\r\n");
            return false;
        };
        // One segment: base, size, reserved.
        // SAFETY: a page this module owns, named through the direct map.
        unsafe {
            (erst_va as *mut u64).write_volatile(ev_phys);
            ((erst_va + 8) as *mut u32).write_volatile(TRBS_PER_RING as u32);
            ((erst_va + 12) as *mut u32).write_volatile(0);
        }
        mmu::dma_sync(erst_va, 16);

        // **Scratchpad buffers, and they are not optional.** A controller that asks for them needs
        // somewhere to keep its own internal state, and it takes them from DCBAA entry 0 - which is
        // otherwise unused, because slot numbers start at 1. Leaving that entry zero on a controller
        // that wants scratchpad is not a slow path or a degraded mode: it is a controller that fails
        // the first Address Device, or wanders into physical address 0. Most xHCI parts want some.
        let hcs2 = rd32(self.base + CAP_HCSPARAMS2);
        let spb = (((hcs2 >> 21) & 0x1F) << 5) | ((hcs2 >> 27) & 0x1F);
        if spb > 0 {
            // The array of pointers is itself one page, which caps how many we can serve. 512 entries
            // is far beyond any real controller's request; refusing loudly beats overrunning the page.
            if spb as usize > 512 {
                put_str(b"xhci: controller wants more scratchpad buffers than one page can index\r\n");
                return false;
            }
            let Some((arr_va, arr_phys)) = dma_page() else {
                put_str(b"xhci: out of memory for the scratchpad pointer array\r\n");
                return false;
            };
            for i in 0..spb as usize {
                let Some((_, p)) = dma_page() else {
                    put_str(b"xhci: out of memory for a scratchpad buffer\r\n");
                    return false;
                };
                // SAFETY: `i < 512`, inside the pointer-array page this module owns.
                unsafe { ((arr_va + (i * 8) as u64) as *mut u64).write_volatile(p) };
            }
            mmu::dma_sync(arr_va, 4096);
            // SAFETY: the DCBAA page this module owns; entry 0 is reserved for exactly this.
            unsafe { (dcbaa_va as *mut u64).write_volatile(arr_phys) };
            put_str(b"xhci: ");
            put_dec(spb as u64);
            put_str(b" scratchpad buffer(s) provided\r\n");
        }
        mmu::dma_sync(dcbaa_va, 4096);

        wr32(self.op + OP_CONFIG, max_slots as u32);
        wr64(self.op + OP_DCBAAP, dcbaa_phys);
        // The command ring pointer carries the initial cycle state in bit 0.
        wr64(self.op + OP_CRCR, self.cmd.phys | 1);

        wr32(self.rt + RT_IR0 + IR_ERSTSZ as u64, 1);
        wr64(self.rt + RT_IR0 + IR_ERDP as u64, ev_phys);
        // ERSTBA last: writing it is what makes the controller latch the table, so everything the table
        // describes has to already be in place.
        wr64(self.rt + RT_IR0 + IR_ERSTBA as u64, erst_phys);
        // Interrupts stay masked - the event ring is polled (see the module header).
        wr32(self.rt + RT_IR0 + IR_IMAN as u64, 0);
        true
    }

    /// Pop one event, if the controller has produced one.
    fn next_event(&mut self) -> Option<Trb> {
        let idx = self.event_deq;
        // SAFETY: `idx < TRBS_PER_RING`, inside the event page this module owns.
        let trb = unsafe {
            mmu::dma_sync(self.event_va + (idx * 16) as u64, 16);
            (self.event_va as *const Trb).add(idx).read_volatile()
        };
        if (trb.control & TRB_CYCLE) != self.event_cycle {
            return None; // the controller has not written this slot yet
        }
        self.event_deq += 1;
        if self.event_deq == TRBS_PER_RING {
            self.event_deq = 0;
            self.event_cycle ^= 1;
        }
        // Tell the controller how far we have consumed, and clear the event handler busy bit (3).
        wr64(
            self.rt + RT_IR0 + IR_ERDP as u64,
            self.event_phys + (self.event_deq * 16) as u64 | (1 << 3),
        );
        Some(trb)
    }

    /// Ring the doorbell for a slot (0 = command ring).
    fn doorbell(&self, slot: u8, target: u32) {
        wr32(self.db + (slot as u64) * 4, target);
    }

    /// Issue a command and wait for its completion event. Returns `(completion code, slot id)`.
    fn command(&mut self, param: u64, ty: u32, extra: u32) -> Option<(u32, u8)> {
        let at = self.cmd.push(param, 0, ty, extra);
        self.doorbell(0, 0);
        let mut waited = 0;
        while waited < 1000 {
            while let Some(ev) = self.next_event() {
                if (ev.control >> 10) & 0x3F == TRB_EV_CMD_COMPLETE && ev.param == at {
                    return Some(((ev.status >> 24) & 0xFF, (ev.control >> 24) as u8));
                }
                // Any other event (a port-status change, say) is not what we are waiting for. Dropping
                // it is correct here: the port scan re-reads PORTSC directly rather than relying on
                // having seen the event.
            }
            delay_us(1000);
            waited += 1;
        }
        put_str(b"xhci: command timed out\r\n");
        None
    }

    /// Look for a connected port, reset it, and try to make a keyboard out of whatever is there.
    fn scan_ports(&mut self) {
        for port in 1..=self.max_ports {
            let sc = rd32(self.portsc(port));
            if sc & PORTSC_CCS == 0 {
                continue;
            }
            put_str(b"xhci: port ");
            put_dec(port as u64);
            put_str(b" has a device (portsc ");
            put_hex(sc as u64);
            put_str(b")\r\n");

            if !self.reset_port(port) {
                continue;
            }
            if self.setup_device(port) {
                return; // one keyboard is enough
            }
        }
    }

    fn reset_port(&mut self, port: u8) -> bool {
        let a = self.portsc(port);
        // Power first: a port that is not powered reports a connect and then refuses to reset. Every
        // write here goes through `port_neutral` - see its comment for what a plain write-back costs.
        let sc = rd32(a);
        if sc & PORTSC_PP == 0 {
            wr32(a, port_neutral(sc) | PORTSC_PP);
            delay_us(20_000);
        }
        wr32(a, port_neutral(rd32(a)) | PORTSC_PR);
        let mut n = 0;
        while n < 500 && rd32(a) & PORTSC_PRC == 0 {
            delay_us(1000);
            n += 1;
        }
        let sc = rd32(a);
        // Acknowledge the reset-complete and connect-change bits explicitly, without disturbing the
        // port's enable state.
        wr32(a, port_neutral(sc) | PORTSC_PRC | PORTSC_CSC);
        if sc & PORTSC_PED == 0 {
            put_str(b"xhci: port did not enable after reset (portsc ");
            put_hex(sc as u64);
            put_str(b")\r\n");
            return false;
        }
        // USB requires a recovery interval after the reset before the device will answer. Addressing it
        // during that window is a transaction error blamed on the address, not on the timing.
        delay_us(20_000);
        put_str(b"xhci: port ");
        put_dec(port as u64);
        put_str(b" enabled after ");
        put_dec(n);
        put_str(b"ms, portsc ");
        put_hex(rd32(a) as u64);
        put_str(b"\r\n");
        true
    }

    /// Context entry size in bytes - 64 when the controller sets CSZ, 32 otherwise. Getting this wrong
    /// puts every endpoint context at the wrong offset and the controller rejects the very first
    /// Address Device with a context error.
    fn ctx_size(&self) -> u64 {
        if self.csz64 { 64 } else { 32 }
    }

    /// Bring up whatever is on a ROOT port. See [`Topology`] for why anything deeper needs more.
    fn setup_device(&mut self, port: u8) -> bool {
        self.setup_at(Topology { root_port: port, route: 0, tt_slot: 0, tt_port: 0, speed: 0 })
    }

    fn setup_at(&mut self, topo: Topology) -> bool {
        let port = topo.root_port;
        let (code, slot) = match self.command(0, TRB_ENABLE_SLOT, 0) {
            Some(v) => v,
            None => return false,
        };
        if code != 1 || slot == 0 {
            put_str(b"xhci: Enable Slot failed (code ");
            put_dec(code as u64);
            put_str(b")\r\n");
            return false;
        }

        let Some((dev_va, dev_phys)) = dma_page() else { return false };
        let Some((in_va, in_phys)) = dma_page() else { return false };
        let Some(mut ep0) = Ring::new() else { return false };
        let Some((buf_va, buf_phys)) = dma_page() else { return false };

        // Register the device context before addressing: the controller writes its output there.
        // SAFETY: the DCBAA page this module owns; `slot` is within the enabled slot count.
        unsafe { ((self.dcbaa_va + (slot as u64) * 8) as *mut u64).write_volatile(dev_phys) };
        mmu::dma_sync(self.dcbaa_va, 4096);

        let cs = self.ctx_size();
        let slot_ctx = in_va + cs; // input control context comes first
        let ep0_ctx = in_va + cs * 2;

        // A device behind a hub has already had its speed reported BY that hub; only a root-port device
        // reads it from PORTSC, which sees the hub rather than what is plugged into it.
        let speed = if topo.route == 0 {
            (rd32(self.portsc(port)) >> 10) & 0xF
        } else {
            topo.speed
        };
        let mps0 = default_max_packet(speed);
        put_str(b"xhci: slot ");
        put_dec(slot as u64);
        put_str(b", link speed ");
        put_dec(speed as u64);
        put_str(b", EP0 max packet ");
        put_dec(mps0 as u64);
        put_str(b"
");

        // SAFETY: the input-context page this module owns. Field offsets are the xHCI 1.2 layout.
        unsafe {
            // Input control: add context flags for slot (bit 0) and EP0 (bit 1).
            ((in_va + 4) as *mut u32).write_volatile(0b11);
            // Slot: context entries = 1, speed, route string, root hub port number, and the
            // transaction translator that reaches a slow device behind a fast hub.
            ((slot_ctx) as *mut u32)
                .write_volatile((1 << 27) | (speed << 20) | (topo.route & 0xF_FFFF));
            ((slot_ctx + 4) as *mut u32).write_volatile((port as u32) << 16);
            if topo.tt_slot != 0 {
                // tt_info: TT hub slot id in bits 7:0, TT port number in bits 15:8.
                ((slot_ctx + 8) as *mut u32)
                    .write_volatile((topo.tt_slot as u32) | ((topo.tt_port as u32) << 8));
            }
            // EP0: Control endpoint (type 4), CErr 3, max packet size, dequeue pointer + DCS.
            ((ep0_ctx + 4) as *mut u32).write_volatile((4 << 3) | (3 << 1) | (mps0 << 16));
            ((ep0_ctx + 8) as *mut u64).write_volatile(ep0.phys | 1);
            ((ep0_ctx + 16) as *mut u32).write_volatile(8); // average TRB length
        }
        mmu::dma_sync(in_va, 4096);

        let (code, _) = match self.command(in_phys, TRB_ADDRESS_DEVICE, (slot as u32) << 24) {
            Some(v) => v,
            None => return false,
        };
        if code != 1 {
            put_str(b"xhci: Address Device failed (code ");
            put_dec(code as u64);
            put_str(b")\r\n");
            return false;
        }
        put_str(b"xhci: device addressed on slot ");
        put_dec(slot as u64);
        put_str(b"\r\n");

        // The DEVICE descriptor first. It is not needed to configure anything - the configuration
        // descriptor carries the endpoint - but it is the only thing that says WHAT this is, and
        // "device is not a HID boot keyboard" is a useless sentence when the device might equally be a
        // hub, a composite device, or a successful transfer that delivered nothing.
        if !self.get_descriptor(slot, &mut ep0, 0x0100, 18, buf_phys, buf_va) {
            put_str(b"xhci: could not read the device descriptor\r\n");
            return false;
        }
        // SAFETY: the descriptor buffer this module owns, just filled and synced by `control_in`.
        let dd = unsafe { core::slice::from_raw_parts(buf_va as *const u8, 18) };
        let dev_class = dd[4];
        put_str(b"xhci: device descriptor: len ");
        put_dec(dd[0] as u64);
        put_str(b" class ");
        put_hex(dev_class as u64);
        put_str(b"/");
        put_hex(dd[5] as u64);
        put_str(b"/");
        put_hex(dd[6] as u64);
        put_str(b" mps0 ");
        put_dec(dd[7] as u64);
        put_str(b" vid:pid ");
        put_hex(((dd[9] as u64) << 8) | dd[8] as u64);
        put_str(b":");
        put_hex(((dd[11] as u64) << 8) | dd[10] as u64);
        put_str(b"\r\n");

        if dd[0] != 18 {
            put_str(b"xhci: the device descriptor did not arrive (length byte wrong) - the control \
                      transfer completed but delivered nothing\r\n");
            return false;
        }

        // Class 9 is a HUB, and on this board that is the expected answer rather than an obstacle: the
        // Pi 4's USB-A sockets hang off an internal VIA hub, so NOTHING plugged into the machine
        // appears on a root port. Walk it.
        if dev_class == 0x09 {
            put_str(b"xhci: this is a USB hub - walking its downstream ports\r\n");
            return self.walk_hub(slot, &mut ep0, topo, buf_phys, buf_va);
        }

        // Read the configuration descriptor: it carries the interface class and the interrupt IN
        // endpoint, which is everything still needed.
        if !self.get_descriptor(slot, &mut ep0, 0x0200, 64, buf_phys, buf_va) {
            put_str(b"xhci: could not read the configuration descriptor\r\n");
            return false;
        }

        let Some((iface_num, ep_addr, ep_mps, ep_interval, cfg_value)) = parse_config(buf_va) else {
            report_config(buf_va);
            return false;
        };

        // Select the configuration, then put the interface in BOOT protocol so the reports are the
        // fixed 8-byte layout `crate::arch::hid` expects rather than an arbitrary report descriptor.
        if !self.control_out(slot, &mut ep0, 0x00, 0x09, cfg_value as u16, 0) {
            put_str(b"xhci: Set Configuration failed\r\n");
            return false;
        }
        if !self.control_out(slot, &mut ep0, 0x21, 0x0B, 0, iface_num as u16) {
            // Not fatal: many keyboards default to boot protocol. Say so rather than assume.
            put_str(b"xhci: Set Protocol(boot) was refused - the device may report non-boot packets\r\n");
        }

        // Configure the interrupt IN endpoint.
        let Some(int_ring) = Ring::new() else { return false };
        let ep_num = ep_addr & 0x0F;
        let dci = (ep_num as u32) * 2 + 1; // IN endpoint

        // SAFETY: the input-context page this module owns.
        unsafe {
            core::ptr::write_bytes(in_va as *mut u8, 0, 4096);
            ((in_va + 4) as *mut u32).write_volatile(1 | (1 << dci)); // slot + this endpoint
            ((slot_ctx) as *mut u32)
                .write_volatile((dci << 27) | (speed << 20) | (topo.route & 0xF_FFFF));
            ((slot_ctx + 4) as *mut u32).write_volatile((port as u32) << 16);
            if topo.tt_slot != 0 {
                ((slot_ctx + 8) as *mut u32)
                    .write_volatile((topo.tt_slot as u32) | ((topo.tt_port as u32) << 8));
            }
            let ep = in_va + cs * (dci as u64 + 1);
            ((ep) as *mut u32).write_volatile(encode_interval(speed, ep_interval) << 16);
            // Interrupt IN = type 7, CErr 3, max packet size.
            ((ep + 4) as *mut u32).write_volatile((7 << 3) | (3 << 1) | (ep_mps << 16));
            ((ep + 8) as *mut u64).write_volatile(int_ring.phys | 1);
            ((ep + 16) as *mut u32).write_volatile(ep_mps);
        }
        mmu::dma_sync(in_va, 4096);

        let (code, _) = match self.command(in_phys, TRB_CONFIGURE_ENDPOINT, (slot as u32) << 24) {
            Some(v) => v,
            None => return false,
        };
        if code != 1 {
            put_str(b"xhci: Configure Endpoint failed (code ");
            put_dec(code as u64);
            put_str(b")\r\n");
            return false;
        }

        let _ = dev_va;
        self.kbd = Some(Keyboard {
            slot,
            ep_dci: dci,
            ring: int_ring,
            buf_va,
            buf_phys,
            pending: false,
            max_packet: ep_mps.min(8),
        });
        true
    }

    /// A control IN transfer: setup, data, status. Used only for descriptors during bring-up.
    /// Configure a hub, then look for a keyboard on each of its downstream ports.
    ///
    /// A hub has to be **configured** before its ports do anything - an unconfigured hub reports every
    /// port as empty and unpowered, which reads exactly like nothing being plugged in. After that the
    /// sequence per port is the USB one: power it, wait for the connect to settle, reset it, read the
    /// speed the hub reports, and address whatever is there with a route string that says how to get
    /// back to it.
    ///
    /// Only one tier is walked. That covers this board (sockets -> internal hub) and a keyboard plugged
    /// directly into it; a hub plugged into a hub is not handled, and says so rather than recursing
    /// into an unbounded walk of someone's docking station.
    fn walk_hub(
        &mut self,
        slot: u8,
        ep0: &mut Ring,
        topo: Topology,
        buf_phys: u64,
        buf_va: u64,
    ) -> bool {
        // Configure it. Reading the configuration descriptor first is what gives us the value to set.
        if !self.get_descriptor(slot, ep0, 0x0200, 64, buf_phys, buf_va) {
            put_str(b"xhci: could not read the hub's configuration descriptor\r\n");
            return false;
        }
        // SAFETY: the descriptor buffer this module owns, just filled.
        let cfg_value = unsafe { *(buf_va as *const u8).add(5) };
        if !self.control_out(slot, ep0, 0x00, 0x09, cfg_value as u16, 0) {
            put_str(b"xhci: could not configure the hub\r\n");
            return false;
        }

        // The hub descriptor says how many downstream ports there are. Class request, not standard.
        if !self.control_in(
            slot, ep0, HUB_GET_DESCRIPTOR.0, HUB_GET_DESCRIPTOR.1, 0x2900, 0, 16, buf_phys, buf_va,
        ) {
            put_str(b"xhci: could not read the hub descriptor\r\n");
            return false;
        }
        // SAFETY: as above.
        let nports = unsafe { *(buf_va as *const u8).add(2) };
        put_str(b"xhci: hub has ");
        put_dec(nports as u64);
        put_str(b" downstream port(s)\r\n");
        if nports == 0 || nports > 15 {
            put_str(b"xhci: implausible downstream port count - not walking it\r\n");
            return false;
        }

        // Power every port first, then let them settle together. Powering one at a time and waiting
        // after each turns a 100 ms settle into 100 ms per port for no benefit.
        for p in 1..=nports {
            self.control_out(
                slot, ep0, HUB_SET_PORT_FEATURE.0, HUB_SET_PORT_FEATURE.1, PORT_FEAT_POWER, p as u16,
            );
        }
        delay_us(200_000);

        for p in 1..=nports {
            if !self.control_in(
                slot, ep0, HUB_GET_PORT_STATUS.0, HUB_GET_PORT_STATUS.1, 0, p as u16, 4, buf_phys,
                buf_va,
            ) {
                continue;
            }
            // SAFETY: the 4-byte port status this module owns, just filled.
            let status = unsafe { (buf_va as *const u16).read_volatile() };
            if status & PS_CONNECTION == 0 {
                continue;
            }
            put_str(b"xhci: hub port ");
            put_dec(p as u64);
            put_str(b" has a device (status ");
            put_hex(status as u64);
            put_str(b")\r\n");

            // Reset it, and give it the USB reset recovery time.
            self.control_out(
                slot, ep0, HUB_SET_PORT_FEATURE.0, HUB_SET_PORT_FEATURE.1, PORT_FEAT_RESET, p as u16,
            );
            let mut n = 0;
            let mut status = 0u16;
            while n < 100 {
                delay_us(10_000);
                n += 1;
                if !self.control_in(
                    slot, ep0, HUB_GET_PORT_STATUS.0, HUB_GET_PORT_STATUS.1, 0, p as u16, 4,
                    buf_phys, buf_va,
                ) {
                    break;
                }
                // SAFETY: as above.
                status = unsafe { (buf_va as *const u16).read_volatile() };
                if status & PS_ENABLE != 0 {
                    break;
                }
            }
            // Acknowledge the change bits, or the hub keeps reporting them.
            self.control_out(
                slot, ep0, HUB_CLEAR_PORT_FEATURE.0, HUB_CLEAR_PORT_FEATURE.1, PORT_FEAT_C_RESET,
                p as u16,
            );
            self.control_out(
                slot, ep0, HUB_CLEAR_PORT_FEATURE.0, HUB_CLEAR_PORT_FEATURE.1,
                PORT_FEAT_C_CONNECTION, p as u16,
            );
            if status & PS_ENABLE == 0 {
                put_str(b"xhci: hub port did not enable after reset\r\n");
                continue;
            }

            // The hub reports the speed; PORTSC cannot, because the root port sees only the hub.
            let speed = if status & PS_LOW_SPEED != 0 {
                2 // low
            } else if status & PS_HIGH_SPEED != 0 {
                3 // high
            } else {
                1 // full
            };
            put_str(b"xhci: hub port ");
            put_dec(p as u64);
            put_str(b" enabled, speed ");
            put_dec(speed as u64);
            put_str(b"\r\n");

            // A low or full speed device behind a high-speed hub reaches the controller only through
            // that hub's transaction translator, which the slot context has to name.
            let needs_tt = speed == 1 || speed == 2;
            let child = Topology {
                root_port: topo.root_port,
                route: (topo.route << 4) | (p as u32 & 0xF),
                tt_slot: if needs_tt { slot } else { 0 },
                tt_port: if needs_tt { p } else { 0 },
                speed,
            };
            if self.setup_at(child) {
                return true; // one keyboard is enough
            }
        }
        put_str(b"xhci: nothing on the hub's ports was a boot keyboard\r\n");
        false
    }

    /// A control IN transfer with an arbitrary request type.
    ///
    /// `req_type` is a parameter rather than hardcoded to `0x80` because hub requests are **class**
    /// requests (`0xA0` to the hub, `0xA3` to one of its ports), and a hub is how anything reaches the
    /// Pi 4's USB-A sockets.
    fn control_in(
        &mut self,
        slot: u8,
        ep0: &mut Ring,
        req_type: u8,
        request: u8,
        value: u16,
        index: u16,
        len: u16,
        buf_phys: u64,
        buf_va: u64,
    ) -> bool {
        // SAFETY: the buffer page this module owns; cleared so a short read cannot be mistaken for old
        // data that happened to parse.
        unsafe { core::ptr::write_bytes(buf_va as *mut u8, 0, 256) };
        mmu::dma_sync(buf_va, 256);

        // Setup stage. The 8 setup bytes travel INSIDE the TRB (immediate data), which is why the
        // parameter field is the request itself rather than a pointer to it.
        let setup: u64 = (req_type as u64)
            | ((request as u64) << 8)
            | ((value as u64) << 16)
            | ((index as u64) << 32)
            | ((len as u64) << 48);
        ep0.push(setup, 8, TRB_SETUP, TRB_IDT | (3 << 16) /* IN data stage */);
        ep0.push(buf_phys, len as u32, TRB_DATA, 1 << 16 /* direction IN */);
        let at = ep0.push(0, 0, TRB_STATUS, TRB_IOC);
        self.doorbell(slot, 1);
        if !self.await_transfer(at) {
            return false;
        }
        mmu::dma_sync(buf_va, 256);
        true
    }

    /// Fetch a standard descriptor. The common case of [`control_in`].
    fn get_descriptor(
        &mut self,
        slot: u8,
        ep0: &mut Ring,
        value: u16,
        len: u16,
        buf_phys: u64,
        buf_va: u64,
    ) -> bool {
        self.control_in(slot, ep0, 0x80, 0x06, value, 0, len, buf_phys, buf_va)
    }

    /// A control OUT transfer with no data stage (SET_CONFIGURATION, SET_PROTOCOL).
    fn control_out(
        &mut self,
        slot: u8,
        ep0: &mut Ring,
        req_type: u8,
        request: u8,
        value: u16,
        index: u16,
    ) -> bool {
        let setup: u64 = (req_type as u64)
            | ((request as u64) << 8)
            | ((value as u64) << 16)
            | ((index as u64) << 32);
        ep0.push(setup, 8, TRB_SETUP, TRB_IDT);
        let at = ep0.push(0, 0, TRB_STATUS, TRB_IOC | (1 << 16));
        self.doorbell(slot, 1);
        self.await_transfer(at)
    }

    fn await_transfer(&mut self, at: u64) -> bool {
        let mut waited = 0;
        while waited < 500 {
            while let Some(ev) = self.next_event() {
                if (ev.control >> 10) & 0x3F == TRB_EV_TRANSFER && ev.param == at {
                    let code = (ev.status >> 24) & 0xFF;
                    // 1 = success, 13 = short packet, which for a descriptor read is the normal case.
                    return code == 1 || code == 13;
                }
            }
            delay_us(1000);
            waited += 1;
        }
        put_str(b"xhci: transfer timed out\r\n");
        false
    }
}

/// Translate an endpoint descriptor's `bInterval` into the xHCI Interval field, **which means
/// different things at different link speeds**.
///
/// xHCI always wants an exponent: the endpoint is serviced every `2^Interval` microframes (125 us
/// each). But `bInterval` in the descriptor is an exponent only for high speed and above; at low and
/// full speed it is a period in **milliseconds**. Feeding the raw value straight through - which is
/// the obvious thing to write, and which reads fine - turns a full-speed keyboard's `bInterval = 10`
/// (10 ms) into `2^10` microframes, or 128 ms per poll. That is not a failure anyone reports as a bug;
/// it is a keyboard that feels broken and a log with nothing wrong in it.
fn encode_interval(speed: u32, b_interval: u8) -> u32 {
    match speed {
        // Low and full speed: bInterval is in milliseconds. One millisecond is 8 microframes, so the
        // exponent is log2(ms) + 3. Clamped to the range the field can express.
        1 | 2 => {
            let ms = b_interval.max(1) as u32;
            let lg = 31 - ms.leading_zeros(); // floor(log2(ms))
            (lg + 3).clamp(3, 10)
        }
        // High speed and above: bInterval is already 1-based exponent.
        _ => (b_interval.max(1) as u32 - 1).min(15),
    }
}

/// EP0's max packet size before the device descriptor has been read, by link speed. The spec gives
/// these as the defaults precisely so the first descriptor fetch has somewhere to start.
fn default_max_packet(speed: u32) -> u32 {
    match speed {
        1 => 8,   // full speed
        2 => 8,   // low speed
        3 => 64,  // high speed
        _ => 512, // super speed and above
    }
}

/// Pull the boot-keyboard interface and its interrupt IN endpoint out of a configuration descriptor.
///
/// Walks by the length byte of each descriptor rather than by assuming an order, and refuses a
/// zero-length one - a malformed descriptor from an untrusted device must end the walk, not spin it
/// forever. This runs in ring 0 on device-supplied bytes (see the module header), so every bound here
/// is doing real work.
/// Where a device sits on the bus. See `setup_device`.
#[derive(Clone, Copy)]
struct Topology {
    /// The root-hub port everything below this point hangs off.
    root_port: u8,
    /// Route string: 4 bits per tier, downstream port number at each. 0 = on the root port itself.
    route: u32,
    /// Slot of the hub providing the transaction translator, or 0 when none is needed.
    tt_slot: u8,
    /// Port on that hub.
    tt_port: u8,
    /// The speed the parent hub reported for this device.
    speed: u32,
}

/// Hub class request types and features (USB 2.0 chapter 11).
const HUB_GET_DESCRIPTOR: (u8, u8) = (0xA0, 0x06);
const HUB_GET_PORT_STATUS: (u8, u8) = (0xA3, 0x00);
const HUB_SET_PORT_FEATURE: (u8, u8) = (0x23, 0x03);
const HUB_CLEAR_PORT_FEATURE: (u8, u8) = (0x23, 0x01);
const PORT_FEAT_RESET: u16 = 4;
const PORT_FEAT_POWER: u16 = 8;
const PORT_FEAT_C_CONNECTION: u16 = 16;
const PORT_FEAT_C_RESET: u16 = 20;
/// wPortStatus bits.
const PS_CONNECTION: u16 = 1 << 0;
const PS_ENABLE: u16 = 1 << 1;
const PS_LOW_SPEED: u16 = 1 << 9;
const PS_HIGH_SPEED: u16 = 1 << 10;

/// Say what a configuration descriptor actually contained, when it did not contain a boot keyboard.
///
/// Walking it a second time to print it is cheap, and it is the difference between "not a keyboard"
/// and a fact. The three interesting cases look identical from the outside: a device that is something
/// else, a device whose interface is a HID keyboard in REPORT protocol rather than boot protocol, and
/// a transfer that completed while delivering nothing.
fn report_config(va: u64) {
    // SAFETY: the descriptor buffer this module owns, filled by the control transfer above.
    let buf = unsafe { core::slice::from_raw_parts(va as *const u8, 256) };
    let total = (buf[2] as usize) | ((buf[3] as usize) << 8);
    put_str(b"xhci: config descriptor: len ");
    put_dec(buf[0] as u64);
    put_str(b" total ");
    put_dec(total as u64);
    put_str(b" interfaces ");
    put_dec(buf[4] as u64);
    put_str(b"\r\n");

    let end = total.min(256);
    let mut i = 9usize;
    while i + 2 <= end {
        let len = buf[i] as usize;
        let ty = buf[i + 1];
        if len < 2 || i + len > end {
            break;
        }
        if ty == 0x04 && len >= 9 {
            put_str(b"xhci:   interface ");
            put_dec(buf[i + 2] as u64);
            put_str(b" class ");
            put_hex(buf[i + 5] as u64);
            put_str(b"/");
            put_hex(buf[i + 6] as u64);
            put_str(b"/");
            put_hex(buf[i + 7] as u64);
            put_str(b" (class/subclass/protocol; 3/1/1 is a boot keyboard)\r\n");
        } else if ty == 0x05 && len >= 7 {
            put_str(b"xhci:   endpoint ");
            put_hex(buf[i + 2] as u64);
            put_str(b" attrs ");
            put_hex(buf[i + 3] as u64);
            put_str(b" interval ");
            put_dec(buf[i + 6] as u64);
            put_str(b"\r\n");
        }
        i += len;
    }
}

fn parse_config(va: u64) -> Option<(u8, u8, u32, u8, u8)> {
    // SAFETY: the buffer page this module owns, filled by the control transfer above.
    let buf = unsafe { core::slice::from_raw_parts(va as *const u8, 256) };
    let total = (buf[2] as usize) | ((buf[3] as usize) << 8);
    let end = total.min(256);
    let cfg_value = buf[5];

    let mut i = 9usize; // past the configuration descriptor itself
    let mut in_boot_kbd = false;
    let mut iface_num = 0u8;
    while i + 2 <= end {
        let len = buf[i] as usize;
        let ty = buf[i + 1];
        if len < 2 || i + len > end {
            break;
        }
        if ty == 0x04 && len >= 9 {
            // Interface: class 3 (HID), subclass 1 (boot), protocol 1 (keyboard).
            iface_num = buf[i + 2];
            in_boot_kbd = buf[i + 5] == 3 && buf[i + 6] == 1 && buf[i + 7] == 1;
        } else if ty == 0x05 && len >= 7 && in_boot_kbd {
            let addr = buf[i + 2];
            let attrs = buf[i + 3];
            if addr & 0x80 != 0 && attrs & 0x03 == 0x03 {
                let mps = (buf[i + 4] as u32) | ((buf[i + 5] as u32) << 8);
                return Some((iface_num, addr, mps & 0x7FF, buf[i + 6], cfg_value));
            }
        }
        i += len;
    }
    None
}

/// Queue one interrupt-IN transfer for the next report.
fn arm_transfer(hc: &mut Xhci) {
    let Some(kbd) = hc.kbd.as_mut() else { return };
    if kbd.pending {
        return;
    }
    // SAFETY: the report buffer this module owns; cleared so a stale report cannot be re-decoded as a
    // fresh keystroke if the transfer completes short.
    unsafe { core::ptr::write_bytes(kbd.buf_va as *mut u8, 0, 8) };
    mmu::dma_sync(kbd.buf_va, 8);
    kbd.ring.push(kbd.buf_phys, kbd.max_packet, TRB_NORMAL, TRB_IOC);
    kbd.pending = true;
    let (slot, dci) = (kbd.slot, kbd.ep_dci);
    hc.doorbell(slot, dci);
}

/// Poll the controller. Called from the core-0 timer tick, every 10 ms.
///
/// The auto-repeat is serviced on EVERY tick, including ticks where no report arrived - a boot keyboard
/// reports only on CHANGE, so a held key sends nothing and the repeat must come from here.
pub fn poll() {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: core 0's timer tick is the only caller after boot, and boot completed before `ACTIVE`
    // was set, so there is no concurrent access to this static.
    let hc = unsafe {
        match (&raw mut XHCI).as_mut().and_then(|o| o.as_mut()) {
            Some(h) => h,
            None => return,
        }
    };
    // SAFETY: same single-caller argument.
    let ks = unsafe { &mut *core::ptr::addr_of_mut!(KBD_STATE) };
    let now = now_us();

    let mut got = false;
    while let Some(ev) = hc.next_event() {
        if (ev.control >> 10) & 0x3F != TRB_EV_TRANSFER {
            continue;
        }
        let code = (ev.status >> 24) & 0xFF;
        if let Some(kbd) = hc.kbd.as_mut() {
            kbd.pending = false;
            // 1 = success, 13 = short packet. Anything else means the transfer did not deliver a
            // report, and decoding the buffer would decode whatever was there before.
            if code != 1 && code != 13 {
                continue;
            }
            mmu::dma_sync(kbd.buf_va, 8);
            let mut report = [0u8; 8];
            // SAFETY: the report buffer this module owns, just synced.
            unsafe {
                core::ptr::copy_nonoverlapping(kbd.buf_va as *const u8, report.as_mut_ptr(), 8)
            };
            got = true;

            // Ctrl+Alt+Del: SIGNAL it on the console stream; the shell, which holds REBOOT, decides
            // what it means (§6.4 SEC-2). The kernel does not interpret keystrokes.
            if crate::arch::hid::is_ctrl_alt_del(&report) {
                super::console_push_byte(crate::arch::hid::CTRL_ALT_DEL_SIGNAL);
            } else {
                crate::arch::hid::decode_keyboard(
                    &report,
                    &mut ks.last,
                    &mut ks.rep,
                    &mut ks.caps,
                    now,
                    super::console_push_byte,
                );
            }
        }
    }
    let _ = got;
    ks.rep.poll(now, super::console_push_byte);
    arm_transfer(hc);
}

/// Microseconds since boot, from the generic timer.
///
/// Truncated to 32 bits on purpose: the repeat logic compares wrap-safely and only ever measures
/// sub-second gaps, so the wrap every ~71 minutes is invisible to it.
fn now_us() -> u32 {
    let hz = super::timer::frequency().max(1);
    ((super::read_cycle_counter() / (hz / 1_000_000).max(1)) & 0xFFFF_FFFF) as u32
}
