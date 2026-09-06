# GodspeedOS

> Small enough to understand. Rigorous enough to trust.

A capability-based microkernel OS written in Rust. Every privileged action requires an explicit capability. Services are isolated. Failures are visible. Authority is never inherited or ambient.

📖 **[Documentation](https://godspeedhq.github.io/godspeed-os/)** · **[SDK API reference](https://godspeedhq.github.io/godspeed-os/api/godspeed_sdk/)** · **[Releases](https://github.com/godspeedhq/godspeed-os/releases)**

> **New here?** [**GETTING_STARTED.md**](GETTING_STARTED.md) takes you from zero to your first running service in a few minutes.

---

## Architecture

The whole system, bottom to top. It fits on a whiteboard on purpose - the constitution makes that a
requirement (§26.11), and a diagram you can redraw from memory is the only proof that it holds.

```
               ┌────────────────────────────────────────────────────────┐
 applications  │ shell  observe  chaos  edit  examples...               │
               ├────────────────────────────────────────────────────────┤
 services      │ fs  net-stack  console  events  time                   │
               ├────────────────────────────────────────────────────────┤
 DRIVERS       │ block-driver  nic-driver  xhci  ehci  dwc2             │
 (userspace)   │ each one a service, with only what it asked for        │
               └────────────────────────────────────────────────────────┘
                         ▲
                         │  spawns each, and RESTARTS it when it dies
               ┌────────────────────────────────────────────────────────┐
 trusted root  │ supervisor      (trusted, itself restartable)          │
               └────────────────────────────────────────────────────────┘
                         ▲
                         │  the kernel's ONE spawn. Nothing else.
 ═══════════════════════════════════════════════════════════════════════  ring 0
               ┌────────────────────────────────────────────────────────┐
 kernel        │ 1 memory isolation    4 capabilities                   │
 (mechanism,   │ 2 scheduling          5 interrupt routing              │
  not policy)  │ 3 IPC                 6 SMP routing                    │
               │                                                        │
               │ Six responsibilities. No filesystem, no network        │
               │ stack, no drivers, no policy. A module serving         │
               │ none of the six does not belong - and a checker        │
               │ refuses the build if one appears.                      │
               └────────────────────────────────────────────────────────┘
                         ▲
                         │  arch::imp - the only seam, and the only unsafe
               ┌────────────────────────────────────────────────────────┐
 hardware      │ x86_64      ARM (Pi 2)      AArch64 (Pi 4)             │
               └────────────────────────────────────────────────────────┘
```

Three things to take from it:

- **The kernel spawns exactly one service.** Everything else is spawned by the supervisor, which is
  itself an ordinary restartable service. The kernel is the only thing that cannot be killed.
- **Drivers live in userspace.** `xhci`, `nic-driver` and the rest are services holding a capability
  to a memory-mapped region and an interrupt line - nothing more. A crashed driver is a restart, not
  a reboot.
- **Authority points downward and is always explicit.** Nothing is ambient: a service can do exactly
  what its contract was granted and no more, checked on every privileged syscall.

### The six things the kernel does

The kernel box lists six responsibilities and nothing else - every other part of the system is a
service. Each one is *mechanism*: the kernel enforces the rule and never decides the policy.

1. **Memory isolation.** Each service gets its own virtual address space and page tables, so one
   service cannot read or write another's memory. Touching anything outside your mapped region is a
   page fault, and the service is killed rather than left running in an undefined state.

2. **Scheduling.** Every CPU core has its own queue of runnable tasks and takes them in turn. A 10 ms
   timer forces the switch whether the running task cooperates or not, so no service can hold a core
   by refusing to yield.

3. **IPC** - *inter-process communication*, how isolated services talk to each other. The kernel
   copies each message from sender to receiver: at most 4 KiB, into a queue that holds 16 before the
   sender has to wait. The copy is deliberate - sharing the memory instead would undo the isolation
   in (1).

4. **Capabilities.** A capability is an unforgeable token naming one resource, the rights held over
   it, and a generation number. Every privileged syscall is checked against the caller's table, and
   holding the token is the whole of the authority - nothing is granted on the basis of who you are.

5. **Interrupt routing.** When hardware raises an interrupt, the kernel turns it into a message to
   whichever driver service registered for that line. It never touches the device itself; delivering
   the news is all it does, and that is precisely what lets drivers run in userspace.

6. **SMP routing** - *symmetric multiprocessing*, meaning several equal CPU cores. The kernel maps
   each endpoint to the core its owner runs on, so a message crossing cores reaches the right queue,
   and the target core is woken with an **IPI** (*inter-processor interrupt*) - the signal one core
   sends another.

A seventh entry belongs in a service, not here. `scripts/commandments.py` fails the build if a kernel
module serves none of the six.

## Portability

One arch-neutral kernel sits behind a single seam, `arch::imp`; everything CPU-specific lives in
`arch/<isa>/`. Adding an ISA is bounded to that directory and enforced by CI - it does not touch a
single arch-neutral file.

| target | status |
|---|---|
| **x86-64** | **Full OS.** The `os.img` you flash: 4 cores, shell, AHCI storage, networking, USB (xHCI + EHCI), IOMMU-confined drivers. Verified on an HP T630 (AMD GX-420GI) and a Dell Wyse 5070 (Intel J5005). |
| **AArch64** (Raspberry Pi 4) | **Full OS.** Boots to an interactive `gsh>` on real hardware: 4-core SMP, GENET gigabit ethernet, USB keyboard and mass storage through the VL805 xHCI over PCIe, journalled filesystem. |
| **32-bit ARM** (Raspberry Pi 2) | **Full OS.** Same neutral kernel: 4-core SMP, USB keyboard, USB mass storage and USB ethernet - all three through the one DWC2 controller - plus the filesystem and the shell. |
| RISC-V 64/32, LoongArch | Compile and boot to their UART. |
| s390x | Compiles clean (big-endian). |

All three full ports are validated the same way and to the same bar: `selfcheck` (400-odd assertions),
then `chaos max-carnage` killing every service repeatedly, then `selfcheck` again, then a USB hotplug,
then `selfcheck` once more - **zero failures, zero kernel panics, zero liveness wedges**.

See **[docs/multi-arch.md](docs/multi-arch.md)** for the proof, **[docs/arm32-status.md](docs/arm32-status.md)**
and **[docs/aarch64.md](docs/aarch64.md)** for the two Pi ports, and
**[kernel/src/arch/CLAUDE.md](kernel/src/arch/CLAUDE.md)** for how to add an ISA.

## Writing a device driver

A driver is an ordinary service. **It needs no kernel source change** - you name the device by its
industry-standard PCI class code, and the kernel supplies the rest:

```
   supervisor IMAGES[]:
     ("my-driver", MY_ELF, ..., hwclass::pci_irq(0x0C_03_30, 0, true))
                                              │      │  │
                    the device's OWN claim ───┘      │  └─ confine it behind the IOMMU
                    about what it is                 └──── which BAR holds its registers

   the kernel then grants:  MMIO window · DMA arena · BDF · IOMMU domain · MSI vector
```

`0x0C0330` means "xHCI controller" everywhere, forever. The kernel records what is on the bus and holds
no opinion about any of it - no per-class statics, no vendor whitelist, no branch that decides *this is
the xHCI*. What it cannot do is decide *which* device you meant when there are two of a class; the
supervisor names one, and that wins.

The image still ships inside the build, so adding a driver means a rebuild and a reflash. That is not
the expensive part: the cost of a kernel change is not compile time, it is that editing kernel source
reopens *"is the kernel still correct?"*. Linking a new driver crate against unchanged, battle-tested
kernel code reopens nothing.

## How it works

**Capabilities** - every privileged action requires an explicit, unforgeable token. A capability carries a resource ID, a rights set, and a generation number. Stale capabilities return `EndpointDead`. Forged ones return `CapNotHeld`. There is no ambient authority.

**IPC** - synchronous message passing with bounded queues (16 messages per endpoint). Services are pinned to CPU cores. Cross-core sends route through the kernel's routing table and wake the receiver via IPI. Zero-copy is permanently rejected - isolation is more important.

**Supervisor:** the service with restart authority. When a service is killed, its endpoint generation is bumped. All outstanding capabilities immediately become stale. Clients detect `EndpointDead`, look up the new instance by name via the kernel's name directory, and resume. The new instance may be on a different core, which is invisible to callers. The supervisor is itself restartable: if it dies, the **kernel respawns it** and it reconciles with the still-running services. The only unkillable component is the kernel.

**Scheduler** - per-core run queues, round-robin, 10 ms preemption quantum enforced by the local timer. Services are placed at spawn and never migrate. Yield is advisory; preemption is not.

---

## Design principles

| Principle | What it means |
|-----------|---------------|
| No ambient authority | Every privileged action requires a capability |
| Explicit authority | Authority comes from holding a cap, not from identity or ancestry |
| Bounded behavior | Queues, tables, memory, and messages all have fixed limits |
| Loud failures | `EndpointDead`, `CapRevoked`, `AllocDenied` - never silent fallback |
| Identity over location | Service names are stable; core assignments are not |
| One irreducible truth | Store the minimal source; derive (and reconcile) every cache, index, or count |
| Restartability | Every service survives kill + respawn, even the supervisor; only the kernel is unkillable |

These distil into the **[Ten Commandments of Godspeed](COMMANDMENTS.md)**, the human-readable form of the constitution.

---

## Test suite

GodspeedOS treats testing as architecture. The suite is layered - each layer must pass before the next is meaningful.

| Suite | Purpose | Status |
|-------|---------|--------|
| Identity (15 tests, 24 cases) | Pin constitutional invariants | 24/24 ✅ |
| Property (P1-P10) | Universal correctness under random inputs | 10/10 |
| Fuzz (F1-F8) | Kernel never panics on user-controllable input | Active |
| Stress (S1-S10) | No drift, leaks, or corruption over time | Active |
| Performance (B1-B10) | Latency / throughput baselines | Active |
| Adversarial (A1-A10) | Capability isolation under direct attack | Active |
| Chaos (C1-C7) | Graceful degradation under partial failures | Active |

The layers above are categories - each generalises over many inputs. Below them sit **scenario tests**,
which do the opposite: one situation, forced deliberately, because waiting for it is not practical.

| Scenario | What it forces | Status |
|----------|----------------|--------|
| `peer-storm` | Kills `block-driver` every 60 ms for 20 s while the shell drives file I/O - the window where a service's peer is unreachable, manufactured rather than waited for. Every outage must END; a permanent one is the bug. | 7/7 ✅ |
| `adopt-storm` | Storms the **supervisor** itself, so its reconciliation path runs over and over: 84 deaths and 462 service adoptions in one run, with the filesystem still working afterwards. | 7/7 ✅ |

They exist because two failures could not be reproduced any other way: the hardware they appear on
restarts services faster than a test can catch, and one of the two ARM ports cannot mount a disk under
emulation at all. `peer-storm` found a real protocol desync on its first outing - `fs` and
`block-driver` left permanently one reply out of phase - which no suite above had surfaced in months.

### Static analysis & unsafe audit

Every `unsafe` block is inventoried in `audits/unsafe-audit.md` and enforced by
`scripts/unsafe_check.py` - counts may not grow without a written SAFETY argument.
The inventory grows as the system does - three CPU ports and userspace drivers all need it - so the
check is that every line is ACCOUNTED for, not that the count stays still. Figures below are from the
current tree; the boot-verified pass they were first taken from is
`milestones/testing/static-analysis-audit.md` (2026-05-31, AMD T630):

| Check | Result |
|-------|--------|
| Unsafe confined to permitted layers (§18.1) | audit passes: 1049 lines across 69 files, no unaccounted additions |
| Safety / correctness lints (static-mut refs, fn-casts, redundant `unsafe`) | ✅ 0 |
| Kernel build warnings | 104 → 57 (remaining are intentional unwired architecture) |
| Hardware boot regression | ✅ clean - 4 cores, cross-core ping/pong to 83k+ msgs, zero faults |

---

## Getting started

**Want to write a service?** See [**GETTING_STARTED.md**](GETTING_STARTED.md) - a 5-minute, copy-`examples/00-hello` walkthrough. The rest of this section is about building and booting the OS itself.

**Requirements:** Rust nightly (pinned in `rust-toolchain.toml`), QEMU on your PATH, an x86_64 host, and the Limine bootloader binaries (one-time setup below). The same commands work on Linux, macOS, and Windows - `osdev` handles the platform differences, and there is no Makefile to keep in sync.

**Set up Limine (once).** GodspeedOS boots via the Limine bootloader, whose binaries are not committed (`tools/` is gitignored). Download a Limine binary release (https://github.com/limine-bootloader/limine/releases - the project tracks the 12.x line) and copy these into `tools/limine/`:

- `BOOTX64.EFI` - the UEFI bootloader (used by `osdev image`),
- `limine-bios.sys` - the BIOS stage,
- the host install tool: `limine` on Linux/macOS, `limine.exe` on Windows.

```bash
# Build the kernel + all services
cargo run -p osdev -- build

# Boot in QEMU with 4 cores
cargo run -p osdev -- run --smp 4

# Run the identity test suite
cargo run -p osdev -- test identity

# Run property tests
cargo run -p osdev -- test property

# Force a peer outage and watch the system recover from it
cargo run -p osdev -- test peer-storm
```

The build is pure Cargo plus the `osdev` CLI - identical on every platform. The full `osdev` CLI reference is in `CLAUDE.md §17` and `osdev/CLAUDE.md`.

### Flashing to real hardware

`osdev image` builds a UEFI-bootable `build/os.img` for a USB stick. Two things make a boot on real hardware reliable:

1. **Build clean, and copy the image *before* you boot it.** `osdev run` and `osdev test` rebuild `build/os.img` incrementally as a side effect, and an incrementally-built kernel can boot under QEMU yet be **rejected by real UEFI firmware** (it boots in emulation but the machine won't pick up the USB). So build the image clean and grab it immediately, before anything reboots it:

   ```bash
   cargo clean --target x86_64-unknown-none   # discard any incremental artifacts
   cargo run -p osdev -- image                 # writes a clean build/os.img
   cp build/os.img build/my-hw.img             # copy NOW, before any `osdev run` / `osdev test`
   ```

   **Booting in QEMU is not proof the on-hardware image is good - a clean build is.** If a copy is taken *after* an `osdev run`/`osdev test` (both rebuild `os.img`), you may hand hardware an incremental image that only works under QEMU.

2. **Flash the copy** with Rufus (DD Image mode) or `dd if=build/my-hw.img of=/dev/sdX bs=4M`, let the write fully finish, and boot the stick in **UEFI** mode. Serial console is 115200 8N1; a healthy boot prints `kernel: N cores ready` then `supervisor: ready`.

---

## Repository layout

```
kernel/       bare-metal microkernel
services/     system services (supervisor, events, block-driver, fs, shell, ...)
sdk/rust/     Rust SDK for service development
osdev/        build / test / run tooling
contracts/    service contracts and JSON schema
examples/     annotated, Commandment-grounded teaching examples (start at examples/README.md)
tests/        identity, property, fuzz, stress, chaos suites
docs/         architecture notes and design docs
website/      documentation site (mdBook; renders this repo's docs)
```

---

## Documentation site

**Live at [godspeedhq.github.io/godspeed-os](https://godspeedhq.github.io/godspeed-os/)**, with the
[SDK API reference](https://godspeedhq.github.io/godspeed-os/api/godspeed_sdk/) under `/api`.

Two sections worth knowing about:
[**the services**](https://godspeedhq.github.io/godspeed-os/services.html) - what each one is, what it
may *not* do, and diagrams of how they reach each other over endpoints - and
[**the utilities**](https://godspeedhq.github.io/godspeed-os/utilities.html), all 47 of them, each with
its full specification.

The docs in this repo also render as a browsable site built with
[mdBook](https://rust-lang.github.io/mdBook/). The site is a
**derived view**: every page is an `{{#include}}` of the real file, so it never duplicates or drifts
from the source. Acronyms link to the glossary and carry a hover definition, injected at build time so
the markdown stays plain. A GitHub Action rebuilds and republishes it on every push to `main` that touches a
doc, so editing `CLAUDE.md` or a `docs/` file updates the site automatically.

Preview or update it locally:

```bash
cargo install mdbook          # one-time
cd website
mdbook serve --open           # live-reload preview at http://localhost:3000
```

How the includes work, how the gallery screenshots are captured, and the launch checklist are in
[`website/README.md`](website/README.md).

---

## Design reference

The full specification (capability model, IPC semantics, scheduler rules, memory enforcement, bootstrap sequence, unsafe policy, and constitutional invariants) is in `CLAUDE.md`. Its human-readable distillation is **[`COMMANDMENTS.md`](COMMANDMENTS.md)**: ten laws that bound every design choice.

The system is defined there first. The implementation exists to satisfy it.
