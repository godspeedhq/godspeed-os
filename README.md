# GodspeedOS

> Small enough to understand. Rigorous enough to trust.

A capability-based microkernel OS written in Rust. Every privileged action requires an explicit capability. Services are isolated. Failures are visible. Authority is never inherited or ambient.

📖 **[Documentation](https://godspeedhq.github.io/godspeed-os/)** · **[Standard library API](https://godspeedhq.github.io/godspeed-os/api/godspeed/)** · **[SDK API](https://godspeedhq.github.io/godspeed-os/api/godspeed_sdk/)** · **[Releases](https://github.com/godspeedhq/godspeed-os/releases)**

> **New here?** [**GETTING_STARTED.md**](GETTING_STARTED.md) takes you from zero to your first running service in a few minutes.
>
> **Writing software for GodspeedOS without building the OS?** The developer CLI,
> [**godspeed-cli**](https://github.com/godspeedhq/godspeed-cli), creates, builds, packages and publishes a
> service, utility, app, driver or library, and a Godspeed machine fetches and installs it with `godspeed fetch`
> and `godspeed install` at its `gsh>` prompt.
>
> **Contributing?** [**CONTRIBUTING.md, "Learn from a real change"**](CONTRIBUTING.md#learn-from-a-real-change) points every kind of contribution - a service, a driver, the kernel, a utility, a test, the docs, a gate, a chaos find - at code in the tree and a merged commit that did exactly that.

---

## Architecture

The whole system, bottom to top. It fits on a whiteboard on purpose - the constitution makes that a
requirement (§26.11), and a diagram you can redraw from memory is the only proof that it holds.

```
               ┌────────────────────────────────────────────────────────┐
 applications  │ shell  observe  chaos  edit  examples...               │
               ├────────────────────────────────────────────────────────┤
 services      │ fs  net-stack  console  events  time  ...              │
               ├────────────────────────────────────────────────────────┤
 DRIVERS       │ block-driver  nic-driver  xhci  ehci  dwc2  ...        │
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
 hardware      │ x86_64   ARM (Pi 2)   AArch64 (Pi 4)   RISC-V (VF2)    │
               │ four ISAs, one kernel                                  │
               └────────────────────────────────────────────────────────┘
```

Three things to take from it:

- **The kernel spawns exactly one service.** Everything else is spawned by the supervisor, which is
  itself an ordinary restartable service. The kernel is the only thing that cannot be killed.
- **Drivers live in userspace.** `xhci`, `nic-driver` and the rest are services holding only what they
  were granted: their device's registers, its interrupt, and, for a device that does DMA, a DMA arena. A
  crashed driver is a restart, not a reboot.
- **Authority points downward and is always explicit.** Nothing is ambient: a service can do exactly
  what its spawn request was granted and no more, checked on every privileged syscall.

### The six things the kernel does

The kernel box lists six responsibilities and nothing else - every other part of the system is a
service. Each one is *mechanism*: the kernel enforces the rule and never decides the policy.

1. **Memory isolation.** Each service gets its own virtual address space and page tables, so one
   service cannot read or write another's memory. Touching anything outside your mapped region is a
   page fault, and the service is killed rather than left running in an undefined state.

2. **Scheduling.** Every CPU core has its own queue of runnable tasks and takes them in turn. A timer
   forces the switch whether the running task cooperates or not, so no service can hold a core by
   refusing to yield.

3. **IPC** - *inter-process communication*, how isolated services talk to each other. The kernel
   copies each message from sender to receiver, into a bounded queue; a sender to a full queue waits.
   The copy is deliberate - sharing the memory instead would undo the isolation in (1).

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
`arch/<isa>/`. Adding an ISA is bounded to that directory plus the registration files
`docs/porting.md` marks; every `osdev build` refuses neutral code that reaches hardware outside the seam.

| target | status |
|---|---|
| **x86-64** | **Full OS**, verified on an HP T630 and a Dell Wyse 5070: SMP, the shell, AHCI storage, networking, USB (xHCI and EHCI), WiFi through a USB dongle, sound through the HD Audio codec, and IOMMU-confined drivers. |
| **AArch64** (Raspberry Pi 4) | **Full OS** on real hardware: SMP, GENET ethernet, the onboard WiFi, USB through the VL805 xHCI over PCIe, sound through the 3.5 mm jack, the journalled filesystem. |
| **32-bit ARM** (Raspberry Pi 2) | **Full OS** on real hardware, on the same neutral kernel: SMP, USB keyboard, mass storage, ethernet and a WiFi dongle, all through the one DWC2 controller, sound through the 3.5 mm jack, the filesystem and the shell. |
| **RISC-V 64** (StarFive VisionFive 2 Lite) | **Full OS** on real hardware: SMP on a JH7110, HDMI, USB through the onboard hub, DWMAC ethernet, the onboard WiFi, Sv39 paging, the journalled filesystem. It has no audio output, and `audio` says so. The first port finished with **no arch-neutral kernel code naming the ISA**. |
| RISC-V 32, LoongArch | Compile and boot to their UART. |
| s390x | Compiles clean (big-endian). |

All four full ports are validated the same way and to the same bar: `selfcheck`, then `chaos max-carnage`
killing every service repeatedly, then `selfcheck` again, then a USB hotplug, then `selfcheck` once more -
**no failures, no kernel panics, no liveness wedges**.

See **[docs/multi-arch.md](docs/multi-arch.md)** for the proof, **[docs/arm32-status.md](docs/arm32-status.md)**
and **[docs/aarch64.md](docs/aarch64.md)** for the two Pi ports,
**[kernel/src/arch/riscv64/CLAUDE.md](kernel/src/arch/riscv64/CLAUDE.md)** for the RISC-V port, and
**[kernel/src/arch/CLAUDE.md](kernel/src/arch/CLAUDE.md)** for how to add an ISA.

## Writing a device driver

A driver is an ordinary service. **It needs no kernel source change** - you name the device by its
industry-standard PCI class code, and the kernel supplies the rest. There are two ways to ship one.

**In the tree**, as a row in the supervisor's image table:

```
   supervisor IMAGES[]:
     ("my-driver", MY_ELF, ..., hwclass::pci_irq(0x0C_03_30, 0, true))
                                              │      │  │
                    the device's OWN claim ───┘      │  └─ confine it behind the IOMMU
                    about what it is                 └──── which BAR holds its registers

   the kernel then grants:  MMIO window · BDF · MSI vector, and where the row asks,
                            a DMA arena in its own IOMMU domain
```

`0x0C0330` means "xHCI controller" everywhere, forever. The kernel records what is on the bus and holds
no opinion about any of it - no per-class statics, no vendor whitelist, no branch that decides *this is
the xHCI*. What it cannot do is decide *which* device you meant when there are two of a class; the
supervisor names one, and that wins. An in-tree driver ships inside the image, so adding one means a
rebuild and a reflash - and the cost of a kernel change is not compile time, it is that editing kernel
source reopens *"is the kernel still correct?"*. Linking a new driver crate against unchanged kernel code
reopens nothing.

**As a package**, with no rebuild at all: its contract names the device (`hw_pci_class`, `hw_pci_bar`),
the developer CLI packages and signs it, and the operator installs it on a running machine and approves the
device. GodspeedOS 0.29.0 and 0.30.0 give a driver package its device's registers only; the release after
0.30.0 adds its interrupt, a DMA arena (asked about separately, because on a machine whose IOMMU does not
confine the device it is kernel-equivalent reach), taking over a device an in-tree driver handles, and USB
devices by vendor and product. `docs/developer-services-design.md` 13.1 has the whole of it.

## How it works

**Capabilities** - every privileged action requires an explicit, unforgeable token. A capability carries a resource ID, a rights set, and a generation number. Stale capabilities return `EndpointDead`. Forged ones return `CapNotHeld`. There is no ambient authority.

**IPC** - synchronous message passing with bounded queues. Services are pinned to CPU cores. Cross-core sends route through the kernel's routing table and wake the receiver via IPI. Zero-copy is permanently rejected - isolation is more important.

**Supervisor:** the service with restart authority. When a service is killed, its endpoint generation is bumped. All outstanding capabilities immediately become stale. Clients detect `EndpointDead`, look up the new instance by name via the kernel's name directory, and resume. The new instance may be on a different core, which is invisible to callers. The supervisor is itself restartable: if it dies, the **kernel respawns it** and it reconciles with the still-running services. The only unkillable component is the kernel.

**Scheduler** - per-core run queues, round-robin, with a preemption quantum enforced by the local timer. Services are placed at spawn and never migrate. Yield is advisory; preemption is not.

The constants behind these - the message size, the queue depth, the quantum - are in `CLAUDE.md` (§8.5, §9.1).

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

| Suite | Purpose |
|-------|---------|
| Identity | Pin constitutional invariants (`CLAUDE.md` §22) |
| Property (P1-P10) | Universal correctness under random inputs |
| Fuzz (F1-F8) | Kernel never panics on user-controllable input |
| Stress (S1-S10) | No drift, leaks, or corruption over time |
| Performance (B1-B10) | Latency / throughput baselines |
| Adversarial (A1-A15) | Capability isolation under direct attack |
| Chaos (C1-C7) | Graceful degradation under partial failures |

The layers above are categories - each generalises over many inputs. Below them sit **scenario tests**,
which do the opposite: one situation, forced deliberately, because waiting for it is not practical.

| Scenario | What it forces |
|----------|----------------|
| `peer-storm` | Kills `block-driver` over and over while the shell drives file I/O - the window where a service's peer is unreachable, manufactured rather than waited for. Every outage must END; a permanent one is the bug. |
| `adopt-storm` | Storms the **supervisor** itself, so its reconciliation path runs over and over, with the filesystem still working afterwards. |

They exist because two failures could not be reproduced any other way: the hardware they appear on
restarts services faster than a test can catch, and one of the two ARM ports cannot mount a disk under
emulation at all. `peer-storm` found a real protocol desync on its first outing - `fs` and
`block-driver` left permanently one reply out of phase - which no suite above had surfaced in months.

### Static analysis & unsafe audit

Every `unsafe` block is inventoried in `audits/unsafe-audit.md` and enforced by
`scripts/unsafe_check.py`: an `unsafe` line the audit does not account for fails the build, and a count
may not grow without a written SAFETY argument. `python scripts/unsafe_check.py` reports the current
inventory. Every service, example, the standard library and `osdev` carry `#![deny(unsafe_code)]`, so the
compiler refuses `unsafe` there before any script runs.

---

## Getting started

**Want to write a service?** See [**GETTING_STARTED.md**](GETTING_STARTED.md) - a short, copy-`examples/00-hello` walkthrough. The rest of this section is about building and booting the OS itself.

**Requirements:** Rust nightly, pinned to an exact date in `rust-toolchain.toml` (rustup installs it for you), QEMU on your PATH, **Python 3 on your PATH as `python`**, an x86_64 host, and the Limine bootloader binaries (one-time setup below). The same commands work on Linux, macOS, and Windows - `osdev` handles the platform differences, and there is no Makefile to keep in sync.

**Python is not optional, so it is stated plainly rather than discovered.** `osdev build` runs the enforcement layer - the checkers in `scripts/` that hold the Ten Commandments, the contracts, the arch boundary and the documentation - and it **refuses to build** if it cannot run one of them, because a checker that cannot run is not a checker that passed. Building for a non-x86 board needs it too: `scripts/board.py` and the three per-board scripts are the only way to produce a Raspberry Pi or VisionFive image. The oldest Python they run on is `FLOOR` in `scripts/python_floor_check.py`, which fails if any script needs a newer one; `docs/conformance.md` records how it was measured.

**Set up Limine (once).** GodspeedOS boots via the Limine bootloader, whose binaries are not committed (`tools/` is gitignored). Download the Limine binary release the release workflow uses (`LIMINE_VERSION` in `.github/workflows/release.yml`, from https://github.com/limine-bootloader/limine/releases) and copy these into `tools/limine/`:

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

**Before you commit: `conform`.** One command over the whole enforcement layer - the checkers that
hold the Ten Commandments, the contracts, the arch boundary and the documentation. It fixes what is
DECIDABLE and reports what needs JUDGEMENT, naming which Commandment a violation breaks and what to do
about it:

```bash
# Fix what is decidable, report what needs a decision
py scripts/conform.py

# Report both and change nothing - this is what CI wants
py scripts/conform.py --check

# What a rule means, why it exists, and whether it is auto-fixable
py scripts/conform.py --explain GS0403

# Every rule, its code and its Commandment
py scripts/conform.py --list
```

A clean tree says `0 would be fixed, 0 need a decision - N checks ran, N passed`. The count of checks
that RAN is there on purpose: a run that silently skipped some of them and printed a clean verdict is
the failure the whole thing exists to prevent.

**The codes are readable.** `GS0001`..`GS0010` are the Ten Commandments - the number IS the numeral, so
`GS0004` is Commandment IV. Above that, `GS01xx` is house writing conventions, `GS02xx` the kernel
boundary and unsafe, `GS03xx` contracts and authority, `GS04xx` documentation and comments.
`conform --list` prints the legend and every rule; `docs/conformance.md` has the reasoning.

`osdev build` runs the same checkers and refuses to build if any fails, so `conform` is not an extra
gate - it is the same gate, askable. `osdev conform` (with the same flags) is a shim that forwards to
`scripts/conform.py` and passes its exit code through; `docs/conformance.md` is the spec.

The build is pure Cargo plus the `osdev` CLI - identical on every platform. The full `osdev` CLI reference is in `CLAUDE.md §17` and `osdev/CLAUDE.md`.

### Flashing to real hardware

`osdev image` builds a UEFI-bootable `build/os-usb.img` for a USB stick (named apart from `build/os.img`, the BIOS image `osdev run` boots in QEMU, so the two cannot be confused). Two things make a boot on real hardware reliable:

1. **Build clean, and copy the image *before* you boot it.** `osdev run` and `osdev test` rebuild the kernel incrementally as a side effect, and an incrementally-built kernel can boot under QEMU yet be **rejected by real UEFI firmware** (it boots in emulation but the machine won't pick up the USB). So build the image clean and grab it immediately, before anything reboots it:

   ```bash
   cargo clean --target x86_64-unknown-none   # discard any incremental artifacts
   cargo run -p osdev -- image                 # writes a clean build/os-usb.img
   cp build/os-usb.img build/my-hw.img         # copy NOW, before anything rebuilds it
   ```

   **Booting in QEMU is not proof the on-hardware image is good - a clean build is.** If the image is built on top of an `osdev run`/`osdev test` (both rebuild the kernel incrementally) without the clean step, you may hand hardware an incremental image that only works under QEMU.

   `py scripts/board.py x86` makes the same image and ends by naming it; the clean step is still yours.
   The Raspberry Pi and VisionFive images are `py scripts/board.py pi2|pi4|visionfive`, each ending
   with what to copy where.

2. **Flash the copy** with Rufus (DD Image mode) or `dd if=build/my-hw.img of=/dev/sdX bs=4M`, let the write fully finish, and boot the stick in **UEFI** mode. Serial console is 115200 8N1; a healthy boot prints `smp: N cores ready` then `supervisor: ready`.

---

## Repository layout

```
kernel/       bare-metal microkernel; arch/<isa>/ is the only part that knows the machine
services/     system services and drivers (supervisor, fs, shell, xhci, audio-driver, ...)
stdlib/rust/  the standard library, `gs` - what every service and example is written against
sdk/          the layer underneath: syscalls, MMIO and DMA (sdk/rust), and shared protocol crates
osdev/        build / test / run tooling
scripts/      the enforcement layer (the gates every build runs) and the board image builds
contracts/    the JSON schema a service contract is validated against
examples/     annotated, Commandment-grounded teaching examples (start at examples/README.md)
utilities/    the specification of every shell utility, one file each
tests/        identity, property, fuzz, stress, chaos suites
docs/         architecture notes and design docs
backlog/      open items, each with its evidence, what is ruled out and the next step
audits/       dated evidence: the unsafe audit, and the kernel, security and documentation audits
milestones/   what was achieved and when
boot/         per-board boot configuration (the Pis' config.txt)
nonfree/      vendor firmware the WiFi chips need, each with its licence and digest
website/      documentation site (mdBook; renders this repo's docs)
```

---

## Documentation site

**Live at [godspeedhq.github.io/godspeed-os](https://godspeedhq.github.io/godspeed-os/)**, with two
API references under `/api`:

- [**`godspeed`** - the standard library](https://godspeedhq.github.io/godspeed-os/api/godspeed/),
  imported as `gs`. **This is what you write a program against**: files, the network, IPC, capabilities,
  bounded waits and request/reply, typed records, a program that runs and ends (`gs::program`, `gs::app`),
  and, for drivers, `gs::driver`. Its API reference lists every module.
  [**Writing a program for GodspeedOS**](https://godspeedhq.github.io/godspeed-os/stdlib.html) is the
  place to start.
- [**`godspeed-sdk`** - the layer underneath](https://godspeedhq.github.io/godspeed-os/api/godspeed_sdk/),
  the raw syscall seam and the audited MMIO/DMA accessors. **Reach for it only if you are writing a
  driver** (CLAUDE.md §18.1); everything else is written on `gs`.

Two sections worth knowing about:
[**the services**](https://godspeedhq.github.io/godspeed-os/services.html) - what each one is, what it
may *not* do, and diagrams of how they reach each other over endpoints - and
[**the utilities**](https://godspeedhq.github.io/godspeed-os/utilities.html), every one of them, each
with its full specification.

The docs in this repo also render as a browsable site built with
[mdBook](https://rust-lang.github.io/mdBook/). The site is a
**derived view**: most pages are an `{{#include}}` of the real file, so they never duplicate or drift
from the source, and the few written for the site are checked against the repository by
`scripts/site_check.py`. Acronyms link to the glossary and carry a hover definition, injected at build
time so the markdown stays plain. A GitHub Action rebuilds and republishes it on every push to `main`
that touches a doc, so editing `CLAUDE.md` or a `docs/` file updates the site automatically.

Preview or update it locally:

```bash
cargo install mdbook          # one-time
cd website
mdbook serve --open           # live-reload preview at http://localhost:3000
```

`bash website/sync.sh` builds the whole site as the Action does, the API references included. How the
includes work, how the gallery screenshots are captured, and the launch checklist are in
[`website/README.md`](website/README.md).

---

## Design reference

The full specification (capability model, IPC semantics, scheduler rules, memory enforcement, bootstrap sequence, unsafe policy, and constitutional invariants) is in `CLAUDE.md`. Its human-readable distillation is **[`COMMANDMENTS.md`](COMMANDMENTS.md)**: ten laws that bound every design choice.

The system is defined there first. The implementation exists to satisfy it.
