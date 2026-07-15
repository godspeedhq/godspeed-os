# GLOSSARY.md
## Abbreviations and Acronyms of GodspeedOS

This file expands the abbreviations and acronyms that appear across the GodspeedOS
codebase, specification, and docs, with a one-line meaning grounded in how the project
actually uses each one.

It is deliberately scoped to **abbreviations and short forms**. For architectural
**terms** (capability, endpoint, generation, placement, routing table, supervisor, name
directory, delegated resource capability, TCB, and so on) the authority is the
**Glossary in `CLAUDE.md` section 24** - read that for the prose definitions of the model
itself. Where this file and `CLAUDE.md` disagree, `CLAUDE.md` wins.

Only abbreviations the repository genuinely uses are listed. Each was verified by grep
before inclusion.

---

## Alphabetical list

| Abbreviation | Expansion | Meaning in GodspeedOS |
|--------------|-----------|------------------------|
| ABI | Application Binary Interface | The syscall ABI follows System V AMD64; it is part of the SDK's audited unsafe boundary (section 18.1) and any freestanding-ELF language can call it (Appendix B.2). |
| ACPI | Advanced Configuration and Power Interface | Firmware tables; the kernel walks RSDP then RSDT/XSDT to the IVRS for IOMMU detection (H1). The kernel does *not* parse ACPI/MADT for SMP topology - Limine supplies that. |
| AHCI | Advanced Host Controller Interface | The SATA block-driver backend used on real hardware (the T630 SSD is AHCI-only); MMIO plus DMA, with command list / FIS / PRDT (`docs/ahci.md`). |
| AMD-Vi | AMD Virtualization (AMD's IOMMU) | The IOMMU implementation behind H1 DMA-confinement; a per-device translation domain confines each DMA-capable driver to its granted arena. |
| AP | Application Processor | Any core other than the BSP; brought up via a real-mode trampoline (`arch/x86_64/ap_boot.rs`), then jumps to long mode and enters the idle scheduler loop. |
| API | Application Programming Interface | General term for a service or supervisor call surface (e.g. the supervisor restart API, section 14.4). |
| APIC | Advanced Programmable Interrupt Controller | x86 interrupt controller; Limine supplies APIC IDs of all cores so the kernel needs no MADT probe. |
| ATA | AT Attachment | Legacy disk interface; the ATA PIO (no-DMA, least-privilege) block backend works under QEMU's legacy IDE, superseded by AHCI for real hardware. |
| BAR | Base Address Register | PCI register locating a device's MMIO region; read during controller enumeration. |
| BDF | Bus / Device / Function | A PCI device's address; the IOMMU confines a specific BDF to its DMA arena (boot prints `confined BDF ...`, section 6.4). |
| BIOS | Basic Input/Output System | Legacy firmware boot path; Limine supports both BIOS (via MBR stage-1) and UEFI from one bootloader. |
| BOOTX64.EFI | (UEFI boot executable name) | The Limine UEFI loader binary placed at `/EFI/BOOT/BOOTX64.EFI` on the ESP; firmware loads it, it loads Limine, which loads the kernel (Appendix A.4). |
| BSP | Bootstrap Processor | The first core to execute kernel code; performs kernel init and brings the APs online. |
| CAS | Compare-And-Swap | Scheduler Running-to-Ready transitions use CAS so a cross-core kill's Dead state is preserved (not clobbered by a plain store). |
| CLI | Command-Line Interface | `osdev`, the host-side CLI that builds, runs, publishes, and controls the OS in QEMU (section 17). |
| CMOS | Complementary Metal-Oxide-Semiconductor | The MC146818 CMOS RTC chip the kernel reads for wall-clock date/time (`arch/x86_64/rtc.rs`). |
| COM1 / COM2 | Serial communication port 1 / 2 | COM1 carries the interactive shell console; COM2 is the control/harness channel used by `osdev` tests. |
| CRC32 | 32-bit Cyclic Redundancy Check | Checksum used for disk-image and GPT integrity (`osdev/src/crc32.rs`). |
| DMA | Direct Memory Access | Device-driven memory access; the unstated exception to "no ambient authority" (invariant 1) because a DMA engine can reach any physical address. Closed by IOMMU confinement (H1, section 6.4). |
| DoS | Denial of Service | A failure mode the design guards against - e.g. the supervisor respawn is unbounded *deliberately* so killing it N times cannot force a reboot (section 6.2). |
| EHCI | Enhanced Host Controller Interface | The USB 2.0 controller driver; runs in transparent IOMMU passthrough (it legitimately reaches firmware/hub regions, section 6.4). |
| ELF | Executable and Linkable Format | Service binary format; the spawner maps an ELF into a new address space; bit-flip-mutated ELFs are a fuzz surface (F3). |
| EOT | End Of Transmission | The `0x04` marker that ends one stream in a capability-mediated pipe (`docs/pipes.md`, Appendix D.3). |
| ESP | EFI System Partition | The FAT boot partition holding `BOOTX64.EFI` and the kernel; the boot region of a bootable GodspeedOS drive. |
| FAT / FAT32 | File Allocation Table (32-bit) | The filesystem of the ESP boot region (Limine can read FAT but not GSFS); also the size-class GSFS sits in today because Phase-1 chose 32-bit fields. |
| FIS | Frame Information Structure | An AHCI command/data frame in the SATA transport. |
| FSF | Free Software Foundation | Steward of the GPL; the kernel is GPL-2.0-only, "the same license as the Linux kernel" (`LICENSE`, `docs/licensing.md`). |
| GDT | Global Descriptor Table | x86 segment descriptor table set up during BSP init. |
| GPL | GNU General Public License | The kernel license (GPL-2.0-only copyleft); the SDK is permissive Apache-2.0, with the capability/IPC boundary as the license boundary. |
| GPT | GUID Partition Table | The partitioning scheme of `build/os.img` (a UEFI GPT disk image). |
| GSFS | Godspeed File System | The project's own on-disk filesystem (magic `GSFS`); the kernel never learns what a file is, `fs` owns it. `GSFS0008` is the 8th on-disk format version (superblock + backup, free bitmap, self-describing file-record tree, crash-consistent redo-journal). |
| gsh | Godspeed shell (language) | The `.gsh` shell scripting language (design sketch, `docs/scripting.md`); `no_std`, no heap, fixed/bounded storage by design. |
| HHDM | Higher-Half Direct Map | Limine pre-maps physical memory at a known high virtual address; the kernel uses the HHDM to read ACPI tables and physical frames. |
| HID | Human Interface Device | The USB keyboard/mouse device class; the xHCI and EHCI drivers share `sdk/rust/src/hid.rs`. |
| IDT | Interrupt Descriptor Table | x86 interrupt-vector table installed at BSP init. |
| IF | Interrupt Flag | x86 flag enabling interrupts; the supervisor respawn must run in the scheduler loop (IF=1), not inside a timer ISR (IF=0), so it can ACK TLB-shootdown IPIs. |
| IOMMU | Input/Output Memory Management Unit | Translates every device DMA through a per-device page table; a domain mapping only the driver's arena confines a compromised driver and lets it leave the TCB (H1, `docs/iommu.md`). |
| IPC | Inter-Process Communication | Synchronous, bounded-queue message passing between services; the core kernel primitive (section 8). |
| IPI | Inter-Processor Interrupt | One core signaling another - used to wake a blocked cross-core `recv` and to drive TLB shootdowns. |
| IRQ | Interrupt Request | A hardware interrupt line; the kernel routes IRQs to userspace driver endpoints via IPC (section 12). |
| ISR | Interrupt Service Routine | A kernel interrupt handler (e.g. the per-core timer ISR that enforces the quantum). |
| IVRS | I/O Virtualization Reporting Structure | The AMD ACPI table describing AMD-Vi; if there is no IVRS the machine has no IOMMU, so DMA drivers stay in the TCB (reported loudly at boot, section 6.4). |
| JSON | JavaScript Object Notation | The schema format (`contracts/schema/service.schema.json`) `osdev` uses to validate service contracts. |
| KVM | Kernel-based Virtual Machine | QEMU hardware acceleration, enabled by the harness when `/dev/kvm` is available. |
| LAPIC | Local APIC | The per-core APIC; its periodic timer drives the 10 ms preemption quantum. |
| LBA | Logical Block Address | A disk block index; the block-IPC LBA field is `u64` (`docs/persistence.md`). |
| MADT | Multiple APIC Description Table | The ACPI table that would enumerate cores; *not* parsed in v1 because Limine supplies APIC IDs directly. |
| MBR | Master Boot Record | BIOS boot sector; `limine bios-install` writes Limine's stage-1 there. |
| MMIO | Memory-Mapped I/O | Device registers accessed as memory; granted to a driver by an `hw_mmio` capability and used only through the SDK's safe `Mmio` wrappers (section 18.1). |
| MSI | Message-Signaled Interrupt | PCI interrupt delivery via a memory write; the current USB drivers use pin-based IRQs, with MSI the basis of the interrupt-driven driver branch. |
| NIC | Network Interface Controller | The network device in the (design-only) networking stack; an IOMMU-confined e1000 driver for QEMU/Intel (`docs/networking.md`). |
| PCI | Peripheral Component Interconnect | The device bus enumerated to find controllers (`arch/x86_64/pci.rs`). |
| PF | Page Fault | A protection violation; an access outside a mapped region kills the service (section 10.4), and a guard-page PF catches stack overflow. |
| PIO | Programmed I/O (port I/O) | The no-DMA ATA disk backend - least-privilege because it cannot reach arbitrary memory the way a DMA engine can. |
| PRDT | Physical Region Descriptor Table | The AHCI scatter-gather list describing DMA buffers for a command. |
| QEMU | Quick Emulator | The virtual machine `osdev run` / `osdev test` boots the OS in. |
| RCU | Read-Copy-Update | A candidate v2 replacement for the v1 global capability-table `RwLock` (requires benchmarks first, section 7.8). |
| RTC | Real-Time Clock | The CMOS battery-backed clock read for uptime and the `date` command; uptime uses `rtc::now_epoch_monotonic` to deglitch. |
| RwLock | Read-Write Lock | The single global lock guarding the capability table in v1 (a known, benchmarked perf cost; section 7.8, benchmark B7). |
| RX / TX | Receive / Transmit | Serial (and network) data directions (e.g. polling UART RX in the timer ISR). |
| SATA | Serial ATA | The disk interface driven by the AHCI block backend. |
| SDK | Software Development Kit | `sdk/rust/` - the typed capability/IPC/MMIO/DMA wrappers services build against; the only place outside the kernel's four layers where `unsafe` is permitted (section 18.1). |
| SemVer | Semantic Versioning | The versioning scheme for contracts and the contract schema (breaking changes need a major bump). |
| SMP | Symmetric Multi-Processing | Multi-core support with *static* placement and no migration - cores discovered at boot, services pinned for life (section 9). |
| SPDX | Software Package Data Exchange | Per-file `SPDX-License-Identifier` tags mark each license zone (kernel GPL-2.0-only vs SDK Apache-2.0). |
| TCB | Trusted Computing Base | The set of components trusted to enforce isolation; deliberately shrunk over time until the non-restartable floor is `{kernel}` alone (sections 6.1-6.3) - DMA drivers excepted only on a machine with no IOMMU. |
| TCG | Tiny Code Generator | QEMU's pure-software (non-KVM) JIT; cross-core wake timing differs under TCG, so some tests poll rather than block-wait. |
| TLB | Translation Lookaside Buffer | On unmap (service death, memory reclaim) the kernel issues a TLB shootdown via IPI to every core and resumes only after all acknowledge (section 10.5). |
| TOCTOU | Time-Of-Check-To-Time-Of-Use | The send-during-restart race; caught atomically by the generation check inside the send syscall (section 8.7, adversarial test A5). |
| TOML | Tom's Obvious Minimal Language | The format of a service contract (`contracts/<name>.toml`), validated structurally against the JSON Schema. |
| TSC | Time-Stamp Counter | The CPU cycle counter (`read_tsc`) used for the performance benchmarks; note `CORE_TOTAL_TICKS` counts scheduler quanta, not time. |
| UART | Universal Asynchronous Receiver/Transmitter | The serial-port hardware carrying the shell (COM1, 115200 8N1). |
| UB | Undefined Behavior | What unsound `unsafe` would produce; the spec's rule is "undefined behavior in spec becomes bugs in system" (section 25). |
| UAF | Use-After-Free | A class of memory bug guarded against in the kill-path reclaim walk (a defensive PTE-validity guard skips and logs corrupt entries). |
| UEFI | Unified Extensible Firmware Interface | The primary firmware boot path: firmware loads `BOOTX64.EFI`, then Limine, then the kernel. |
| USB | Universal Serial Bus | Keyboard and mouse input, driven by the userspace xHCI and EHCI services. |
| W^X | Write XOR Execute | A page-table hardening invariant - no page is both writable and executable (the H4 kstack-guard / W^X work, section 18.5). |
| xHCI | eXtensible Host Controller Interface | The USB 3.0 controller driver; IOMMU-confined to its DMA arena (section 6.4, identity test 12). |

---

## Naming schemes

A few families of identifiers are easier to understand as a scheme than as a list.
Rather than enumerate every one, here is what each prefix means.

### Amendment and program IDs (in `CLAUDE.md`)

- **H-prefix (H1, H4, H11, ...)** - numbered **hardening / security amendments** to the
  constitution. Each closes a specific gap with a written rationale. H1 = IOMMU
  DMA-confinement (closes the DMA hole in invariant 1); H4 = kstack-guard / W^X
  hardening; H11 = making `registry` a restartable userspace service (it left the TCB).
- **P-prefix (P2)** - a **feature/persistence amendment**. P2 = file-as-capability
  (delegated resource capabilities, section 7.10). (Note: `P1`-`P10` separately label the
  **property tests** - see below; context disambiguates.)
- **"Path C"** - the chosen end-state of the naming-out-of-kernel redesign
  (`docs/naming-design.md` section 3.7): name policy lives in the supervisor while the
  kernel keeps only a minimal gated recovery directory.
- **Phases (Phase 4, Phase 5, Phase 6, Phase D, ...)** - the ordered implementation steps
  *within* an amendment program. For example, in Path C: Phase 4 retires the registry
  service, Phase 5 removes `init`, Phase 6 makes the supervisor restartable; the "Phase D"
  amendment dropped `block-driver` and `fs` from the TCB once `fs` gained crash-consistent
  recovery.

### Test-suite IDs (`CLAUDE.md` section 22, `tests/qemu/`)

The identity suite numbers its constitutional tests (Test 1 ... Test 15). The
battle-hardening categories use a letter prefix plus a number:

- **(Identity)** - Tests 1-15, the executable constitution; a failure is a constitutional
  violation.
- **P** - **Property** tests (P1-P10): universal invariants under randomized inputs.
- **F** - **Fuzz** tests (F1-F8): the kernel must never panic on user-controllable input.
- **S** - **Stress** tests (S1-S10): no drift, leak, or corruption under sustained load.
- **B** - **Performance Benchmarks** (B1-B10): latency/throughput numbers tracked against a
  baseline.
- **BP** - the **Brutal Perf** variant of the benchmarks (BP1-BP10): the same metrics run
  in a contended "perf-brutal" build, the form quoted in the hardware results table.
- **A** - **Adversarial / red-team** tests (A1-A10): capability isolation under direct attack.
- **C** - **Chaos** tests (C1-C7): graceful degradation under partial failures.
- **"brutal"** - in general, the harsher contended variant of a suite (e.g. perf-brutal).

### The name directory

Not an abbreviation but worth stating once: the **name directory** is the kernel's
minimal `name -> EndpointId` map (`ipc::names`) plus a gated "mint a SEND cap by name"
call (`AcquireSendCap`). It is the bounded recovery anchor that **replaced the retired
`registry` service** (Path C / Phase 4): the supervisor wires services from a `name -> cap`
map, and clients reacquire a service by name through this directory after a restart.
