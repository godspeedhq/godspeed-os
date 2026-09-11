# 17. What the riscv64 port changed OUTSIDE riscv64, and what still needs testing elsewhere

**Status:** one item open (xHCI, below). Everything else here is a finding, recorded so nobody has to
re-derive the blast radius of this branch before touching another board.

Measured at `c05aa845` on `feat/riscv64`, against `main`.

## The kernel: nothing shared changed at all

```
git diff --stat main...HEAD -- kernel/src ':(exclude)kernel/src/arch/riscv64/*'
   (empty)
```

Every kernel line in this branch is inside `arch/riscv64/`, plus the linker scripts, `Cargo.toml` and
`build.rs`. x86, aarch64 and arm32 cannot see any of it. Kernel SCOPE went down rather than up: no new
syscalls, and PCI enumeration moved out of ring 0 into `hw-enumerator`, which is why that service's
exemption could be deleted from `service_embed_check.py`.

## Enforcement: all twelve checks pass, and they are alive

`commandments`, `unsafe_check`, `contract_check`, `dash_check`, `arch_boundary_check`,
`arch_seam_check`, `service_embed_check`, `embed_order_check`, `stack_fit_check`, `doc_refs`,
`facts_check`, `site_check` - all pass, and `commandments_redteam.py` catches every deliberate
break, so none of them is a check that has quietly stopped checking.

`arch_seam_check.py` is new here and is now wired into `arm_build.py` and `osdev` as well, so the
other ports gained enforcement from this work rather than losing any.

## Above the kernel: four shared files, three of them inert

| file | effect on other ISAs |
|---|---|
| `sdk/rust/src/syscall.rs` | the whole addition is `#[cfg(target_arch = "riscv64")]`. None. |
| `services/net-stack/src/main.rs` | riscv64 added to an existing arm/aarch64 TSC floor (x86 keeps its own); two counters on a log line that only prints on a ping timeout. Inert. |
| `scripts/selfcheck.gsh` | the net-lease gate SKIPs instead of FAILing with no DHCP server. Deliberate. A healthy board still PASSes identically; what is lost is that a dead receive path now reads the same as an absent server, which is stated at the site. |
| **`services/xhci/src/main.rs`** | **225 lines, no arch gating. The one real exposure.** |

## The open item: xHCI

`services/xhci/src/main.rs` gained the hot-plug handover fix - `ep0_hw_dequeue` reads the controller's
actual EP0 dequeue pointer instead of assuming where it is, plus a root-port settle wait and clearing
of port poison when nothing binds anywhere. It runs on every port that uses the service.

It targets the bug `project_xhci_hotplug_handover` records as root-caused on the Pi 4 ("hub
port-status probes are posted BEHIND the controller's dequeue"), so it should HELP x86 and the Pi 4.
It has only been verified on riscv64.

**It was deliberately NOT gated to one arch.** The Pi 4 GENET work already paid for that lesson: when
a fixed or compliant path is hidden behind a flag, the flag is the bug, and the unexercised path is
the one that rots. The answer is to test it, not to fence it off.

**To close this item:** on x86 and on the Pi 4, exercise USB hot-plug in both directions - keyboard
and mass storage, unplug and replug, including into a different port.

---

## Cross-port coverage of the NEUTRAL changes (2026-09-11)

Three neutral kernel changes landed while closing the riscv64 chaos wedge, and they run on every port:
`phys_in_ram` gaining a LOWER bound, `CORE_LEAVING` + routing all 15 `CORE_CURRENT` releases through
one helper, and BOUNDING the kill spin-wait (it had no deadline at all).

| port | `phys_in_ram` lower bound | kill-path changes |
|------|---------------------------|-------------------|
| riscv64 (VisionFive 2 Lite) | **HARDWARE: 461/0, chaos 100/100, 461/0, hot-plug, 461/0** | **same run** |
| x86_64 | **HARDWARE (HP T630, AMD): selfcheck 461/0, chaos 100/100, 461/0 again, hot-plug, 461/0 again** - plus identity 24/24 in QEMU | **same run**; identity 6A/6B/15/4A/4B/10A/10B all drive the kill path |
| aarch64 (Pi 4) | QEMU: boots, 12 services up, no panic - so the bound is not wrong here | **NOT COVERED** |
| arm32 (Pi 2) | builds clean (`--release`) | **NOT COVERED** |
| x86_64 (Dell Wyse, **Intel**) | **HARDWARE: 461/0, chaos 100/100, 461/0, hot-plug, 461/0** | **same run** |

**CLOSED ON x86 IN HARDWARE 2026-09-11, ON TWO DIFFERENT VENDORS.** The T630 is AMD and the Wyse is
Intel; both ran 461/0, chaos 100/100, 461/0, hot-plug, 461/0 with zero panics, zero wedges, zero
kill-path panics and zero kernel faults. Between them and the VisionFive that is THREE hardware
memory models - RISC-V weak ordering, AMD x86-TSO and Intel x86-TSO - which is what makes the SeqCst
handshake in `CORE_LEAVING` genuinely tested rather than executed three times on the same silicon.

Original T630 note follows.

**CLOSED ON x86 IN HARDWARE 2026-09-11.** 639 kills, 538 floods, 50 supervisor respawns absorbed, and the
selfcheck passed BEFORE, AFTER the chaos, and again after hot-plug - 461/0 every time. A different vendor
(AMD) and a different memory model from the riscv64 board, which is what makes the `SeqCst` handshake
changes genuinely tested rather than merely executed.

**The x86 result is the one that was most wanted, and it is a NULL result by design.** x86 RAM starts at
zero, so the lower bound closes a hole with no width there - anything OTHER than "no change" would have
meant the fix was wrong in a way no other port could expose. Nothing moved.

**The two gaps are real and are blocked on hardware, not on effort.** The operator has ONE microSD, in
use by the VisionFive, and reflashing it would destroy the active branch's only boot chain. The Pi 4
needs Raspberry Pi OS firmware on a FAT partition before GodspeedOS's two files (`godspeed8.img` +
`config.txt`) mean anything - GodspeedOS does not ship `start4.elf` and friends.

**To close them:** a spare card for either Pi, then `chaos max-carnage all-services 100 yes` followed by
`selfcheck` - in that order, because "alive" and "still correct" are different claims and the riscv64
result only became convincing when the suite passed AFTER the carnage.

The T630 needs no card: `build/os-usb.img` is built and flashes to a USB stick.
