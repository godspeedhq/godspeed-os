// SPDX-License-Identifier: GPL-2.0-only
//! The VL805 xHCI host controller: bringing up a USB keyboard on the Pi 4.
//!
//! ## The topology, which is not what you would guess
//!
//! Nothing plugged into a Pi 4 appears on a root port. The four USB-A sockets hang off an **internal
//! VIA hub** (`2109:3431`), which the root port reports as a single high-speed device, so the real
//! shape is:
//!
//! ```text
//!   root port 1 -> VIA hub (high speed)
//!                    +- port 2 -> whatever is in socket 2
//!                    +- port 4 -> a low-speed keyboard
//! ```
//!
//! A low-speed device behind a high-speed hub is reachable only through that hub's **transaction
//! translator**, which the slot context has to name. `link speed 3` on a root port is the tell that
//! you are looking at the hub rather than at a keyboard.
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
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
/// Abort whatever is in flight and take a RUNNING endpoint to Stopped.
///
/// This is the step the first recovery attempt was missing, and the hardware said so exactly:
/// `set-dequeue dci 0x3 refused, cc 0x13` - Context State Error, because Set TR Dequeue Pointer is only
/// legal on a Stopped or Error endpoint. A timed-out transfer does NOT halt the endpoint: the command is
/// still pending, the endpoint is still Running, and Reset Endpoint (which only applies to Halted) does
/// nothing at all. So the ring could never be re-pointed and the recovery could never take.
const TRB_STOP_ENDPOINT: u32 = 15;
/// Take an endpoint out of Halted after a failed transfer. Only legal when it IS halted, which is why
/// a Context State Error here is accepted rather than treated as failure.
const TRB_RESET_ENDPOINT: u32 = 14;
/// Tell the controller where to resume on a reset endpoint. Required after a reset: without it the
/// stale dequeue pointer is kept and the ring stays desynchronised even though the halt is cleared.
const TRB_SET_TR_DEQUEUE: u32 = 16;
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
    /// A ring that names nothing, for `Disk::placeholder` - which exists only to move a `Disk` out of
    /// its option and must never be used as a device.
    const fn null() -> Ring {
        Ring { va: 0, phys: 0, enqueue: 0, cycle: 0 }
    }

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

    /// Re-establish the wrap-around Link TRB after the ring has been zeroed and rewound.
    fn rearm_link(&self) {
        let link = Trb {
            param: self.phys,
            status: 0,
            control: (TRB_LINK << 10) | (1 << 1) | self.cycle,
        };
        self.write(TRBS_PER_RING - 1, link);
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

    /// How long a transfer may take before it is given up on, in milliseconds.
    ///
    /// A field rather than a constant because the two callers have opposite needs and both are right.
    /// Enumeration is rare and legitimately slow, so it gets a generous bound. The hot-plug watcher
    /// runs **inside the timer tick with interrupts masked**, once a second, forever - so a transfer
    /// that hangs there does not merely delay the watcher, it stops the scheduler. Its bound is short
    /// enough that a whole visit costs less than one quantum even if every port fails to answer.
    xfer_ms: u32,

    /// The keyboard, once found.
    kbd: Option<Keyboard>,
    /// The USB stick, once found. The SD card is never a candidate - see `setup_at`.
    disk: Option<Disk>,
    /// The internal hub, kept alive after enumeration so its ports can be watched. Everything plugged
    /// into this board is behind it, so without this a keyboard unplugged once stays dead until reboot.
    hub: Option<Hub>,
}

struct Keyboard {
    slot: u8,
    /// The control ring THIS device's EP0 context names.
    ///
    /// Not the shared enumeration scratch. xHCI keeps an independent dequeue pointer per endpoint
    /// context, so two slots pointing at one ring means each executes the other's TRBs and neither's
    /// dequeue matches the software `enqueue`. The hub already avoided this by swapping a fresh ring
    /// into the scratch and keeping the one its context named; the keyboard and the disk did not, so
    /// every device enumerated after them shared a ring with them.
    ep0: Ring,

    /// Which downstream port of the hub it is on, so a disconnect there can be matched to it.
    hub_port: u8,
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

/// A USB mass-storage stick, spoken to over Bulk-Only Transport.
struct Disk {
    slot: u8,
    /// The control ring THIS device's EP0 context names.
    ///
    /// Not the shared enumeration scratch. xHCI keeps an independent dequeue pointer per endpoint
    /// context, so two slots pointing at one ring means each executes the other's TRBs and neither's
    /// dequeue matches the software `enqueue`. The hub already avoided this by swapping a fresh ring
    /// into the scratch and keeping the one its context named; the keyboard and the disk did not, so
    /// every device enumerated after them shared a ring with them.
    ep0: Ring,

    in_dci: u32,
    out_dci: u32,
    in_ring: Ring,
    out_ring: Ring,
    /// One page for block data.
    data_va: u64,
    data_phys: u64,
    /// The CBW at offset 0 and the CSW at offset 64 of one page, kept apart so a device that writes
    /// the status early cannot land it on top of the command still being read.
    cmd_va: u64,
    cmd_phys: u64,
    sectors: u64,
    tag: u32,
    /// Which hub port it is on, so its removal stands the DISK down and not the keyboard.
    hub_port: u8,
}

impl Disk {
    /// A stand-in used only to move a `Disk` out of its option while it is being operated on. Never
    /// reachable as a disk: every field is zero, so any use faults on the first transfer rather than
    /// quietly reading block zero of nothing.
    fn placeholder() -> Disk {
        Disk {
            slot: 0,
            ep0: Ring::null(),
            in_dci: 0,
            out_dci: 0,
            in_ring: Ring { va: 0, phys: 0, enqueue: 0, cycle: 1 },
            out_ring: Ring { va: 0, phys: 0, enqueue: 0, cycle: 1 },
            data_va: 0,
            data_phys: 0,
            cmd_va: 0,
            cmd_phys: 0,
            sectors: 0,
            tag: 0,
            hub_port: 0,
        }
    }
}

/// Sector count of the attached USB disk, 0 when there is none. Published as an atomic because the
/// block-driver asks for it from a SYSCALL while the driver itself lives behind the tick.
static DISK_SECTORS: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// Whoever holds this owns the controller: the event ring, the command ring, and every device on it.
///
/// **Three unrelated contexts drive this hardware** - the timer tick (keyboard), a syscall (disk I/O),
/// and the idle loop (hot-plug enumeration) - and they all consume the SAME event ring. Two of them
/// running at once is not a slow path, it is one of them silently eating the other's completion event
/// and waiting forever for it.
///
/// So exclusion is a claim rather than a masked section: the holder runs with interrupts ENABLED, and
/// everyone else stands aside. The tick skips a keyboard sample. The disk answers BUSY, which the block
/// driver already knows how to retry (`with_busy_retry` in `usbdisk.rs`). Hot-plug waits a second and
/// tries again. This is the protocol the Pi 2 arrived at for the same three-way collision, and for the
/// same reason: masking instead would suppress the tick for the ~100 ms an enumeration takes.
static USB_CLAIM: AtomicBool = AtomicBool::new(false);

/// Take the controller, or report that someone else has it.
fn claim_usb() -> bool {
    USB_CLAIM
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

fn release_usb() {
    USB_CLAIM.store(false, Ordering::Release);
}

/// The internal hub and what we believe is plugged into it.
struct Hub {
    slot: u8,
    ep0: Ring,
    nports: u8,
    buf_va: u64,
    buf_phys: u64,
    /// Bit `p` set = we believe port `p` holds a device.
    connected: u16,
    /// Per-port attempts to bring a device up. **Bounded**: a device that arrives and refuses to
    /// enumerate must not be retried forever, which is the unbounded-retry shape §26.6 forbids. Reset
    /// when the port empties, so an unplug/replug always gets a fresh set.
    tries: [u8; 16],
}

/// How many times a single arrival is retried before the port is left alone until it changes again.
const HOTPLUG_MAX_TRIES: u8 = 3;

/// Counter value at the last hot-plug visit; 0 = never. One visit per second is plenty for a human
/// plugging a cable, and it keeps the steady-state cost to one control transfer per port per second.
static HOTPLUG_LAST: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// True once anything USB is worth polling - a keyboard, or a hub whose ports are being watched.
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

/// Transient pages every enumeration needs and no device keeps: the input context, the control-endpoint
/// ring, and the descriptor buffer.
///
/// **Allocated once and reused**, because enumeration has ~40 failure paths and none of them freed
/// anything. A device that refuses to come up is retried three times per arrival, so repeated plugging
/// leaked pages steadily - the unbounded-resource shape §26.6 forbids, arrived at by omission rather
/// than by design. A fixed arena removes the question instead of answering it forty times: the pages
/// are cheap, the footprint is three pages forever, and bring-up is serialised under `USB_CLAIM` so
/// there is never a second user.
struct Scratch {
    in_va: u64,
    in_phys: u64,
    ep0: Ring,
    buf_va: u64,
    buf_phys: u64,
}

static mut SCRATCH: Option<Scratch> = None;

/// Hand out the enumeration scratch, allocating it the first time. The control ring is REWOUND rather
/// than rebuilt: a fresh device starts its own transfers at the ring's head, and leaving the previous
/// device's enqueue position would have the controller walk stale TRBs it has already executed.
fn scratch() -> Option<&'static mut Scratch> {
    // SAFETY: bring-up is serialised under `USB_CLAIM`, so this is the only live borrow.
    let slot = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
    if slot.is_none() {
        let (in_va, in_phys) = dma_page()?;
        let ep0 = Ring::new()?;
        let (buf_va, buf_phys) = dma_page()?;
        *slot = Some(Scratch { in_va, in_phys, ep0, buf_va, buf_phys });
    }
    let sc = slot.as_mut()?;
    // SAFETY: pages this module owns, sized 4096 by `dma_page`.
    unsafe {
        core::ptr::write_bytes(sc.in_va as *mut u8, 0, 4096);
        core::ptr::write_bytes(sc.buf_va as *mut u8, 0, 4096);
        core::ptr::write_bytes(sc.ep0.va as *mut u8, 0, 4096);
    }
    sc.ep0.enqueue = 0;
    sc.ep0.cycle = 1;
    sc.ep0.rearm_link();
    mmu::dma_sync(sc.in_va, 4096);
    mmu::dma_sync(sc.buf_va, 4096);
    mmu::dma_sync(sc.ep0.va, 4096);
    Some(sc)
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
        xfer_ms: 500,
        kbd: None,
        disk: None,
        hub: None,
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
        // NOT a reason to stop polling. `walk_hub` already armed the watcher if a hub was found, and
        // booting with the keyboard unplugged is the ordinary case that hot-plug exists to handle -
        // switching the poll off here would mean it only ever worked if the cable was in at power-on.
        put_str(b"xhci: no keyboard yet - serial input works, and the hub is being watched\r\n");
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
                // A port-status change is safely dropped - the port scan re-reads PORTSC rather than
                // relying on having seen the event. A TRANSFER event is not: it may be the keyboard's,
                // and dropping that one wedges it. See `absorb_foreign_event`.
                self.absorb_foreign_event(&ev);
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
        self.setup_at(Topology { tier: 0, root_port: port, route: 0, tt_slot: 0, tt_port: 0, speed: 0 })
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

        // The three transient pages come from the reusable arena, so the ~40 failure paths below leak
        // nothing. Only the DEVICE CONTEXT is per-device, because the controller keeps writing to it for
        // as long as the slot is addressed - it cannot be shared with the next enumeration.
        //
        // The device context IS still allocated per bring-up and not freed on the failure paths after
        // this point. That is one page per failed attempt rather than four, and it is bounded by the
        // three-attempt cap in `hotplug_tick`; freeing it properly wants the slot teardown that
        // `stand_down` does, applied to a device that never finished coming up. Recorded rather than
        // claimed fixed (§26.3).
        let Some((dev_va, dev_phys)) = dma_page() else { return false };
        let Some(sc) = scratch() else { return false };
        let (in_va, in_phys, buf_va, buf_phys) = (sc.in_va, sc.in_phys, sc.buf_va, sc.buf_phys);
        let mut ep0 = &mut sc.ep0;

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
            // One tier, and say so rather than recursing - which is what `walk_hub`'s doc has always
            // claimed and nothing enforced. The walk is MUTUALLY RECURSIVE (walk_hub ->
            // bring_up_hub_port -> setup_at -> walk_hub) with its depth chosen by a device-supplied
            // `bDeviceClass`, on the KERNEL STACK. A device reporting class 9 forever, or a real chain
            // of hubs in a docking station, walks until the stack meets its guard page.
            //
            // The Pi 4's own USB-A sockets are behind an internal hub, so tier 1 is the NORMAL case
            // here; tier 2 is a hub plugged into that. Refusing it loudly is a bounded, explicable
            // limit (§26.6); recursing is a stack overflow whose depth a stranger's hardware picks.
            if topo.tier >= HUB_TIER_MAX {
                put_str(b"xhci: hub beyond tier 1 - not walked (a hub behind a hub is not supported)\r\n");
                return false;
            }
            put_str(b"xhci: this is a USB hub - walking its downstream ports\r\n");
            return self.walk_hub(slot, &mut ep0, topo, buf_phys, buf_va);
        }

        // Read the configuration descriptor: it carries the interface class and the interrupt IN
        // endpoint, which is everything still needed.
        if !self.get_descriptor(slot, &mut ep0, 0x0200, 64, buf_phys, buf_va) {
            put_str(b"xhci: could not read the configuration descriptor\r\n");
            return false;
        }

        // A mass-storage device is the OTHER thing worth keeping on this board. `fs` needs a disk, and
        // the SD card cannot be one: it is the boot medium, and GSFS's superblock lives at LBA 0 where
        // its partition table is. The Pi 2 port learned that by destroying two cards. So storage here
        // means a USB stick, exactly as it does there.
        if let Some(msc) = parse_msc(buf_va) {
            return self.setup_msc(slot, &mut ep0, msc, buf_phys, buf_va);
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
        let (kbd_buf_va, kbd_buf_phys) = match dma_page() {
            Some(p) => p,
            None => return false,
        };
        // Take the ring this device's EP0 context names, leaving a fresh one behind for the next
        // enumeration - the same swap the hub does, and for the same reason.
        let mut kbd_ep0 = match Ring::new() {
            Some(r) => r,
            None => return false,
        };
        core::mem::swap(&mut kbd_ep0, ep0);
        self.kbd = Some(Keyboard {
            slot,
            ep0: kbd_ep0,
            hub_port: 0,
            ep_dci: dci,
            ring: int_ring,
            // Its OWN page, not the shared enumeration scratch. The `Scratch` doc says "no device
            // keeps" that buffer, and three long-lived owners were keeping it: the keyboard's report
            // target, the hub's port-status target, and the disk's CBW/CSW page. The keyboard's
            // interrupt transfer stays permanently ARMED, so the controller DMAs into it on any
            // keypress regardless of who holds USB_CLAIM - which serialises the driver, not the
            // hardware. A keystroke therefore overwrote hub port status (the all-zero reads that were
            // worked around as "a read that did not happen") and the disk's CBW signature mid-command,
            // which is a strong candidate for the BOT wedges recovery was built to clean up after.
            buf_va: kbd_buf_va,
            buf_phys: kbd_buf_phys,
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
        // after each turns a 200 ms settle into 200 ms per port for no benefit.
        for p in 1..=nports {
            self.control_out(
                slot, ep0, HUB_SET_PORT_FEATURE.0, HUB_SET_PORT_FEATURE.1, PORT_FEAT_POWER, p as u16,
            );
        }
        delay_us(200_000);

        // Keep the hub. Everything on this board is behind it, so a keyboard unplugged once would stay
        // dead until reboot without something still holding its control endpoint.
        let (hub_buf_va, hub_buf_phys) = match dma_page() {
            Some(p) => p,
            None => return false,
        };
        let mut hub = Hub {
            slot,
            ep0: match Ring::new() {
                Some(r) => r,
                None => return false,
            },
            nports,
            // Its OWN page, not the shared enumeration scratch. The `Scratch` doc says "no device
            // keeps" that buffer, and three long-lived owners were keeping it: the keyboard's report
            // target, the hub's port-status target, and the disk's CBW/CSW page. The keyboard's
            // interrupt transfer stays permanently ARMED, so the controller DMAs into it on any
            // keypress regardless of who holds USB_CLAIM - which serialises the driver, not the
            // hardware. A keystroke therefore overwrote hub port status (the all-zero reads that were
            // worked around as "a read that did not happen") and the disk's CBW signature mid-command,
            // which is a strong candidate for the BOT wedges recovery was built to clean up after.
            buf_va: hub_buf_va,
            buf_phys: hub_buf_phys,
            connected: 0,
            tries: [0; 16],
        };
        // The ring built above is a placeholder to satisfy the type; the real control endpoint is the
        // one already addressed, so hand that over and let the placeholder go.
        core::mem::swap(&mut hub.ep0, ep0);

        let mut found = false;
        for p in 1..=nports.min(15) {
            if self.hub_port_connected(&mut hub, p) {
                hub.connected |= 1 << p;
                // Keep walking after a device is brought up: this board can have a stick on one port
                // and a keyboard on another, and stopping at the first would silently pick whichever
                // socket happened to be lower-numbered.
                if self.bring_up_hub_port(&mut hub, topo, p) {
                    found = true;
                }
            }
        }
        if !found {
            put_str(b"xhci: nothing on the hub's ports was a boot keyboard (yet - hot-plug is watching)\r\n");
        }
        self.hub = Some(hub);
        // Watch the ports whether or not a keyboard was found: plugging one in later is the whole point.
        ACTIVE.store(true, Ordering::Release);
        found
    }

    /// Is there something on this hub port right now?
    fn hub_port_connected(&mut self, hub: &mut Hub, p: u8) -> bool {
        let (slot, bp, bv) = (hub.slot, hub.buf_phys, hub.buf_va);
        if !self.control_in(
            slot, &mut hub.ep0, HUB_GET_PORT_STATUS.0, HUB_GET_PORT_STATUS.1, 0, p as u16, 4, bp, bv,
        ) {
            return false;
        }
        // SAFETY: the 4-byte port status this module owns, just filled by the transfer above.
        let status = unsafe { (bv as *const u16).read_volatile() };
        status & PS_CONNECTION != 0
    }

    /// Reset one hub port and address whatever is on it. Used by the boot walk and by hot-plug, so a
    /// device plugged in later goes through exactly the sequence that worked at boot.
    fn bring_up_hub_port(&mut self, hub: &mut Hub, topo: Topology, p: u8) -> bool {
        let (slot, bp, bv) = (hub.slot, hub.buf_phys, hub.buf_va);
        put_str(b"xhci: hub port ");
        put_dec(p as u64);
        put_str(b" has a device\r\n");

        self.control_out(
            slot, &mut hub.ep0, HUB_SET_PORT_FEATURE.0, HUB_SET_PORT_FEATURE.1, PORT_FEAT_RESET,
            p as u16,
        );
        let mut n = 0;
        let mut status = 0u16;
        while n < 100 {
            delay_us(10_000);
            n += 1;
            if !self.control_in(
                slot, &mut hub.ep0, HUB_GET_PORT_STATUS.0, HUB_GET_PORT_STATUS.1, 0, p as u16, 4, bp,
                bv,
            ) {
                break;
            }
            // SAFETY: as above.
            status = unsafe { (bv as *const u16).read_volatile() };
            if status & PS_ENABLE != 0 {
                break;
            }
        }
        // Acknowledge the change bits, or the hub keeps reporting the same event forever.
        self.control_out(
            slot, &mut hub.ep0, HUB_CLEAR_PORT_FEATURE.0, HUB_CLEAR_PORT_FEATURE.1, PORT_FEAT_C_RESET,
            p as u16,
        );
        self.control_out(
            slot, &mut hub.ep0, HUB_CLEAR_PORT_FEATURE.0, HUB_CLEAR_PORT_FEATURE.1,
            PORT_FEAT_C_CONNECTION, p as u16,
        );
        if status & PS_ENABLE == 0 {
            put_str(b"xhci: hub port did not enable after reset\r\n");
            return false;
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

        // A low or full speed device behind a high-speed hub reaches the controller only through that
        // hub's transaction translator, which the slot context has to name.
        let needs_tt = speed == 1 || speed == 2;
        let child = Topology {
            tier: topo.tier.saturating_add(1), // one hub deeper; the dispatch refuses past HUB_TIER_MAX
            root_port: topo.root_port,
            route: (topo.route << 4) | (p as u32 & 0xF),
            tt_slot: if needs_tt { slot } else { 0 },
            tt_port: if needs_tt { p } else { 0 },
            speed,
        };
        if self.setup_at(child) {
            // Record the port on whichever device this actually was. Setting it unconditionally on the
            // KEYBOARD was wrong in a way that only shows up later: bringing up a stick on port 2 would
            // relabel the keyboard as being on port 2, so unplugging the stick stood the keyboard down
            // and unplugging the keyboard did nothing.
            if let Some(d) = self.disk.as_mut() {
                if d.hub_port == 0 {
                    d.hub_port = p;
                }
            }
            if let Some(k) = self.kbd.as_mut() {
                if k.hub_port == 0 {
                    k.hub_port = p;
                }
            }
            return true;
        }
        false
    }

    /// Configure a USB stick's two bulk endpoints and ask it how big it is.
    fn setup_msc(
        &mut self,
        slot: u8,
        ep0: &mut Ring,
        msc: MscInfo,
        buf_phys: u64,
        buf_va: u64,
    ) -> bool {
        if !self.control_out(slot, ep0, 0x00, 0x09, msc.cfg_value as u16, 0) {
            put_str(b"xhci: mass storage: Set Configuration failed\r\n");
            return false;
        }

        let Some(in_ring) = Ring::new() else { return false };
        let Some(out_ring) = Ring::new() else { return false };
        let Some((data_va, data_phys)) = dma_page() else { return false };

        let in_dci = ((msc.in_ep & 0x0F) as u32) * 2 + 1; // IN endpoints are odd
        let out_dci = ((msc.out_ep & 0x0F) as u32) * 2; // OUT endpoints are even
        let last = in_dci.max(out_dci);

        // Both endpoints go in ONE Configure Endpoint. Issuing two commands would leave the device
        // half-configured between them, and the second would be rebuilding a slot context the first had
        // already changed.
        let Some((in_va, in_phys)) = dma_page() else { return false };
        let cs = self.ctx_size();
        // SAFETY: the input-context page this module owns; offsets are the xHCI 1.2 layout.
        unsafe {
            ((in_va + 4) as *mut u32).write_volatile(1 | (1 << in_dci) | (1 << out_dci));
            // The slot context must be re-stated, and `Context Entries` must reach the HIGHEST endpoint
            // in use - not the one being added. A lower value leaves the controller believing the upper
            // endpoint does not exist, and its doorbell is then simply ignored.
            let dev_ctx_src = self.dcbaa_va + (slot as u64) * 8;
            let _ = dev_ctx_src;
            ((in_va + cs) as *mut u32).write_volatile(last << 27);
            for (dci, ring, epty) in [(in_dci, &in_ring, 2u32), (out_dci, &out_ring, 2u32)] {
                let ep = in_va + cs * (dci as u64 + 1);
                // Bulk IN is endpoint type 6, Bulk OUT is 2.
                let ty = if dci == in_dci { 6 } else { epty };
                ((ep + 4) as *mut u32)
                    .write_volatile((ty << 3) | (3 << 1) | (msc.mps << 16));
                ((ep + 8) as *mut u64).write_volatile(ring.phys | 1);
                ((ep + 16) as *mut u32).write_volatile(msc.mps);
            }
        }
        mmu::dma_sync(in_va, 4096);

        let (code, _) = match self.command(in_phys, TRB_CONFIGURE_ENDPOINT, (slot as u32) << 24) {
            Some(v) => v,
            None => return false,
        };
        if code != 1 {
            put_str(b"xhci: mass storage: Configure Endpoint failed (code ");
            put_dec(code as u64);
            put_str(b")\r\n");
            return false;
        }

        let (disk_cmd_va, disk_cmd_phys) = match dma_page() {
            Some(p) => p,
            None => return false,
        };
        let mut disk_ep0 = match Ring::new() {
            Some(r) => r,
            None => return false,
        };
        core::mem::swap(&mut disk_ep0, ep0);
        let mut disk = Disk {
            slot,
            ep0: disk_ep0,
            in_dci,
            out_dci,
            in_ring,
            out_ring,
            data_va,
            data_phys,
            // Its OWN page, not the shared enumeration scratch. The `Scratch` doc says "no device
            // keeps" that buffer, and three long-lived owners were keeping it: the keyboard's report
            // target, the hub's port-status target, and the disk's CBW/CSW page. The keyboard's
            // interrupt transfer stays permanently ARMED, so the controller DMAs into it on any
            // keypress regardless of who holds USB_CLAIM - which serialises the driver, not the
            // hardware. A keystroke therefore overwrote hub port status (the all-zero reads that were
            // worked around as "a read that did not happen") and the disk's CBW signature mid-command,
            // which is a strong candidate for the BOT wedges recovery was built to clean up after.
            cmd_va: disk_cmd_va,
            cmd_phys: disk_cmd_phys,
            sectors: 0,
            tag: 1,
            hub_port: 0,
        };

        // A stick that has just been reset answers NOT READY for a while. Asking once and believing the
        // answer reports a perfectly good disk as absent.
        let mut n = 0;
        while n < 20 && !self.scsi_test_unit_ready(&mut disk) {
            delay_us(100_000);
            n += 1;
        }

        match self.scsi_read_capacity(&mut disk) {
            Some(sectors) => {
                disk.sectors = sectors;
                put_str(b"xhci: USB disk ready - ");
                put_dec(sectors);
                put_str(b" sectors (");
                put_dec(sectors / 2048);
                put_str(b" MiB)\r\n");
                self.disk = Some(disk);
                DISK_SECTORS.store(sectors, Ordering::Release);
                ACTIVE.store(true, Ordering::Release);
                true
            }
            None => {
                put_str(b"xhci: mass storage did not report a capacity - not usable as a disk\r\n");
                false
            }
        }
    }

    /// One Bulk-Only Transport command: CBW out, optional data, CSW in.
    ///
    /// Returns the CSW status byte (0 = good). BOT is deliberately literal here - three transfers, in
    /// order, every time - because the interesting failures are all about a stage being skipped or
    /// reordered, and a device left mid-command answers the NEXT command with the previous one's data.
    /// Repair a wedged bulk endpoint pair.
    ///
    /// A transfer that times out does not leave the device merely late - it leaves the endpoint HALTED
    /// and its transfer ring desynchronised, and re-issuing the same command can never clear that. This
    /// is why 32 seconds of retries produced 106 identical timeouts: every attempt after the first was
    /// asking a stopped endpoint to run.
    ///
    /// Two layers have to be repaired, and BOTH are required:
    ///
    /// 1. **xHCI**: `Reset Endpoint` takes the endpoint out of Halted, and `Set TR Dequeue Pointer`
    ///    tells the controller where to resume - without which it keeps the stale dequeue pointer and
    ///    the ring stays desynchronised even though the halt is gone. This half has no equivalent in
    ///    the Pi 2's DWC2 driver, which is why that port's recovery could not simply be copied.
    /// 2. **Bulk-Only Transport**: the DEVICE also has state. The classic wedge is a CSW we never
    ///    collected, which the device waits on before it will accept any new CBW, so a controller-side
    ///    reset alone leaves it stuck. The class reset plus Clear-Feature HALT on both endpoints is what
    ///    Linux's `usb-storage` runs at exactly this point.
    ///
    /// Returns whether every step was accepted. Loud on failure: a repair that silently did nothing is
    /// indistinguishable from one that worked, right up to the next timeout.
    fn bot_recover(&mut self) -> bool {
        let Some(d) = self.disk.as_ref() else { return false };
        let (slot, in_dci, out_dci) = (d.slot, d.in_dci, d.out_dci);
        let (in_phys, out_phys) = (d.in_ring.phys, d.out_ring.phys);

        put_str(b"xhci: bulk endpoint wedged - running BOT reset recovery\r\n");

        let mut ok = true;
        for (dci, phys) in [(in_dci, in_phys), (out_dci, out_phys)] {
            // STOP first. A timed-out transfer leaves the endpoint RUNNING with the command still
            // pending, not Halted - so this, not Reset, is what actually ends it, and without it the
            // Set TR Dequeue below is illegal and the whole repair is a no-op. A context-state error
            // here means it was already stopped, which is fine.
            let st = self.command(0, TRB_STOP_ENDPOINT, (slot as u32) << 24 | dci << 16);
            match st {
                Some((1, _)) | Some((19, _)) => {}
                other => {
                    ok = false;
                    put_str(b"xhci:   stop-endpoint dci ");
                    put_hex(dci as u64);
                    put_str(b" refused, cc ");
                    put_hex(other.map(|(c, _)| c as u64).unwrap_or(0xFFFF));
                    put_str(b"\r\n");
                }
            }
            // Reset Endpoint. A "context state" completion here means it was not halted after all,
            // which is not an error - a stopped-but-unhalted endpoint is exactly what a timeout leaves.
            let rc = self.command(0, TRB_RESET_ENDPOINT, (slot as u32) << 24 | dci << 16);
            match rc {
                Some((1, _)) | Some((19, _)) => {}
                other => {
                    ok = false;
                    put_str(b"xhci:   reset-endpoint dci ");
                    put_hex(dci as u64);
                    put_str(b" refused, cc ");
                    put_hex(other.map(|(c, _)| c as u64).unwrap_or(0xFFFF));
                    put_str(b"
");
                }
            }
            // Set TR Dequeue Pointer: resume at the ring head, cycle 1, matching the rewind below.
            // Without this the controller resumes from wherever the failed transfer left it.
            let dq = self.command(phys | 1, TRB_SET_TR_DEQUEUE, (slot as u32) << 24 | dci << 16);
            if dq.map(|(c, _)| c != 1).unwrap_or(true) {
                ok = false;
                put_str(b"xhci:   set-dequeue dci ");
                put_hex(dci as u64);
                put_str(b" refused, cc ");
                put_hex(dq.map(|(c, _)| c as u64).unwrap_or(0xFFFF));
                put_str(b"
");
            }
        }

        // Rewind our own view of both rings to match what we just told the controller.
        if let Some(d) = self.disk.as_mut() {
            for r in [&mut d.in_ring, &mut d.out_ring] {
                // SAFETY: this driver's own DMA page, 4 KiB, zeroed before reuse.
                unsafe { core::ptr::write_bytes(r.va as *mut u8, 0, 4096) };
                r.enqueue = 0;
                r.cycle = 1;
                r.rearm_link();
            }
        }

        // Device-side recovery, over EP0. The scratch control ring is free here: enumeration is not
        // running (we hold the USB claim), and it is rewound on every use.
        //
        // Interface 0 is assumed. Every mass-storage device this port enumerates presents its BOT
        // interface first, and the interface number is not retained at enumeration; if a device ever
        // needs a different one, this is the line that will say so by failing loudly.
        // Repair the DISK'S OWN control ring, not the enumeration scratch.
        //
        // This used to reach for `scratch()`, which was wrong twice over: it rewound a ring shared with
        // every other device (clobbering whatever the keyboard or hub had in flight), and it re-pointed
        // a ring that this slot's EP0 context may not even have named. Now each device owns the ring its
        // context names, so the thing repaired is the thing the controller reads.
        {
            let (ep0_va, ep0_phys) = {
                let d = match self.disk.as_mut() {
                    Some(d) => d,
                    None => return false,
                };
                d.ep0.enqueue = 0;
                d.ep0.cycle = 1;
                d.ep0.rearm_link();
                // SEC-28: the rewind zeroed 4 KiB through the cache, and the controller is not coherent
                // with it. Without this the xHC still sees the OLD TRBs in RAM - several carrying
                // cycle == 1 - and runs on into them once it consumes what we push. `scratch()` does this
                // for the same operation; this path did not.
                mmu::dma_sync(d.ep0.va, 4096);
                (d.ep0.va, d.ep0.phys)
            };
            let _ = ep0_va;

            // Tell the CONTROLLER about that rewind, exactly as the bulk endpoints needed: it keeps an
            // independent dequeue pointer per endpoint context, so moving only our software view to the
            // head leaves the two reading different places and every control transfer times out.
            //
            // EP0 is DCI 1. `| 1` is the dequeue cycle state, matching `cycle = 1` above.
            //
            // STOP it first, for the same reason the bulk endpoints needed it: Set TR Dequeue Pointer
            // is only legal on a Stopped or Error endpoint, and a running EP0 refuses it with a context
            // state error - which is exactly what the last boot showed once the bulk half started
            // working. A context-state completion HERE means it was already stopped, which is fine.
            // STOP first, then RESET, then re-point - the same three steps the bulk endpoints get.
            //
            // Reset was missing here. Set TR Dequeue is legal only on a Stopped or Error endpoint, and
            // `Halted` leaves ONLY via Reset Endpoint - so if EP0 were halted, which is the one state
            // actually needing repair, Stop returned a context-state error, Set TR Dequeue was refused,
            // and the device-side half could never run. A context-state completion on either is fine:
            // it means the endpoint was already in the state that command would have produced.
            let _ = self.command(0, TRB_STOP_ENDPOINT, (slot as u32) << 24 | 1 << 16);
            let _ = self.command(0, TRB_RESET_ENDPOINT, (slot as u32) << 24 | 1 << 16);
            let dq0 = self.command(ep0_phys | 1, TRB_SET_TR_DEQUEUE, (slot as u32) << 24 | 1 << 16);
            if dq0.map(|(c, _)| c != 1).unwrap_or(true) {
                // Print the completion code: a context-state error and a parameter error mean different
                // fixes, and a message that says only "refused" costs a boot to learn which.
                ok = false; // and COUNT it - this step failing is why control transfers do not land,
                            // yet it alone was reported without failing the recovery (§26.7).
                put_str(b"xhci:   set-dequeue EP0 refused, cc ");
                put_hex(dq0.map(|(c, _)| c as u64).unwrap_or(0xFFFF));
                put_str(b" - control transfers will not land\r\n");
            }

            // Take the disk out to talk on its own ring: `control_out` needs `&mut self`, and the ring
            // lives in `self.disk`. `placeholder` exists for exactly this move, and the disk is put back
            // before returning on every path.
            let mut d = match self.disk.take() {
                Some(d) => d,
                None => return false,
            };
            let mut ep0 = core::mem::replace(&mut d.ep0, Ring::null());
            self.disk = Some(d);

            // Bulk-Only Mass Storage Reset: class request to the interface, no data stage.
            if !self.control_out(slot, &mut ep0, 0x21, 0xFF, 0, 0) {
                ok = false;
                put_str(b"xhci:   BOT class reset (EP0) refused\r\n");
            }
            // Clear the HALT feature on each bulk endpoint, by ADDRESS (dci = 2*ep + in), not by dci.
            for (dci, is_in) in [(in_dci, true), (out_dci, false)] {
                let ep_num = dci / 2;
                let addr = if is_in { 0x80 | ep_num } else { ep_num };
                if !self.control_out(slot, &mut ep0, 0x02, 0x01, 0, addr as u16) {
                    ok = false;
                    put_str(b"xhci:   clear-halt ep ");
                    put_hex(addr as u64);
                    put_str(b" refused\r\n");
                }
            }
            // Give the ring back. The disk keeps the ring its EP0 context names, for the next command
            // and the next recovery.
            if let Some(d) = self.disk.as_mut() {
                d.ep0 = ep0;
            }
        }

        if !ok {
            put_str(b"xhci: BOT reset recovery did NOT complete - the device may stay wedged\r\n");
        }
        ok
    }

    fn bot(&mut self, cmd: &[u8], data_len: u32, data_in: bool) -> Option<u8> {
        let mut disk = self.disk.take()?;
        let ok = self.bot_inner(&mut disk, cmd, data_len, data_in);
        self.disk = Some(disk);
        ok
    }

    fn bot_inner(&mut self, d: &mut Disk, cmd: &[u8], data_len: u32, data_in: bool) -> Option<u8> {
        let tag = d.tag;
        d.tag = d.tag.wrapping_add(1);

        // Command Block Wrapper: 31 bytes, little-endian, signature 'USBC'.
        // SAFETY: the command page this module owns.
        unsafe {
            let p = d.cmd_va as *mut u8;
            core::ptr::write_bytes(p, 0, 31);
            let sig: [u8; 4] = [0x55, 0x53, 0x42, 0x43];
            core::ptr::copy_nonoverlapping(sig.as_ptr(), p, 4);
            core::ptr::copy_nonoverlapping(tag.to_le_bytes().as_ptr(), p.add(4), 4);
            core::ptr::copy_nonoverlapping(data_len.to_le_bytes().as_ptr(), p.add(8), 4);
            p.add(12).write_volatile(if data_in { 0x80 } else { 0x00 });
            p.add(13).write_volatile(0); // LUN 0
            p.add(14).write_volatile(cmd.len() as u8);
            core::ptr::copy_nonoverlapping(cmd.as_ptr(), p.add(15), cmd.len().min(16));
        }
        mmu::dma_sync(d.cmd_va, 32);

        let (slot, out_dci, in_dci) = (d.slot, d.out_dci, d.in_dci);
        let at = d.out_ring.push(d.cmd_phys, 31, TRB_NORMAL, TRB_IOC);
        self.doorbell(slot, out_dci);
        if !self.await_transfer(at) {
            return None;
        }

        if data_len > 0 {
            let ring = if data_in { &mut d.in_ring } else { &mut d.out_ring };
            let at = ring.push(d.data_phys, data_len, TRB_NORMAL, TRB_IOC);
            self.doorbell(slot, if data_in { in_dci } else { out_dci });
            if !self.await_transfer(at) {
                return None;
            }
            if data_in {
                mmu::dma_sync(d.data_va, data_len as usize);
            }
        }

        // Command Status Wrapper: 13 bytes. Its tag must match the CBW's, or we are looking at the
        // answer to an earlier command - which is exactly what a device does after a stall.
        let at = d.in_ring.push(d.cmd_phys + 64, 13, TRB_NORMAL, TRB_IOC);
        self.doorbell(slot, in_dci);
        if !self.await_transfer(at) {
            return None;
        }
        mmu::dma_sync(d.cmd_va + 64, 16);
        // SAFETY: the command page this module owns, just synced.
        let (csw_tag, status) = unsafe {
            let p = (d.cmd_va + 64) as *const u8;
            let mut t = [0u8; 4];
            core::ptr::copy_nonoverlapping(p.add(4), t.as_mut_ptr(), 4);
            (u32::from_le_bytes(t), p.add(12).read_volatile())
        };
        if csw_tag != tag {
            return None; // a reply to something else; treat as failure rather than as this command's
        }
        Some(status)
    }

    fn scsi_test_unit_ready(&mut self, d: &mut Disk) -> bool {
        let cmd = [0u8; 6];
        let mut disk = core::mem::replace(d, Disk::placeholder());
        let r = self.bot_inner(&mut disk, &cmd, 0, true);
        *d = disk;
        r == Some(0)
    }

    fn scsi_read_capacity(&mut self, d: &mut Disk) -> Option<u64> {
        let cmd = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0]; // READ CAPACITY(10)
        let mut disk = core::mem::replace(d, Disk::placeholder());
        let r = self.bot_inner(&mut disk, &cmd, 8, true);
        let data_va = disk.data_va;
        *d = disk;
        if r != Some(0) {
            return None;
        }
        // SAFETY: the data page this module owns, synced by `bot_inner`.
        let (last_lba, blen) = unsafe {
            let p = data_va as *const u8;
            let mut a = [0u8; 4];
            let mut b = [0u8; 4];
            core::ptr::copy_nonoverlapping(p, a.as_mut_ptr(), 4);
            core::ptr::copy_nonoverlapping(p.add(4), b.as_mut_ptr(), 4);
            (u32::from_be_bytes(a), u32::from_be_bytes(b))
        };
        // SCSI reports the LAST addressable block, not the count. Off by one here is a filesystem that
        // believes in a sector the device does not have.
        if blen != 512 {
            put_str(b"xhci: USB disk block size is not 512 - unsupported\r\n");
            return None;
        }
        Some(last_lba as u64 + 1)
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
        while waited < self.xfer_ms {
            while let Some(ev) = self.next_event() {
                if (ev.control >> 10) & 0x3F == TRB_EV_TRANSFER && ev.param == at {
                    let code = (ev.status >> 24) & 0xFF;
                    // 1 = success, 13 = short packet, which for a descriptor read is the normal case.
                    return code == 1 || code == 13;
                }
                self.absorb_foreign_event(&ev);
            }
            delay_us(1000);
            waited += 1;
        }
        put_str(b"xhci: transfer timed out\r\n");
        false
    }

    /// Deal with an event that arrived while waiting for a different one.
    ///
    /// **Discarding it silently kills the keyboard.** There is one event ring for the whole controller,
    /// so a disk transfer waiting for its own completion also receives the keyboard's. Dropping that
    /// leaves `pending` set forever: `arm_transfer` never queues another interrupt transfer, no further
    /// report ever arrives, and the keyboard is dead until it is physically unplugged and replugged -
    /// which is exactly what happened after the first disk enumeration.
    ///
    /// The event is matched against the keyboard's transfer ring by address, so this cannot mistake an
    /// unrelated completion for the keyboard's and re-arm a transfer that is still outstanding.
    fn absorb_foreign_event(&mut self, ev: &Trb) {
        if (ev.control >> 10) & 0x3F != TRB_EV_TRANSFER {
            return;
        }
        if let Some(kbd) = self.kbd.as_mut() {
            if ev.param >= kbd.ring.phys && ev.param < kbd.ring.phys + 4096 {
                // The report itself is not decoded here - this runs inside a disk transfer, and a
                // keystroke that arrived during a block read can wait for the next tick. What matters
                // is that the endpoint is marked free so the next poll re-arms it.
                kbd.pending = false;
            }
        }
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
/// How many hubs deep enumeration will walk. The Pi 4's USB-A sockets are behind an internal hub, so
/// tier 1 is the ordinary case; a hub plugged into that is tier 2 and is refused.
const HUB_TIER_MAX: u8 = 1;

#[derive(Clone, Copy)]
struct Topology {
    /// How many hubs deep this device sits. 0 = on a root port, 1 = behind one hub.
    ///
    /// Exists to BOUND the mutually-recursive walk (`walk_hub` -> `bring_up_hub_port` -> `setup_at` ->
    /// `walk_hub`), whose depth is otherwise chosen by a device-supplied `bDeviceClass` and spent on
    /// the kernel stack.
    tier: u8,
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

/// What a mass-storage interface needs: config value, interface number, and its two bulk endpoints.
#[derive(Clone, Copy)]
struct MscInfo {
    cfg_value: u8,
    iface: u8,
    in_ep: u8,
    out_ep: u8,
    mps: u32,
}

/// Find a **Bulk-Only Transport, SCSI transparent** mass-storage interface and its two bulk endpoints.
///
/// Class 8 / subclass 6 / protocol 0x50 is the combination essentially every USB stick presents, and
/// the only one this driver speaks. Anything else - a UAS-only device, or CBI - is left alone rather
/// than half-driven, because a storage device driven by a protocol it does not implement is a device
/// that returns plausible garbage where a filesystem expects blocks.
fn parse_msc(va: u64) -> Option<MscInfo> {
    // SAFETY: the descriptor buffer this module owns, filled by the control transfer above.
    let buf = unsafe { core::slice::from_raw_parts(va as *const u8, 256) };
    let total = (buf[2] as usize) | ((buf[3] as usize) << 8);
    let end = total.min(256);
    let cfg_value = buf[5];

    let mut i = 9usize;
    let mut in_msc = false;
    let mut iface = 0u8;
    let (mut in_ep, mut out_ep, mut mps) = (0u8, 0u8, 0u32);
    while i + 2 <= end {
        let len = buf[i] as usize;
        let ty = buf[i + 1];
        if len < 2 || i + len > end {
            break; // a malformed descriptor ends the walk; it never spins it
        }
        if ty == 0x04 && len >= 9 {
            iface = buf[i + 2];
            in_msc = buf[i + 5] == 0x08 && buf[i + 6] == 0x06 && buf[i + 7] == 0x50;
            if !in_msc {
                // A different interface's endpoints must not be adopted by the previous one.
                in_ep = 0;
                out_ep = 0;
            }
        } else if ty == 0x05 && len >= 7 && in_msc {
            let addr = buf[i + 2];
            // attrs bits 1:0 == 2 is Bulk. An interrupt endpoint on the same interface is not a data
            // pipe and taking it would send commands somewhere they are never read.
            if buf[i + 3] & 0x03 == 0x02 {
                mps = ((buf[i + 4] as u32) | ((buf[i + 5] as u32) << 8)) & 0x7FF;
                if addr & 0x80 != 0 {
                    in_ep = addr;
                } else {
                    out_ep = addr;
                }
            }
        }
        i += len;
    }
    if in_msc && in_ep != 0 && out_ep != 0 && mps != 0 {
        Some(MscInfo { cfg_value, iface, in_ep, out_ep, mps })
    } else {
        None
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

/// Watch the hub's ports for a device arriving or leaving, at most once a second.
///
/// **This is the whole reason a keyboard can be unplugged and plugged back in.** On this board every
/// device is behind the internal hub, so without a watcher the machine enumerates once at boot and
/// anything moved afterwards is gone until reboot.
///
/// Two things it does not do, both deliberately: it does not act on a change it could not acknowledge
/// (an event whose latch will not clear is one that would be re-reported forever, standing a working
/// device down once a second), and it does not retry an arrival unboundedly - a device that will not
/// enumerate gets [`HOTPLUG_MAX_TRIES`] attempts and is then left alone until the port changes again.
///
/// Every transition is announced. A keyboard that silently stops working is indistinguishable from a
/// broken driver, and the operator is the one holding the cable.
pub fn hotplug_poll() {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    // Cheap rate-limit BEFORE claiming, so an idle machine does not take and release the claim a
    // hundred times a second just to decide it has nothing to do.
    let hz = super::timer::frequency().max(1);
    let now = super::read_cycle_counter();
    let last = HOTPLUG_LAST.load(Ordering::Relaxed);
    if last != 0 && now.wrapping_sub(last) < hz {
        return;
    }
    if !claim_usb() {
        return; // the tick or a disk transfer has it; a second from now is fine
    }
    // SAFETY: the claim above makes this the only context touching the controller.
    if let Some(hc) = unsafe { (&raw mut XHCI).as_mut().and_then(|o| o.as_mut()) } {
        hotplug_tick(hc);
    }
    release_usb();
}

fn hotplug_tick(hc: &mut Xhci) {
    let Some(mut hub) = hc.hub.take() else { return };

    let hz = super::timer::frequency().max(1);
    let now = super::read_cycle_counter();
    let last = HOTPLUG_LAST.load(Ordering::Relaxed);
    if last != 0 && now.wrapping_sub(last) < hz {
        hc.hub = Some(hub);
        return;
    }
    HOTPLUG_LAST.store(now.max(1), Ordering::Relaxed);

    // Short bound for the status reads: see `Xhci::xfer_ms`. Restored before returning, so the
    // enumeration this visit may trigger still gets the generous one it needs.
    hc.xfer_ms = 50;

    let topo = Topology { tier: 0, root_port: 1, route: 0, tt_slot: 0, tt_port: 0, speed: 0 };
    let mut announced = false;
    for p in 1..=hub.nports.min(15) {
        let bit = 1u16 << p;
        let (slot, bp, bv) = (hub.slot, hub.buf_phys, hub.buf_va);
        if !hc.control_in(
            slot, &mut hub.ep0, HUB_GET_PORT_STATUS.0, HUB_GET_PORT_STATUS.1, 0, p as u16, 4, bp, bv,
        ) {
            continue; // could not ask; try again next visit
        }
        // SAFETY: the 4-byte port status this module owns, just filled.
        let (status, change) = unsafe {
            ((bv as *const u16).read_volatile(), (bv as *const u16).add(1).read_volatile())
        };
        let now_conn = status & PS_CONNECTION != 0;
        let mut was = hub.connected & bit != 0;

        // An all-zero answer on a port we believed OCCUPIED is a read that did not happen, not a
        // device that left.
        //
        // The buffer is zeroed before the transfer so a short read cannot be mistaken for old data,
        // and `await_transfer` succeeding does not promise all four bytes arrived. A completion that
        // delivers nothing therefore leaves exactly "not connected, nothing changed" - which is a
        // perfectly good answer for an empty port and a fabricated disconnect for an occupied one.
        //
        // What separates them is the latch: a real removal ALWAYS sets C_CONNECTION, because the
        // connection state changed. Zero status with zero change is the signature of a lost reply.
        // Believing it tore the device down and re-enumerated it seconds later, which is what put
        // "device REMOVED / device ATTACHED" in the log of a machine nobody was touching.
        if was && status == 0 && change == 0 {
            continue; // ask again next visit
        }

        if change & 1 != 0 {
            // Clear the latch, and CHECK it cleared before acting. An event we cannot acknowledge is
            // one we cannot safely consume - it would be re-reported on every visit, and a still-
            // occupied port would be torn down and re-enumerated once a second, forever.
            if !hc.control_out(
                slot, &mut hub.ep0, HUB_CLEAR_PORT_FEATURE.0, HUB_CLEAR_PORT_FEATURE.1,
                PORT_FEAT_C_CONNECTION, p as u16,
            ) {
                continue;
            }
            // A latched change on a port that is STILL occupied is the case levels cannot see: unplugged
            // and replugged, or swapped, between two visits. What we believed is no longer true.
            if was && now_conn {
                stand_down(hc, &mut hub, p);
                was = false;
                hub.connected &= !bit;
                hub.tries[p as usize] = 0;
            }
        } else if now_conn == was {
            continue; // no latch, no level change
        }

        if was && !now_conn {
            super::console_notice_fmt(format_args!("usb: device REMOVED from hub port {}", p));
            announced = true;
            stand_down(hc, &mut hub, p);
            hub.connected &= !bit;
            hub.tries[p as usize] = 0; // a fresh set of attempts for whatever arrives next
        } else if !was && now_conn {
            if hub.tries[p as usize] >= HOTPLUG_MAX_TRIES {
                continue; // said its piece already; wait for the port to change again
            }
            hub.tries[p as usize] += 1;
            super::console_notice_fmt(format_args!("usb: device ATTACHED to hub port {} - enumerating", p));
            announced = true;
            hc.xfer_ms = 500; // enumeration is allowed to be slow
            let ok = hc.bring_up_hub_port(&mut hub, topo, p);
            hc.xfer_ms = 50;
            if ok {
                hub.connected |= bit;
                hub.tries[p as usize] = 0;
                let what: &[u8] = if hc.disk.as_ref().is_some_and(|d| d.hub_port == p) {
                    b"disk"
                } else {
                    b"keyboard"
                };
                super::console_notice_fmt(format_args!(
                    "usb: {} on hub port {} is ready",
                    core::str::from_utf8(what).unwrap_or("device"), p));
            } else if hub.tries[p as usize] >= HOTPLUG_MAX_TRIES {
                crate::kprintln!(
                    "usb: hub port {} device did not come up in {} attempts - leaving it until the \
                     port changes again",
                    p, HOTPLUG_MAX_TRIES
                );
                // Recorded as occupied even though it failed: otherwise every visit re-tries the
                // arrival branch and the bound above never holds.
                hub.connected |= bit;
            }
        }
    }
    hc.xfer_ms = 500;
    hc.hub = Some(hub);

    // Give the operator a PROMPT back. Every announcement above scrolls `gsh>` away, and the shell is
    // blocked in `console_read` with no reason to redraw - so a working machine presents as a dead one,
    // which is exactly what invariant 12 is about: what the OPERATOR can see. One newline makes the
    // shell finish an empty line and print a fresh prompt; an empty command runs nothing.
    //
    // This grants no authority that was not already there. A keyboard driver can synthesize keystrokes
    // by definition (the SEC-2 residual, CLAUDE.md §6.4) - which is precisely why it may synthesize the
    // one that says "you may type now".
    if announced {
        super::console_push_byte(b'\n');
    }
}

/// Forget the device on a hub port, and give the controller its slot back.
///
/// Releasing the slot matters: a slot is a bounded resource (32 on this controller), and an
/// unplug/replug cycle that leaked one would run the machine out after 30 cable pulls. The keyboard's
/// auto-repeat is disarmed too - an unplugged key is not being held down, and our view of which keys
/// are down is now stale.
fn stand_down(hc: &mut Xhci, hub: &mut Hub, p: u8) {
    let _ = hub;

    // The DISK, if that is what left. Publishing 0 sectors is the load-bearing part: `fs` and the block
    // driver ask the kernel how big the disk is, and a stale non-zero answer means every later read is
    // aimed at a device that is no longer there - which fails slowly, one bounded timeout at a time,
    // instead of saying "no disk".
    if hc.disk.as_ref().is_some_and(|d| d.hub_port == p) {
        let slot = hc.disk.as_ref().map(|d| d.slot).unwrap_or(0);
        hc.disk = None;
        DISK_SECTORS.store(0, Ordering::Release);
        hc.command(0, TRB_DISABLE_SLOT, (slot as u32) << 24);
        super::console_notice_fmt(format_args!(
            "usb: disk on hub port {} stood down, slot {} released", p, slot));
        return;
    }

    let Some(k) = hc.kbd.as_ref() else { return };
    if k.hub_port != p {
        return;
    }
    let slot = k.slot;
    hc.kbd = None;
    // SAFETY: single caller, from the timer tick on core 0. See `poll`.
    let ks = unsafe { &mut *core::ptr::addr_of_mut!(KBD_STATE) };
    ks.rep.disarm();
    ks.last = [0; 6];
    hc.command(0, TRB_DISABLE_SLOT, (slot as u32) << 24);
    super::console_notice_fmt(format_args!("usb: keyboard on hub port {} stood down, slot {} released", p, slot));
    // ACTIVE is deliberately NOT cleared: the hub is still there, and the watcher that will notice the
    // keyboard coming back is the same poll this runs inside.
}

// --- The block interface the `block-driver` service reaches through syscalls ---------------------
//
// These run in TASK context, not the tick. `DISK_IO` is what keeps them from racing the keyboard poll
// for the event ring - see its comment.

/// How many 512-byte sectors the attached USB disk has. 0 = no disk.
pub fn disk_sectors() -> u64 {
    DISK_SECTORS.load(Ordering::Acquire)
}

/// Whether the last transfer failed in a way that is worth retrying (device busy), rather than
/// permanently. Distinguishing them is what lets the block driver retry instead of failing a mount.
static DISK_BUSY: AtomicBool = AtomicBool::new(false);

/// Consecutive transfer timeouts with no success in between.
///
/// Recovery is keyed on a RUN, not on a single timeout: one timeout can be a slow device, and resetting
/// its endpoints mid-operation would be the cure causing the disease. A handful in a row cannot be
/// anything but a halted endpoint. Reset by any success, so a merely-slow device never triggers it.
static TIMEOUT_RUN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// How long a device may go without answering before we conclude it is stuck rather than working.
///
/// This was 4, and 4 was actively harmful. At the 500 ms transfer budget that is TWO SECONDS, after
/// which recovery issues Stop Endpoint - which ABORTS the command the device is still working on. A
/// stick doing a block remap or garbage collection takes seconds, so it was being interrupted every
/// time, and then needed ~30 s to settle from the interruption. That is the whole shape of the last
/// run: 62 wedges, recovery completing 62 times because OUR commands were fine, and the device still
/// unresponsive afterwards because we kept killing its work.
///
/// Recovery is surgery, not a retry. It must fire only when the device is genuinely stuck - never
/// because it is merely slow. Linux's `usb-storage` waits 30 seconds before resetting, and the Pi 2
/// driver reaches the same conclusion in its own words: "no device is occupied for 30 seconds; a device
/// that never once pauses is not busy, it is stuck".
///
/// 60 timeouts at 500 ms is 30 s of unbroken silence, matched to that and to the caller's own budget.
/// Any success resets the run, so a device that pauses even once is never touched.
const TIMEOUT_RUN_MAX: u32 = 60;

pub fn disk_busy() -> bool {
    DISK_BUSY.load(Ordering::Relaxed)
}

/// Borrow the controller for a disk operation, with the event ring held against the tick.
fn with_disk<R>(f: impl FnOnce(&mut Xhci) -> R, absent: R, busy: R) -> R {
    if DISK_SECTORS.load(Ordering::Acquire) == 0 {
        return absent;
    }
    if !claim_usb() {
        // Someone is enumerating or sampling the keyboard. BUSY is not a failure - the block driver
        // retries it - and it is the one answer that does not corrupt the other holder's transfer.
        DISK_BUSY.store(true, Ordering::Relaxed);
        return busy;
    }
    // SAFETY: the claim above makes this the only context touching the controller.
    let r = unsafe {
        match (&raw mut XHCI).as_mut().and_then(|o| o.as_mut()) {
            Some(hc) => f(hc),
            None => {
                release_usb();
                return absent;
            }
        }
    };
    release_usb();
    r
}

/// The highest block `READ(10)`/`WRITE(10)` can name: their LBA field is 32 bits.
///
/// The block protocol carries a **u64** LBA on purpose (`fs` sizes capacity in u64), so a cast down to
/// `u32` is not a formality - it silently WRAPS. Block `0x1_0000_0000` would become block 0, which for
/// a write means the superblock is overwritten by whatever was meant for the far end of the disk, with
/// every layer reporting success. The device in hand is 31M sectors so it cannot reach this, and that
/// is exactly why it has to be refused rather than left to a bigger stick to discover.
const MAX_LBA_10: u64 = u32::MAX as u64;

/// Read one 512-byte block.
pub fn disk_read(lba: u64, dst: &mut [u8]) -> bool {
    if dst.len() < 512 {
        return false;
    }
    if lba > MAX_LBA_10 {
        put_str(b"xhci: block address is beyond what READ(10) can address - refusing\r\n");
        return false;
    }
    with_disk(
        |hc| {
            // READ(10): opcode, flags, 4-byte big-endian LBA, reserved, 2-byte block count.
            let b = (lba as u32).to_be_bytes();
            let cmd = [0x28u8, 0, b[0], b[1], b[2], b[3], 0, 0, 1, 0];
            match hc.bot(&cmd, 512, true) {
                Some(0) => {
                    let Some(d) = hc.disk.as_ref() else { return false };
                    // SAFETY: the data page this module owns, synced by `bot`.
                    unsafe {
                        core::ptr::copy_nonoverlapping(d.data_va as *const u8, dst.as_mut_ptr(), 512)
                    };
                    DISK_BUSY.store(false, Ordering::Relaxed);
                    TIMEOUT_RUN.store(0, Ordering::Relaxed); // one success ends the run
                    true
                }
                // A non-zero status is the device refusing THIS command - often "not ready" while it
                // finishes an internal operation. Recorded as busy so the caller retries rather than
                // concluding the disk is gone.
                Some(_) => {
                    DISK_BUSY.store(true, Ordering::Relaxed);
                    false
                }
                // The transfer did not complete in its window. That is NOT a failed device: the disk
                // enumerated, it answered until now, and absence is a separate question already asked
                // first by the syscall. A stick doing a block remap or garbage collection simply takes
                // longer than one command budget, and "come back" is the honest answer.
                //
                // Recording this as a hard failure is what broke the mount path: `block-driver` keys its
                // entire retry-with-backoff on BUSY, so a timeout skipped the wait completely and failed
                // instantly, `fs` re-mounted, the re-mount read LBA 0, timed out again, and cascaded -
                // 19 identical failures per run and not one `gave up after` line, because the wait was
                // never entered at all.
                //
                // The 30-second duration budget in block-driver is what decides when to actually give
                // up, and it announces at 2s, so a genuinely stuck device is still bounded and loud.
                None => {
                    DISK_BUSY.store(true, Ordering::Relaxed);
                    // A RUN of timeouts unbroken by a single success is what tells stuck from slow -
                    // duration is the only thing that distinguishes them, exactly as the Pi 2 port
                    // records. A device that is merely working pauses and then answers; one that never
                    // answers has a halted endpoint, and no number of retries will clear it.
                    if TIMEOUT_RUN.fetch_add(1, Ordering::Relaxed) + 1 >= TIMEOUT_RUN_MAX {
                        TIMEOUT_RUN.store(0, Ordering::Relaxed);
                        hc.bot_recover();
                    }
                    false
                }
            }
        },
        false,
        false,
    )
}

/// Write one 512-byte block.
pub fn disk_write(lba: u64, src: &[u8]) -> bool {
    if src.len() < 512 {
        return false;
    }
    // See `MAX_LBA_10`. A wrapped WRITE is the worse half: it corrupts a block nobody asked about.
    if lba > MAX_LBA_10 {
        put_str(b"xhci: block address is beyond what WRITE(10) can address - refusing
");
        return false;
    }
    with_disk(
        |hc| {
            {
                let Some(d) = hc.disk.as_ref() else { return false };
                // SAFETY: the data page this module owns; the caller's slice is at least 512 bytes.
                unsafe {
                    core::ptr::copy_nonoverlapping(src.as_ptr(), d.data_va as *mut u8, 512)
                };
                mmu::dma_sync(d.data_va, 512);
            }
            let b = (lba as u32).to_be_bytes();
            let cmd = [0x2Au8, 0, b[0], b[1], b[2], b[3], 0, 0, 1, 0]; // WRITE(10)
            match hc.bot(&cmd, 512, false) {
                Some(0) => {
                    DISK_BUSY.store(false, Ordering::Relaxed);
                    TIMEOUT_RUN.store(0, Ordering::Relaxed); // one success ends the run
                    true
                }
                Some(_) => {
                    DISK_BUSY.store(true, Ordering::Relaxed);
                    false
                }
                // The transfer did not complete in its window. That is NOT a failed device: the disk
                // enumerated, it answered until now, and absence is a separate question already asked
                // first by the syscall. A stick doing a block remap or garbage collection simply takes
                // longer than one command budget, and "come back" is the honest answer.
                //
                // Recording this as a hard failure is what broke the mount path: `block-driver` keys its
                // entire retry-with-backoff on BUSY, so a timeout skipped the wait completely and failed
                // instantly, `fs` re-mounted, the re-mount read LBA 0, timed out again, and cascaded -
                // 19 identical failures per run and not one `gave up after` line, because the wait was
                // never entered at all.
                //
                // The 30-second duration budget in block-driver is what decides when to actually give
                // up, and it announces at 2s, so a genuinely stuck device is still bounded and loud.
                None => {
                    DISK_BUSY.store(true, Ordering::Relaxed);
                    // A RUN of timeouts unbroken by a single success is what tells stuck from slow -
                    // duration is the only thing that distinguishes them, exactly as the Pi 2 port
                    // records. A device that is merely working pauses and then answers; one that never
                    // answers has a halted endpoint, and no number of retries will clear it.
                    if TIMEOUT_RUN.fetch_add(1, Ordering::Relaxed) + 1 >= TIMEOUT_RUN_MAX {
                        TIMEOUT_RUN.store(0, Ordering::Relaxed);
                        hc.bot_recover();
                    }
                    false
                }
            }
        },
        false,
        false,
    )
}

/// Make previously acknowledged writes durable.
///
/// A write completing means the stick took the bytes, not that they are on flash - it acknowledges
/// into its own volatile buffer. `fs` asks for durability explicitly at the points where it promises
/// it (format, journal commit), and a device that refuses `SYNCHRONIZE CACHE` must say so rather than
/// let the filesystem imply a guarantee it does not have (§6.1, 2026-07-25 amendment).
pub fn disk_flush() -> bool {
    with_disk(
        |hc| {
            let cmd = [0x35u8, 0, 0, 0, 0, 0, 0, 0, 0, 0]; // SYNCHRONIZE CACHE(10)
            matches!(hc.bot(&cmd, 0, false), Some(0))
        },
        false,
        false,
    )
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
    // Someone else owns the controller (a disk transfer, or an enumeration in the idle loop). Skipping
    // costs one keyboard sample - 10 ms of typing latency - and taking it anyway would eat their
    // completion event. See `USB_CLAIM`.
    if !claim_usb() {
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
    // BOUNDED per visit (§26.6). This runs on the TIMER TICK, and the loop ends only when the
    // controller stops producing events - so a device or controller generating them faster than we
    // consume holds core 0 inside the tick and nothing else on that core runs again. The arm32 audit
    // already recorded this exact shape as a HIGH finding (`net_rx_isr` IRQ-storm livelock); it is the
    // same defect on a different device.
    //
    // A tick's worth is a tick's worth: whatever is left stays queued and the next tick takes it, which
    // is the behaviour a ring exists to provide. 64 is far more than a keyboard can generate in 10 ms
    // and far less than a storm needs to wedge the core.
    const EVENTS_PER_VISIT: u32 = 64;
    let mut drained = 0u32;
    while let Some(ev) = hc.next_event() {
        drained += 1;
        if drained > EVENTS_PER_VISIT {
            break;
        }
        if (ev.control >> 10) & 0x3F != TRB_EV_TRANSFER {
            continue;
        }
        let code = (ev.status >> 24) & 0xFF;
        if let Some(kbd) = hc.kbd.as_mut() {
            // Is this event actually the KEYBOARD'S? The event ring is shared by every endpoint, and
            // this loop used to treat any transfer completion as a report: it cleared `pending` and
            // decoded 8 bytes of the buffer straight into console keystrokes.
            //
            // Orphaned completions are routine, not hypothetical - every `await_transfer` timeout
            // leaves a TRB in flight whose completion lands later, and `TIMEOUT_RUN_MAX = 60` presumes
            // sixty of them. Two consequences, both live: `pending` cleared while the real transfer is
            // still outstanding queues a second TRB into one 8-byte buffer and desynchronises the count
            // permanently; and whatever bytes happen to be in that buffer get decoded as HID usages and
            // INJECTED INTO THE CONSOLE - a `CONSOLE_PUSH` path, inside the shell's trust perimeter
            // (§6.4 SEC-2). `absorb_foreign_event` does this check and documents it as load-bearing;
            // this path simply did not.
            if ev.param < kbd.ring.phys || ev.param >= kbd.ring.phys + 4096 {
                continue; // somebody else's completion
            }
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
    release_usb();
}

/// Microseconds since boot, from the generic timer.
///
/// Truncated to 32 bits on purpose: the repeat logic compares wrap-safely and only ever measures
/// sub-second gaps, so the wrap every ~71 minutes is invisible to it.
fn now_us() -> u32 {
    let hz = super::timer::frequency().max(1);
    ((super::read_cycle_counter() / (hz / 1_000_000).max(1)) & 0xFFFF_FFFF) as u32
}
