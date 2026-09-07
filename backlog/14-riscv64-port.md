# 14. RISC-V 64 port - what exists, what it runs on, and what is next

**Severity:** feature, in progress. The target board is a StarFive **VisionFive 2** class machine
(JH7110); QEMU `virt` is the primary development target and will remain so for the early work.
**Status:** 2026-09-07 - the kernel BUILDS for `riscv64imac-unknown-none-elf` and BOOTS under QEMU
`virt`, reaching S-mode and driving the 16550 UART. Everything above that is stubbed.

## What was actually wrong when this started

`kernel/src/arch/riscv64/` had been in the tree for some time and had **rotted: 35 compile errors**,
every one a seam member the neutral kernel had grown since the stub was written. Nothing ever built
this target, so the boundary was doing its job and telling nobody.

That is the same failure as a test that only runs when named, and this project had just been bitten by
it twice in one week (`BC1` asserting a string the kernel stopped printing; the x86 build path never
running `arch_boundary_check.py`). The fix was therefore not only to repair the stub:

- **`scripts/arch_seam_check.py`** (new) verifies that EVERY arch answers every `arch::imp` member the
  neutral kernel actually calls. The seam is discovered from usage, never from a hand-kept list,
  because a list drifts from the code it describes - which is the failure being fixed. It costs a
  grep and needs no toolchain, so it can run on every build path, which is the property that decides
  whether a check protects anything.
- **`scripts/riscv_build.py` / `scripts/riscv_run.py`** (new) give the port a repeatable path, with
  the same gates every other build path runs.
- **`osdev` now runs the full checker set.** It gated on `commandments.py` alone while the ARM paths
  gated on six; that is how an `arch::x86_64::` reference from the neutral `smp/` layer got committed
  from the x86 path and was refused days later, by accident, by an ARM build.

## The boot contract (QEMU `virt` and the real JH7110 agree on this)

```
BootROM -> U-Boot SPL -> OpenSBI (M-mode) + U-Boot -> kernel (S-MODE)
```

- Entry at **`0x8020_0000`**, in **S-mode**, with **`a0` = hart id** and **`a1` = device-tree pointer**.
- Only the boot hart arrives; OpenSBI parks the rest and starts them through **SBI HSM**.
- The kernel runs UNDER OpenSBI. Timers, IPIs and hart start/stop are **SBI calls**, not direct
  hardware pokes - a real difference from x86 ring 0 and from the Pi's EL1-with-everything.
- **The device tree is not optional.** On the Pi we hard-coded peripheral bases; here the machine
  describes itself, and the UART, PLIC, CLINT and memory map all come from the FDT in `a1`.

## MEASURED ON THE REAL BOARD (2026-09-07, VisionFive 2 Lite, full boot to a Debian login)

Read off the board's own serial log and its vendor image, not from documentation. Where QEMU `virt`
and this board differ, the difference is the interesting column - each one is a place where code that
works in QEMU is silently wrong on hardware.

| | QEMU `virt` | VisionFive 2 Lite | |
|---|---|---|---|
| UART | 0x1000_0000, 16550 | **0x1000_0000, 16550A, IRQ 44** | MATCHES - our hard-coded address is right |
| Kernel entry | 0x8020_0000 | **0x4020_0000** | we link at the wrong address |
| RAM base | 0x8000_0000 | 0x4000_0000 | |
| Boot hart | 0 | **1** | hart 0 is the S7 monitor core; the U74s are 1-4 |
| Hart count | 1 (as run) | 5 | |
| Timer | 10 MHz | **4 MHz** (aclint-mtimer) | cannot be a constant |
| OpenSBI | v1.8.1 | v1.2, SBI 1.0 | older than QEMU's - do not assume new SBI calls |
| FDT (`a1`) | QEMU-supplied | **0x4220_0000** | |

**The dangerous one is the boot hart.** `Boot HART ID: 1`. Code that assumes "hart 0 is the boot
hart" is natural, and QEMU never punishes it because its boot hart IS 0. That is the same shape as
the x86 LAPIC-id bug: an environment where the wrong value happens to be right. Take the hart id from
`a0`, never from an assumption.

The lucky one is the UART: the JH7110 puts UART0 where QEMU does, and it is the same 16550 family, so
the existing banner will print on hardware unchanged. That was the main risk and it evaporated.

## How a kernel is actually loaded on this board

From the vendor image's own ESP (partition 3, FAT16, 100 MB - readable from Windows):

```
/extlinux/extlinux.conf     label -> linux /vmlinuz-...  initrd ...  fdtdir /dtbs/...
/uEnv_Lite.txt              kernel_addr_r=0x40200000   fdt_addr_r=0x46000000
                            fdtfile=starfive/jh7110s-starfive-visionfive-2-lite.dtb
/dtbs/6.12.5-starfive/...   45 device trees + overlays
```

So GodspeedOS goes on as **a file copy onto a FAT partition** plus an `extlinux.conf` label - no card
rewrite, no Linux host needed. `kernel_addr_r=0x40200000` independently confirms the load address.

## Putting it on the board

    py scripts/riscv_build.py --release --visionfive   ->  build/godspeed-riscv64-visionfive.img

A flat binary linked at 0x4020_0000. `booti` loads an image, not an ELF; the ELF only works for
QEMU's `-kernel` because QEMU parses it.

**Fastest first light - the U-Boot prompt, no card edits.** Interrupt autoboot, then load and jump.
This is the loop worth using while the kernel is one banner long: seconds per try, and a mistake
costs nothing.

```
=> fatload mmc 0:3 0x40200000 godspeed-riscv64-visionfive.img
=> booti 0x40200000 - ${fdt_addr_r}
```

**Or persistently**, by adding a label to `/extlinux/extlinux.conf` on the ESP (partition 3, FAT16,
writable from Windows once `diskpart` assigns it a letter - Windows hides EFI System Partitions from
Explorer by default, which is why no drive letter appears):

```
label godspeed
        menu label GodspeedOS riscv64
        linux /godspeed-riscv64-visionfive.img
        fdtdir /dtbs/6.12.5-starfive
```

Leave `default l0` alone so a power cycle still lands in Debian. Do not make GodspeedOS the default
until it does something worth booting into.

**What to expect on success:** the banner and a halt. That is the entire kernel today. The value is
that it proves the chain end to end on real silicon - link address, flat image, U-Boot handoff,
S-mode entry, and the UART - which is exactly the set of assumptions that cannot be tested in QEMU.

**If it is silent**, the load address is the first suspect, then the FDT argument. The banner writes
to 0x1000_0000 directly and does not depend on the FDT, so a silent board means it never reached
`_start` rather than that it failed later.

## What is stubbed, in the order it probably wants doing

1. **Read the FDT.** Everything else needs it, and it removes the last hard-coded address (the 16550
   the banner currently writes to).
2. **Sv39 paging.** Per-service address spaces are invariant 2; nothing above the kernel is real
   without them.
3. **S-mode trap vector** (`stvec`), so a fault kills a task instead of the machine.
4. **Timer** via SBI, then the scheduler quantum.
5. **PLIC**, for device interrupts routed to userspace services (§12).
6. **SMP** via SBI HSM - and note `publish_bsp_lapic_id` is a no-op here for a real reason: the hart
   id arrives in `a0` rather than being read back from an interrupt controller, so the x86 bug that
   function exists to prevent cannot occur in the same way.

## The scaffolds, and why they are not this

`riscv32`, `loongarch64` and `s390x` are scaffolds proving the seam generalises - nothing builds
them, no board is targeted. `arch_seam_check.py` names them and their drift count on every run rather
than failing on them, and removing a name from its `SCAFFOLDS` set is how a scaffold becomes a port.
`riscv32` is currently 12 members behind and would take the same mechanical repair this port just had.

## Hardware notes, for when the board is in front of us

- **The Pi mental model does not transfer.** There is no `config.txt` and no `kernel8.img`; U-Boot is
  a real bootloader with a shell, and the kernel is loaded and `booti`-ed.
- **Boot the vendor image first.** It proves card, boot-mode switches and serial wiring before
  GodspeedOS is a variable. Debugging three unknowns at once is how a weekend disappears.
- **Boot-mode DIP switch positions and partition GUIDs must come from StarFive's own docs** for the
  exact board revision. They are deliberately NOT recorded here from memory.
- **Serial:** the 40-pin header carries the console UART on the same physical pins as a Pi
  (GND/TX/RX at 6/8/10), 115200 8N1, so a Pi debug probe's UART side works. Its SWD side does not -
  that is an ARM debug protocol; RISC-V uses JTAG on different pins.
