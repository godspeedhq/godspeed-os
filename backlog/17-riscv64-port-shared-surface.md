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
