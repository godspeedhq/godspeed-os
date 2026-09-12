# kernel/src/arch/riscv64/ (RV64GC, S-mode - QEMU `virt` and StarFive VisionFive 2 Lite / JH7110)

The fourth complete port, and the first with **zero mentions of the ISA in architecture-neutral
kernel code**. Everything below the `arch::imp` seam is here; nothing above it names RISC-V.

**Hardware-verified** on a StarFive VisionFive 2 Lite (JH7110, 4 usable harts): first boot 2026-09-07,
and a 22,872-round `chaos max-carnage` soak absorbing 136,906 service kills with zero kernel panics
and zero liveness wedges.

## Two machines, and they differ in the one place that bites

| | QEMU `virt` | VisionFive 2 Lite |
|---|---|---|
| kernel load address | `0x8020_0000` | `0x4020_0000` (`visionfive` feature + its linker script) |
| harts | `0..3` | **`1..4`** - hart 0 is disabled in the device tree |
| PCI | `pci-host-ecam-generic`, walkable | a StarFive vendor bridge this port does not read |
| NIC | e1000 on PCI (`--net`) | DesignWare `dwmac` on the SoC |
| USB | none | xHCI at MMIO `0x1011_0000`, a PLATFORM device with no BDF |

**Building one and booting the other silently produces nothing.** A kernel linked at `0x4020_0000`
started under QEMU `virt` stops after the OpenSBI banner with no output of its own, because the same
`target/` directory serves both and the last build wins. If a QEMU run dies at OpenSBI, check the link
address before debugging anything else.

## Boot flow

```
BootROM -> U-Boot SPL -> OpenSBI (M-mode) + U-Boot -> kernel (S-MODE)
   a0 = hart id      a1 = device-tree pointer
```

Only the boot hart arrives. The rest are parked by OpenSBI and started through **SBI HSM**
(`sbi::hart_start`, `sbi.rs`). `--features riscv-single-hart` clamps discovery to one hart, which is
the control for deciding whether a bug is a race: **a bug that survives on one hart is not a race.**

On the board the image must be a **flat binary with a RISC-V Image header**, not an ELF - `booti`
loads an image. The jump at the head of that header is wrapped in `.option norvc`, because compressed
instructions make `j` two bytes, every field behind it shifts, and the magic lands where U-Boot does
not look. Deployment is `scripts/deploy_visionfive.ps1`; the card layout is `boot/visionfive/README.md`.

## The things that cost real debugging time

**`fence.i` is HART-LOCAL.** Code arrives as *data* (the loader writes bytes), and a store is invisible
to instruction fetch until `fence.i` on **the hart that will execute it**. The loader publishing to
itself publishes to nobody. Signature of getting this wrong: the same `sepc`/`stval` every time, but
only on some harts, and the `sepc` disassembles mid-instruction - the bytes fetched are not the file's.
Only respawns fail; boot spawns get fresh frames. Twin of arm32's `publish_user_pages_to_other_cores`.

**`tp` is two things.** The kernel's hart id AND the userspace thread pointer. The trap frame therefore
parks the kernel's copy at `OFF_HARTID` inside `FRAME_BYTES` (288), and the prologue reloads it **only
on the user path** - reloading it unconditionally corrupts a kernel-mode `tp`.

**`sscratch` is the kernel-stack latch.** It holds this hart's kernel stack pointer while U-mode runs
and **zero** while the kernel runs, which is what lets a trap handler find a stack before it has one.
It is the single register a trap may use before anything is set up; treat it as load-bearing.

**DMA coherence is PER MASTER on the JH7110, and the device tree does not say so.** The display
controller is not coherent; the USB controller is. Silence in the DT is not a claim either way - this
was established by observation, not by reading.

**The kill-path quiesce is PCI-shaped and does nothing here.** The kernel stops a dying driver's
controller by clearing PCI Bus-Master-Enable, and this board's xHCI is a platform device with no BDF,
so `task_hw_bdf` returns `0xFFFF` and nothing stops it. A respawned driver therefore meets a **running**
controller. That is why `xhci` must halt the controller before resetting it and allow a realistic
`HCRST` budget - see `services/xhci` and `backlog/14`.

**No generic PCI host bridge.** `ECAM_BASE == 0` means *we found no `pci-host-ecam-generic` node*, NOT
that the machine has no PCI. The VisionFive has PCIe (U-Boot probes `starfive_pcie pcie@2C000000`);
this port simply does not read that binding, so `hw-enumerator` honestly reports zero devices.

## Memory model (RVWMO) and the SMP obligations

RVWMO is **weaker than x86-TSO** and comparable to ARMv8. Everything the SMP-port contract in
`kernel/src/arch/CLAUDE.md` asks for applies here without exception: the scheduler's cross-core
handshakes need real barriers rather than relying on store order, and the `Ordering` on a shared
atomic is load-bearing rather than decorative. An allocator bug that hid on x86 for the life of the
project surfaced here within a single chaos run.

Paging is **Sv39** - three levels, 39-bit virtual addresses, 4 KiB pages (`sv39.rs`). No `Svpbmt` and
no `Zicbom` on this part, so memory types cannot be set per page-table-entry and cache maintenance is
not available through those extensions; the DMA arena is handled accordingly. Address-space switches
fence on **every** switch, not only when the `satp` value changes.

## Files

| file | what it owns |
|---|---|
| `mod.rs` | boot, the `arch::imp` surface, device discovery, PCI/ECAM, per-hart state |
| `trap.rs` | the trap vector, `TrapFrame`, fault reporting, the double-fault guard |
| `sv39.rs` | page tables |
| `context_switch.rs` | the switch, split Pi-style: Rust installs `satp`, naked asm swaps registers |
| `usermode.rs`, `uaccess.rs`, `syscall.rs` | the U-mode boundary |
| `sbi.rs` | SBI calls, including HSM hart start |
| `fdt.rs` | the device-tree reader |
| `display.rs`, `net.rs`, `usb.rs` | board device discovery handed to userspace services |

## Scripts

```
python scripts/riscv_build.py --release [--visionfive] [--features riscv-single-hart]
python scripts/riscv_run.py --release --smp 4 [--net] [--cmd "..."] [--settle N] [--after N]
```

`--settle` defaults to **8 seconds** after each typed command, which kills a chaos storm almost
immediately; use `--after` to keep capturing. `riscv_run.py` waits for the `gsh>` prompt rather than
for a service, because with no disk the prompt is roughly two minutes behind the supervisor and
keystrokes typed into that gap are discarded silently.
