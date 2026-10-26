// SPDX-License-Identifier: GPL-2.0-only
//! The PLIC - how a wired device interrupt reaches this kernel, and through it a driver service.
//!
//! Until 2026-10-26 this port had no interrupt controller at all: no device interrupt reached the
//! kernel, every driver polled, and a driver package that asked for its PCI device's interrupt
//! (`hw_pci_irq`) was given none. The neutral spawn path asks the arch to "program a vector" for such a
//! device (`pci_msi_vector` in `task/mod.rs`), and this port has no MSI controller (AIA/IMSIC) to program,
//! so it answers the same question the way RISC-V machines without one wire PCI: the device's INTx pin,
//! through the host bridge's `interrupt-map`, to a PLIC source. The VECTOR is the kernel's name for that
//! route - one of `POOL_LEN` numbers above `POOL_BASE` - so `interrupt::route` delivers it exactly as it
//! delivers an MSI on x86, and nothing above the seam learns that a wire is underneath.
//!
//! **What it routes, and what it does not.** PCI INTx, for a device on the root bus whose pin the
//! bridge's map names. Not a device behind a bridge (its pin would need swizzling, which this does not
//! do and says so), not the SoC's own peripherals (none of their drivers asks), and not MSI.
//!
//! **Everything the PLIC needs is read from the device tree**: its base, its source count, and which of
//! its contexts is which hart's SUPERVISOR context. That last is the one that differs between the two
//! machines: on QEMU `virt` the contexts alternate M and S for harts 0..N, and on the JH7110 hart 0 (the
//! S7 monitor core) has an M context only, so every later index is shifted by one. Assuming QEMU's layout
//! would claim from the wrong hart's context on the board.
//!
//! **Level-triggered, so masked between delivery and the driver's ack.** INTx stays asserted until the
//! driver clears the cause in its device, so the source is DISABLED in its context when it is delivered
//! (`mask_vector`, called by `interrupt::route::deliver`) and enabled again by the driver's `IrqUnmask`
//! (`unmask_vector`). The claim is completed at once, which is safe precisely because the source is off
//! until the driver says so.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use super::fdt::Fdt;
use crate::smp::SpinLock;

/// The vectors this port hands out for PCI INTx routes, `POOL_BASE..POOL_BASE + POOL_LEN`. Above the
/// fixed xHCI/EHCI numbers (0x28, 0x29) so neither can be confused with a route made here.
pub const POOL_BASE: u8 = 0x30;
pub const POOL_LEN: usize = 8;

/// How many harts this file can name a context for - the same bound as `HART_TO_CORE`.
const MAX_HART: usize = 8;
/// A source number no PLIC uses: "none".
const NONE: u32 = u32::MAX;

static BASE: AtomicUsize = AtomicUsize::new(0);
/// Sources the PLIC implements (`riscv,ndev`), so the enables cleared at start are exactly its own.
static NDEV: AtomicU32 = AtomicU32::new(0);
/// Each hart's SUPERVISOR context, from `interrupts-extended`. `NONE` for a hart with none.
static S_CTX: [AtomicU32; MAX_HART] = [const { AtomicU32::new(NONE) }; MAX_HART];

/// Each pool vector's route: the PLIC source and the context it is enabled in. `NONE` = not routed.
static SLOT_SOURCE: [AtomicU32; POOL_LEN] = [const { AtomicU32::new(NONE) }; POOL_LEN];
static SLOT_CTX: [AtomicU32; POOL_LEN] = [const { AtomicU32::new(NONE) }; POOL_LEN];

/// The PCI host bridge's `interrupt-map`, kept as (unit-address high cell, pin, PLIC source), with the
/// `interrupt-map-mask` applied when looking up. QEMU `virt` has 16 entries (4 slots x 4 pins).
const MAP_MAX: usize = 32;
static MAP_HI: [AtomicU32; MAP_MAX] = [const { AtomicU32::new(0) }; MAP_MAX];
static MAP_PIN: [AtomicU32; MAP_MAX] = [const { AtomicU32::new(0) }; MAP_MAX];
static MAP_SRC: [AtomicU32; MAP_MAX] = [const { AtomicU32::new(0) }; MAP_MAX];
static MAP_LEN: AtomicUsize = AtomicUsize::new(0);
static MASK_HI: AtomicU32 = AtomicU32::new(0);
static MASK_PIN: AtomicU32 = AtomicU32::new(0);

/// Enable words are read-modified-written, and a delivery on one hart (mask) can meet an `IrqUnmask` on
/// another for a source in the same word. Held with interrupts masked, because `mask_vector` runs in
/// the trap handler (the same reasoning as `interrupt::route::IRQ_TABLE`).
static ENABLE_LOCK: SpinLock<()> = SpinLock::new(());

fn rd(off: usize) -> u32 {
    let base = BASE.load(Ordering::Acquire);
    // SAFETY: `base` is the PLIC the device tree described, inside the kernel's identity map, and every
    // offset this file forms is inside the register map the RISC-V PLIC specification defines for the
    // sources and contexts that same tree declared. Reads have no side effect except `claim`, which is
    // read only by `handle`, on purpose.
    unsafe { ((base + off) as *const u32).read_volatile() }
}

fn wr(off: usize, val: u32) {
    let base = BASE.load(Ordering::Acquire);
    // SAFETY: as `rd`. Writes are the specification's own interface: a priority, an enable word, a
    // threshold, or a completion of a source this hart claimed.
    unsafe { ((base + off) as *mut u32).write_volatile(val) };
}

const fn priority(src: u32) -> usize { 4 * src as usize }
const fn enable(ctx: u32, src: u32) -> usize { 0x2000 + 0x80 * ctx as usize + 4 * (src as usize / 32) }
const fn threshold(ctx: u32) -> usize { 0x20_0000 + 0x1000 * ctx as usize }
const fn claim(ctx: u32) -> usize { 0x20_0004 + 0x1000 * ctx as usize }

fn be32(b: &[u8], i: usize) -> Option<u32> {
    let w = b.get(4 * i..4 * i + 4)?;
    Some(u32::from_be_bytes([w[0], w[1], w[2], w[3]]))
}

/// Find the PLIC, learn which context is each hart's, and read the PCI host bridge's interrupt map.
/// Called once, on the boot hart, before any hart admits an external interrupt; each hart readies its
/// own context on arrival (`this_hart`). Every outcome is printed: a machine with no PLIC routes nothing
/// and says so.
pub fn init(tree: &Fdt) {
    const COMPAT: [&str; 2] = ["riscv,plic0", "sifive,plic-1.0.0"];
    let mut want = [None];
    let Some((compat, reg)) = COMPAT.iter().find_map(|c| tree.find_compatible(c, &["riscv,ndev"], &mut want).map(|r| (*c, r))) else {
        super::print_str("riscv64: plic - none in the device tree; no device interrupt is routed, drivers poll\n");
        return;
    };
    let Some(ndev) = want[0] else {
        super::print_str("riscv64: plic - no riscv,ndev; not used, drivers poll\n");
        return;
    };
    BASE.store(reg.base as usize, Ordering::Release);
    NDEV.store(ndev, Ordering::Relaxed);

    // Context i is the i-th (phandle, cause) pair; cause 9 is supervisor external. The phandle is a
    // hart's interrupt controller, which names no hart itself - its parent `cpu@N` does.
    let mut harts = 0u32;
    if let Some(ext) = tree.find_compatible_prop(compat, "interrupts-extended") {
        let mut ctx = 0u32;
        while let (Some(ph), Some(cause)) = (be32(ext, 2 * ctx as usize), be32(ext, 2 * ctx as usize + 1)) {
            if cause == 9 {
                if let Some(h) = tree.hart_of_intc(ph).filter(|&h| (h as usize) < MAX_HART) {
                    S_CTX[h as usize].store(ctx, Ordering::Release);
                    harts += 1;
                }
            }
            ctx += 1;
        }
    }
    super::print_str("riscv64: plic ");
    super::print_hex(reg.base);
    super::print_str(", ");
    super::print_dec(ndev as u64);
    super::print_str(" sources, a supervisor context for ");
    super::print_dec(harts as u64);
    super::print_str(" hart(s)\n");

    // The PCI host bridge's map: each entry is <unit address 3 cells><pin 1><parent phandle 1>
    // <parent unit address: the PLIC's #address-cells><parent interrupt 1>.
    let Some(map) = tree.find_compatible_prop("pci-host-ecam-generic", "interrupt-map") else { return };
    let Some(mask) = tree.find_compatible_prop("pci-host-ecam-generic", "interrupt-map-mask") else {
        super::print_str("riscv64: pci interrupt-map without a mask - PCI INTx is not routed\n");
        return;
    };
    let parent_addr = tree.find_compatible_prop(compat, "#address-cells").and_then(|v| be32(v, 0)).unwrap_or(0) as usize;
    let stride = 6 + parent_addr;
    MASK_HI.store(be32(mask, 0).unwrap_or(0), Ordering::Relaxed);
    MASK_PIN.store(be32(mask, 3).unwrap_or(0), Ordering::Relaxed);
    let mut n = 0usize;
    let mut e = 0usize;
    while let (Some(hi), Some(pin), Some(src)) = (be32(map, e * stride), be32(map, e * stride + 3), be32(map, e * stride + stride - 1)) {
        if n == MAP_MAX { break; }
        MAP_HI[n].store(hi, Ordering::Relaxed);
        MAP_PIN[n].store(pin, Ordering::Relaxed);
        MAP_SRC[n].store(src, Ordering::Relaxed);
        n += 1;
        e += 1;
    }
    MAP_LEN.store(n, Ordering::Release);
    super::print_str("riscv64: pci INTx -> plic, ");
    super::print_dec(n as u64);
    super::print_str(" interrupt-map entries\n");
}

/// Ready THIS hart's supervisor context: threshold 0 and every source off. Called by each hart on
/// arrival, just before it admits external interrupts - not once by the boot hart for all of them,
/// because the firmware resets a hart's PLIC contexts when it starts that hart: OpenSBI's warm init
/// leaves the threshold at 7 and every enable clear, and a source at priority 1 then never reaches the
/// hart (measured on QEMU `virt`: hart 1's threshold read 7 after the boot hart had written 0).
/// Sources off, so nothing reaches the hart that no route here names.
pub fn this_hart() {
    if BASE.load(Ordering::Acquire) == 0 {
        return;
    }
    // SAFETY: reading this hart's own id; architecturally side-effect free (see `get_lapic_id`).
    let hart = unsafe { super::boot::get_lapic_id() } as usize;
    let Some(ctx) = S_CTX.get(hart).map(|c| c.load(Ordering::Acquire)).filter(|&c| c != NONE) else { return };
    let ndev = NDEV.load(Ordering::Relaxed);
    let mut w = 0;
    while w * 32 <= ndev {
        wr(enable(ctx, w * 32), 0);
        w += 1;
    }
    wr(threshold(ctx), 0);
}

/// The PLIC source a root-bus device's INTx `pin` (1 = INTA) reaches, from the bridge's map.
fn source_of(bdf: u32, pin: u32) -> Option<u32> {
    let hi = (bdf << 8) & MASK_HI.load(Ordering::Relaxed);
    let pin = pin & MASK_PIN.load(Ordering::Relaxed);
    (0..MAP_LEN.load(Ordering::Acquire))
        .find(|&i| MAP_HI[i].load(Ordering::Relaxed) == hi && MAP_PIN[i].load(Ordering::Relaxed) == pin)
        .map(|i| MAP_SRC[i].load(Ordering::Relaxed))
}

fn slot_of(vector: u8) -> Option<usize> {
    let s = vector.checked_sub(POOL_BASE)? as usize;
    (s < POOL_LEN).then_some(s)
}

/// Turn one source on or off in one context.
fn set_enable(ctx: u32, src: u32, on: bool) {
    let _g = ENABLE_LOCK.lock_irq();
    let off = enable(ctx, src);
    let bit = 1u32 << (src % 32);
    let w = rd(off);
    wr(off, if on { w | bit } else { w & !bit });
}

/// Route PCI device `bdf`'s INTx to pool `vector`, delivered on the hart that is `core`. Left DISABLED:
/// `interrupt::route::register` unmasks it once the driver's endpoint holds the route. Re-routing the
/// same vector (a respawn, possibly on another core) moves it. `false`, with the reason printed, when
/// there is nothing to route to.
pub fn route_intx(bdf: u32, vector: u8, core: u32) -> bool {
    let Some(slot) = slot_of(vector) else { return false };
    if BASE.load(Ordering::Acquire) == 0 {
        crate::kprintln!("pci-intx: BDF {:#06x} - no PLIC on this machine, so no interrupt", bdf);
        return false;
    }
    if bdf >> 8 != 0 {
        crate::kprintln!("pci-intx: BDF {:#06x} is behind a bridge - its pin is not swizzled here, so no interrupt", bdf);
        return false;
    }
    let Some(pin) = super::pci::intx_pin_enable(bdf) else {
        crate::kprintln!("pci-intx: BDF {:#06x} has no interrupt pin, so no interrupt", bdf);
        return false;
    };
    let Some(src) = source_of(bdf, pin as u32).filter(|&s| s != 0 && s <= NDEV.load(Ordering::Relaxed)) else {
        crate::kprintln!("pci-intx: BDF {:#06x} INT{} is not in the host bridge's interrupt-map, so no interrupt", bdf, (b'A' + pin - 1) as char);
        return false;
    };
    let hart = super::core_hart(core);
    let Some(ctx) = hart.and_then(|h| S_CTX.get(h as usize)).map(|c| c.load(Ordering::Relaxed)).filter(|&c| c != NONE) else {
        crate::kprintln!("pci-intx: BDF {:#06x} - core {} has no PLIC supervisor context, so no interrupt", bdf, core);
        return false;
    };
    // A respawn may land on another core: switch the source off where it was before routing it here.
    let was = SLOT_CTX[slot].load(Ordering::Acquire);
    if was != NONE {
        set_enable(was, SLOT_SOURCE[slot].load(Ordering::Relaxed), false);
    }
    wr(priority(src), 1);
    SLOT_SOURCE[slot].store(src, Ordering::Relaxed);
    SLOT_CTX[slot].store(ctx, Ordering::Release);
    crate::kprintln!("pci-intx: BDF {:#06x} INT{} -> plic source {} -> vector {:#04x} on hart {} (context {})",
        bdf, (b'A' + pin - 1) as char, src, vector, hart.unwrap_or(0), ctx);
    true
}

/// Mask (`on == false`) or unmask a pool vector's source. Not a pool vector, or not routed: nothing.
pub fn set_vector(vector: u8, on: bool) {
    let Some(slot) = slot_of(vector) else { return };
    let ctx = SLOT_CTX[slot].load(Ordering::Acquire);
    if ctx != NONE {
        set_enable(ctx, SLOT_SOURCE[slot].load(Ordering::Relaxed), on);
    }
}

/// A supervisor external interrupt on this hart: claim each pending source, deliver its vector, and
/// complete it. Bounded, so a source that re-asserts without end cannot hold the hart here: what is
/// left is taken on the next trap. A source no route names is switched off and said once.
pub fn handle() {
    // SAFETY: reading this hart's own id; architecturally side-effect free (see `get_lapic_id`).
    let hart = unsafe { super::boot::get_lapic_id() } as usize;
    let Some(ctx) = S_CTX.get(hart).map(|c| c.load(Ordering::Relaxed)).filter(|&c| c != NONE) else { return };
    for _ in 0..16 {
        let src = rd(claim(ctx));
        if src == 0 {
            return;
        }
        match (0..POOL_LEN).find(|&s| SLOT_SOURCE[s].load(Ordering::Relaxed) == src && SLOT_CTX[s].load(Ordering::Relaxed) == ctx) {
            // SAFETY: the trap handler, with `sstatus.SIE` clear for the whole of it - the calling
            // convention `deliver` names. It masks the source before the completion below.
            Some(s) => unsafe { crate::interrupt::route::deliver(POOL_BASE + s as u8) },
            None => {
                set_enable(ctx, src, false);
                crate::kprintln!("plic: source {} claimed on hart {} with no route - switched off", src, hart);
            }
        }
        wr(claim(ctx), src);
    }
}
