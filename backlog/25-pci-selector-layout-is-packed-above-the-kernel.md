# 25. `hw-enumerator` bit-packs a hardware config selector, which is the kernel's job

**Severity:** structural, no defect today. Both encodings are correct and both ports work. What is
wrong is which side of the seam the packing sits on, and a doc claim that said the seam was cleaner
than it is.
**Status: OPEN.** Recorded rather than done because the fix is a kernel-and-SDK change across three
ports.

## What it is

`services/hw-enumerator/src/main.rs::cfg_read` builds the PCI configuration selector itself:

```rust
#[cfg(target_arch = "x86_64")]
let sel = ((bus as u32) << 16) | ((dev as u32) << 11) | ((func as u32) << 8);   // mechanism #1
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
let sel = ((bus as u32) << 20) | ((dev as u32) << 15) | ((func as u32) << 12);  // ECAM
```

and hands the packed word to `ctx.pci_cfg_read(sel, offset)`.

**The kernel then unpacks it**, so the layout is a contract written out at both ends:

- `kernel/src/arch/x86_64/pci.rs::cfg_read_gated` does `0x8000_0000 | (sel & 0x00FF_FF00) | ...`,
  which is only meaningful in the mechanism-#1 layout;
- `kernel/src/arch/aarch64/pcie.rs::cfg_read_gated` opens `let bus = (sel >> 20) & 0xFF;` to
  range-check the bus, which is only meaningful in the ECAM layout.

So a third platform with a third layout needs a change in `arch/<isa>/` **and** a change in this
service. The comment above `cfg_read` claimed the opposite - "it never learns either encoding - so a
third platform with a third layout needs no kernel change at all" - and that is corrected in the same
commit as this entry.

## Why it is worth fixing rather than tolerating

Naming a device is policy and belongs in the service. **Addressing** it is mechanism and belongs in
`arch/` (§26.10). This is the only place in userspace that packs bits for a specific host bridge, and
it is the reason two of the three remaining arch-conditional sites in this crate exist at all.

It is also the branch's own test, failed: `arch_boundary_check.py` enforces that neutral KERNEL code
reaches hardware only through the seam, and nothing enforces the same on the userspace side of it.
A service encoding a host bridge's address format is the same category of leak, one layer up.

## What is RULED OUT

- **Not a bug.** Both layouts are right; the Pi 4, the VisionFive and both x86 boxes enumerate
  correctly, and `hw-enumerator`'s walk is cross-checked against the kernel's own scan at boot.
- **Not fixable by moving the cfg.** Inventing a `pci_cfg_ecam` cfg in `build.rs` would take two
  sites to three (two here plus the mapping), and leave the packing in the same place. Measured, not
  assumed - that is why it was not done.
- **Not improved by defaulting.** Today a port that is neither x86 nor ECAM fails to COMPILE
  ("cannot find value `sel`"), which is loud. Giving it a default would make a new port silently
  address the wrong registers, which is worse than a compile error (§26.7, invariant 12). The current
  shape is defensible and deliberately kept until the real fix.

## The next concrete step

Define a **neutral BDF encoding** for the `pci_cfg_read`/`pci_cfg_write` syscall arguments - the
service passes `(bus, device, function)` in one agreed word that means nothing to hardware - and let
each `arch/<isa>/cfg_read_gated` translate it to its own host bridge's format. That is where the
translation belongs and where a fifth port would already be writing code.

Cost: one SDK signature, three arch translations, and this service's two lines become zero. It
re-widens testing to all four boards because it touches the path every PCI driver's spawn depends on,
which is why it is a deliberate piece of work rather than a tidy-up.
