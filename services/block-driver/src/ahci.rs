//! AHCI (SATA) backend for `block-driver` (docs/ahci.md).
//!
//! A DMA + MMIO driver: the kernel maps the HBA's ABAR (MMIO) and grants a
//! physically-contiguous DMA arena at spawn (same path as the USB drivers). This
//! replaces ATA PIO on modern machines (the T630's SSD is AHCI-only).
//!
//! **Step A (this file): detection + HBA init.** Map the ABAR, enable AHCI mode,
//! enumerate implemented ports, and report which carry a SATA disk. Read/write
//! (command list + FIS + PRDT) come in the next steps.

use godspeed_sdk::{Mmio, ServiceContext};

// HBA Generic Host Control registers (offsets from ABAR).
const HBA_CAP: usize = 0x00; // host capabilities
const HBA_GHC: usize = 0x04; // global host control
const HBA_PI: usize = 0x0C;  // ports implemented (bitmask)
const HBA_VS: usize = 0x10;  // version

const GHC_AE: u32 = 1 << 31; // AHCI enable

// Per-port registers: base = 0x100 + port*0x80.
const PORT_BASE: usize = 0x100;
const PORT_STRIDE: usize = 0x80;
const PX_SIG: usize = 0x24;  // device signature
const PX_SSTS: usize = 0x28; // SATA status (DET in bits 3:0)

const SIG_SATA: u32 = 0x0000_0101; // a SATA disk (vs 0xEB14_0101 ATAPI)

/// Step A: detect the HBA and report disks. Idles afterwards (read/write later).
pub fn run(ctx: &ServiceContext, hba: &Mmio) -> ! {
    let cap = hba.read32(HBA_CAP);
    let vs = hba.read32(HBA_VS);

    // Ensure the HBA is in AHCI mode (GHC.AE) before touching ports.
    let mut ghc = hba.read32(HBA_GHC);
    if ghc & GHC_AE == 0 {
        hba.write32(HBA_GHC, ghc | GHC_AE);
        ghc = hba.read32(HBA_GHC);
    }
    let pi = hba.read32(HBA_PI);
    let n_ports = (cap & 0x1F) + 1;
    let n_slots = ((cap >> 8) & 0x1F) + 1;

    ctx.log_fmt(format_args!(
        "block-driver: AHCI HBA v{:x}.{:02x} CAP={:#010x} ({} ports, {} cmd slots) GHC={:#x} PI={:#010x}",
        (vs >> 16) & 0xffff, (vs >> 8) & 0xff, cap, n_ports, n_slots, ghc, pi
    ));

    let mut disk_port = None;
    for p in 0..32u32 {
        if pi & (1 << p) == 0 {
            continue;
        }
        let base = PORT_BASE + (p as usize) * PORT_STRIDE;
        let ssts = hba.read32(base + PX_SSTS);
        let det = ssts & 0xF;
        if det == 3 {
            let sig = hba.read32(base + PX_SIG);
            let is_sata = sig == SIG_SATA;
            ctx.log_fmt(format_args!(
                "block-driver: AHCI port {}: device present (DET=3) sig={:#010x}{}",
                p, sig, if is_sata { " — SATA disk" } else { "" }
            ));
            if is_sata && disk_port.is_none() {
                disk_port = Some(p);
            }
        }
    }
    match disk_port {
        Some(p) => ctx.log_fmt(format_args!(
            "block-driver: AHCI detection OK — SATA disk on port {}", p)),
        None => ctx.log("block-driver: AHCI — no SATA disk found on any implemented port"),
    }

    loop {
        ctx.yield_cpu();
    }
}
