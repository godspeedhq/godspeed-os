# 21. The NIC backend is chosen by instruction set, and NET_DEVICE has no caller left

**Severity:** low today, structural. Nothing is broken on any of the five machines (four ports); what
is recorded here is an axis that is wrong and a kernel surface that is now unused.
**Status: FIXED 2026-10-28, both items.** Item 1 carded the same day: the Pi 4 took GENET (`genet v5.0 ... driving it from userspace`) and the VisionFive took DWMAC (`dwmac up`) because the kernel said so, each `selfcheck` 679/0; no cable on either, so WiFi carried the link and GENET's and DWMAC's frame paths, which this did not change, were not re-exercised. *Was:* OPEN for item 1's cards only - item 1 BUILT 2026-10-28. The kernel publishes which ethernet controller its boot probe found soldered to the SoC (InspectKernel query 29, `soc_nic`: GENET on the Pi 4, DWMAC on the VisionFive, none elsewhere; the arch seam's soc_nic_present became `soc_nic`), and `nic-driver` chooses its backend from that answer and then from the PCI identity, falling through to the Pi 2's USB bridge when there is neither. `target_arch` in `nic-driver/src/main.rs` went from 10 sites to 0; which backends an image CARRIES is one table in `nic-driver/build.rs` (4), for size alone. QEMU: x86 identity 24/0, adv 15/0, shell 276/0; riscv64 `virt` with an e1000 takes the PCI path (`e1000 up link UP`); the Pi 2 takes the bridge; the Pi 4 under `raspi4b`, which models no GENET, serves empty replies as before. Owed: cards on the Pi 4 (GENET) and VisionFive (DWMAC), the two the query actually decides. *Was:* OPEN for item 1 only. Item 2 is DONE 2026-10-28: syscalls 42-44 are removed with NET_DEVICE (ResourceId 10, privilege bit 11), and USB_DISK's 46-49 (ResourceId 12) with them - the same dead shape, recorded as audit SEC-37 - from the kernel, every arch's seam (125 members, from 134) and the SDK (its floor 80 -> 74). QEMU on every port; hardware-confirmed 2026-10-28 on all five boards at `1d569c22` (Wyse, T630, Pi 2, Pi 4, VisionFive: selfcheck 673/673/670/679/679, 0 failed; kill-storm of the USB host 10/10 on each). Item 1 needs a new kernel query and is untouched. *Was:* Item 2's
first step - stop GRANTING NET_DEVICE - landed on `portability-hardening` and is hardware-confirmed; its
second - the SDK wrappers - went on 2026-10-11 (`feat/ask`). The syscalls themselves remain.

---

## 1. Three of the four backend arms ask the ISA when the question is "which MAC"

`services/nic-driver/src/main.rs` dispatches its backend in `service_main`:

| Board | Arm | What it actually asks |
|-------|-----|-----------------------|
| T630 / Wyse / QEMU | `not(any(arm, aarch64, riscv64))` | **the device** - `ctx.nic_vendor_device()`, a PCI identity the kernel discovered at runtime |
| Pi 4 | `target_arch = "aarch64"` | the instruction set |
| Pi 2 | `target_arch = "arm"` | the instruction set |
| VisionFive 2 | `target_arch = "riscv64"` | the instruction set |

The x86 arm is already on the right axis: a third PCI NIC in that machine is driven without a
rebuild, which is what `hw_pci_class = "020000"` in the contract exists to enable (step D). The other
three are on the wrong one, and the failure is concrete rather than theoretical: **a second aarch64
board whose MAC is not GENET takes the GENET arm and drives the wrong silicon.** The ISA is standing
in for the board, exactly as it was in the supervisor's spawn table before `a0392632`.

**Why it was not fixed with the same move.** In the supervisor the board fact could be named locally
(`board::STORAGE_PEERS`), because the spawn table only needed a peer list. Here the fact is *which
controller exists*, and a SoC MAC has no enumerable identity: no bus to scan, no vendor/device pair,
and probing a version register means reading an address that on the next board may not be a register
at all. There is nothing to ask.

**The route that closes it.** The kernel already knows - its boot probe is what decides whether to
grant this service an MMIO window in the first place (`arch/aarch64/genet.rs` reads GENET's revision
register; the RISC-V path resolves dwmac's window the same way). It simply does not publish the
answer. One `InspectKernel` query, in the shape of query 14 (`nic_vendor_device`) and query 18
(`xhci_present`) that already exist, turns all three ISA arms into one runtime match:

```rust
match ctx.nic_controller() {        // new query: which network controller did the probe find
    NIC_GENET => genet::genet_main(ctx),
    NIC_DWMAC => dwmac::dwmac_main(ctx),
    NIC_DWC2  => kernel_net_main(ctx),
    _         => /* PCI: ask the device, as today */
}
```

That is **a new kernel query**, so it is a deliberate decision and not a refactor: §26.2 says a
feature is pulled into existence by a real problem, and the real problem arrives with the second
board of an ISA we already support. Recorded here so the next person meets the argument rather than
the symptom.

The module gates (`#[cfg(target_arch = "aarch64")] mod genet;`) stay regardless. Each backend is a
whole MAC; compiling GENET's ~1,450 lines into a RISC-V image would be dead weight in a service with
a 16 MiB limit, not merely dead code.

## 2. NET_DEVICE (syscalls 42-44) has no caller anywhere in userspace

`kernel_net_main` used to carry two bodies, chosen by `#[cfg(not(target_arch = "arm"))]`: IPC to the
`dwc2` service on the Pi 2, and the NET_DEVICE syscalls (`net_info` / `net_frame_tx` /
`net_frame_rx`) for the Pi 4's in-kernel GENET. **The syscall body has had no caller since GENET moved
into this service** (CLAUDE.md §6.4, amendment 2026-08-09) - `service_main` sends aarch64 to
`genet::genet_main`, so it was compiled into every Pi 4 image and never executed. It is deleted on
this branch; thirteen `#[cfg]` sites in one function existed to choose between a live path and a dead
one.

What remains is the other end of it:

- ~~`sdk/rust/src/service_context.rs` still exposes `net_info`, `net_frame_tx`, `net_frame_rx`.~~
  **DONE 2026-10-11.** Removed with no caller anywhere in the tree; every port's image builds and the
  x86 suites pass (the SDK's `unsafe` floor fell 80 -> 77).
- ~~The kernel still grants NET_DEVICE by name on `arm` and `aarch64`.~~ **DONE 2026-09-13.** The
  supervisor stopped requesting it (`10d3b43e`) and the kernel's by-name arm is `net_device: false`
  (`3c75638e`). Confirmed on all five machines: the Pi 4 and both x86 boxes boot with DHCP, ARP,
  ping and SNTP working, and the Pi 2 and VisionFive likewise.
- The kernel still IMPLEMENTS syscalls 42-44 (the SDK wrappers are gone, above). Nothing holds
  the capability, so nothing can call them.
- `nic-driver`'s contract already says the aarch64 grant "simply goes unused".

So a SYSCALL SURFACE is maintained for no caller. That is what is left: the capability half is
closed, and nothing can reach syscalls 42-44 because nothing holds the authority to. Not a bug -
dead code that cannot be invoked harms nothing - but §26.2's preferred state for an unneeded feature
is that it not be there.

**Deliberately not done here** because removing a syscall is a kernel change and this branch is about
bounding what a new ISA has to touch, not about shrinking the kernel. The order matters too: the
grant went first (done - see above, and it was provable by boot exactly as predicted); the SDK
wrappers are second (done 2026-10-11) and the syscalls last, each wanting a boot behind it. One step
remains: the syscalls. Note that the Pi 2 path does **not**
use these syscalls either - it reaches `dwc2` by IPC - so this is all four ports, not three.
