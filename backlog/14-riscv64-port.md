# 14. RISC-V 64 port - what exists, what it runs on, and what is next

> **HARDWARE BOOT ACHIEVED 2026-09-07.** GodspeedOS runs on the StarFive VisionFive 2 Lite:
> `Starting kernel ...` from U-Boot, then our own banner out of the JH7110's 16550. Fourth
> architecture, real silicon.
>
> Three things had to be fixed, and two of them QEMU could not have found:
> 1. **Load address.** Linked at QEMU's 0x8020_0000; the board enters at 0x4020_0000
>    (`kernel-riscv64-visionfive.ld`, `visionfive` feature).
> 2. **Flat binary.** `booti` loads an image, not an ELF.
> 3. **RISC-V Image header.** `booti` refused the image outright - `Bad Linux RISCV Image magic!` -
>    after reading all 4279 bytes correctly. QEMU never asked because `-kernel` takes an ELF and
>    reads the entry from its header. A property of the BOOT PROTOCOL, not the silicon.
>    The jump at the head of it is wrapped in `.option norvc`: compressed instructions make `j` two
>    bytes, every field behind it shifts, and the magic lands where U-Boot does not look.
>
> Also learned on the card, not from documentation: **the bootloader is in the board's SPI flash**
> (`Trying to boot from SPI`), so an SD card needs only to carry a kernel. One FAT32 partition is
> enough - and it must sit in **MBR slot 3**, because this U-Boot's SPI-resident environment has
> `mmc 0:3` baked in from the vendor layout. Moving the 16-byte partition entry from slot 1 to slot 3
> is enough; no filesystem data moves.
>
> Next is the FDT parser: U-Boot already hands us a valid device tree at 0x4600_0000, and reading it
> is what ends the hard-coded UART address and unblocks Sv39, the trap vector and the timer.


> **SINGLE HART VERIFIED ON HARDWARE 2026-09-11.** `--features riscv-single-hart` clamps the hart
> count at DISCOVERY, so the percpu arenas, `ap_count` and the liveness watchdog are all sized for one
> core rather than sized for four and overridden later. On the board:
>
> ```
> riscv64: usable harts 1
> smp: no secondary harts started - running single-core
> smp: 1 core ready
> ```
>
> `smp: 1 core ready` is singular, and no line names core 1, 2 or 3 anywhere in 1.36 MB of serial.
> Sequence: selfcheck 461/0, chaos max-carnage 100 rounds absorbing 609 service kills, selfcheck
> 461/0, USB hot-plug, selfcheck 461/0. **0 kernel panics, 0 liveness wedges, 0 kernel faults.**
> Networking held throughout - DHCP was re-acquired DURING the chaos run, ping 8.8.8.8 returned 2/2
> at 0% loss and DNS resolved afterwards. 461 is the same count the multi-hart runs produce, so it is
> a full run rather than a truncated one.
>
> Deployment is now `scripts/deploy_visionfive.ps1` rather than steps retyped from memory. The card
> runs an official StarFive image; our kernel goes on its ESP (partition 3) and our label is APPENDED
> to the `extlinux.conf` already there, so Debian stays selectable. The script refuses to write unless
> the target really is that partition, verifies the kernel by SHA256 after copying, and parses the
> installed config back to confirm the Debian fallback survived. It needs elevation, because Windows
> hides an EFI System Partition and ACLs it to administrators.

> **STALE PEER CAPS - fixed 2026-09-11, verified in QEMU, NOT yet verified at soak scale.**
> A send cap to a peer that respawned stayed stale for the rest of the boot: `find_send_slot` answers
> from a cache only `reacquire_cap` writes, so a service that never explicitly reacquired kept
> resolving the same dead slot. Seven services never reacquired at all (`xhci`, `console`, `dwc2`,
> `ehci`, `events`, `hw-enumerator`, `observe`). On this board it presented as a dead USB keyboard and
> no shell prompt after a storm, with `xhci` reporting `1 HID, disk yes` while delivering `0 msg`.
>
> **Not a RISC-V bug and not new.** The signature is in every chaos log in `build/` going back to
> July, on every port. Counts of the serious `liveness=Alive` variant (peer respawned and running,
> client holding a cap to the previous incarnation): x86_64 Wyse 608, riscv64 660, aarch64 Pi 4 224,
> arm32 Pi 2 95. Nobody had read the line.
>
> Fixed in the SDK at the choke point - the four raw send syscalls all fifteen request/send helpers
> funnel through - so no helper can forget. It repairs the CACHE and deliberately does not retry: the
> failed send stays failed and is reported, because §14.3 is explicit that reacquiring is necessary but
> not sufficient, and replaying a stateful request into an instance that never issued the ids it
> references desyncs a protocol rather than recovering it.
>
> QEMU, 8-round max-carnage run to completion: 49 kills, 327 reacquisitions, 0 panics, 0 wedges, and
> **0 stale-cap lines after the storm ended**. Stale lines still appear DURING a storm, which is
> correct.
>
> **What is NOT established:** there is no same-environment before/after (the pre-fix QEMU attempt was
> truncated mid-storm, so the comparison crosses from a before-fix HARDWARE log to an after-fix QEMU
> one), and QEMU cannot reach soak scale - roughly 85 s per chaos round under TCG with 4 harts, so 100
> rounds is over two hours. The board's soak is what settles both.

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

## Confirmed on hardware, and one new trap

The parser, unchanged, on two machines:

| | QEMU `virt` | VisionFive 2 Lite |
|---|---|---|
| ram | 0x8000_0000 + 256 MiB | 0x4000_0000 + **8192 MiB** |
| usable harts | 1, highest id 0 | **4**, highest id 4 |
| timebase | 10 MHz | 4 MHz |
| uart | 0x1000_0000 shift 0 width 1 | 0x1000_0000 shift **2** width **4** |
| plic | 0xc00_0000 | 0xc00_0000 |

**8192 MiB settles the argument for parsing.** The DTB FILE in the vendor image declares
`/memory@40000000` as 4 GiB. U-Boot patches the node from what the SPL detected, so the runtime blob
says 8. Constants taken from that file, or from any datasheet, would have sized the machine at half
and been PLAUSIBLY wrong - the failure mode that survives review.

**4 usable harts rather than the 5 OpenSBI counts** is `status = "okay"` doing its job: hart 0 is the
S7 monitor core and the tree marks it `disabled`, so it is excluded with no board knowledge in the
kernel at all.

**NEW TRAP: `boot_cpuid_phys` IS WRONG ON THIS BOARD.** The FDT header field reads 0; `a0` says the
boot hart is 1, and OpenSBI's own banner agrees (`Boot HART ID : 1`). U-Boot appears not to update
the field when it hands the tree on. So:

> **Take the boot hart from `a0`. Never from the device tree header.**

That is the third member of one family now - the x86 BSP whose APIC id was assumed 0, "hart 0 is the
boot hart", and now a header field that says 0 while the register says 1. Each is a place where the
plausible value is zero and zero is what an unset field already contains. The kernel prints both, so
a disagreement is visible rather than latent.

## Verification reference: what the FDT parser must PRODUCE

**These are not constants to hard-code.** The whole point of the parser is that the kernel asks the
machine instead of being told which board it is on. They are recorded so a parser can be checked
against something, and because two independent sources agreeing is worth more than either alone: the
board's own Debian boot log, and the DTB shipped in the vendor image.

| fact | device tree | Linux on the board |
|------|-------------|--------------------|
| RAM base | `/memory@40000000` base 0x4000_0000 | `DRAM: 8 GiB` |
| PLIC | `/soc/interrupt-controller@c000000`, `sifive,plic-1.0.0`, 0x4000000 long | `riscv-plic: interrupt-controller@c000000: 136 interrupts, 9 contexts` |
| CLINT | `/soc/timer@2000000`, `sifive,clint0` | `clint: timer@2000000` (Linux then declines it) |
| UART0 | `/soc/serial@10000000`, `snps,dw-apb-uart`, reg-shift 2, io-width 4, irq 32 | `ttyS0 at MMIO 0x10000000 (irq = 44) is a 16550A` |
| timebase | `aclint-mtimer @ 4000000Hz` (OpenSBI) | `sched_clock: 64 bits at 4MHz, resolution 250ns` |
| harts | `cpu@0` = `sifive,s7`, **`status = disabled`**; `cpu@1..4` = `sifive,u74-mc` | `CPU with hartid=0 is not available`; `Brought up 4 CPUs` |

**The device tree already says not to use hart 0** - `status = "disabled"` on the S7 monitor core. So
"do not assume the boot hart" is not a special case to remember; it falls out of reading `status`
honestly. An arch that enumerates harts from the FDT gets the right answer without knowing it is a
JH7110.

**AND THE FILE ON DISK IS NOT THE TREE WE ARE GIVEN.** The DTB in the vendor image declares
`/memory@40000000` with size 0x1_0000_0000 (4 GiB); the board has 8 GiB and U-Boot says so
(`LPDDR4: 8G`). U-Boot PATCHES the memory node from what the SPL detected before passing it on. So a
parser that reads the runtime pointer in `a1` learns the truth, and one that trusts a DTB from disk
would size RAM at half the machine. This is the single strongest argument for parsing the FDT rather
than shipping constants, and it was found by comparing the two rather than by reasoning.

Also worth carrying forward: OpenSBI on this board reports `Boot HART ISA Extensions : none`, where
QEMU lists `sstc`. So the timer must go through an SBI call rather than the Sstc extension - another
place where the emulator offers a capability the hardware does not.

## Where the port actually is (2026-09-07, end of first day on hardware)

Everything below is verified on BOTH QEMU `virt` and the VisionFive 2 Lite, with the values differing
between them because they are read from the machine rather than compiled in.

| | QEMU `virt` | VisionFive 2 Lite |
|---|---|---|
| boot hart | 0 | **1** (hart 0 is a disabled S7 monitor core) |
| RAM | 0x8000_0000 + 256 MiB | 0x4000_0000 + **8192 MiB** |
| usable harts | 1 | **4** of the 5 OpenSBI counts |
| UART | ns16550a, shift 0, width 1 | snps,dw-apb-uart, shift **2**, width **4** |
| timebase | 10 MHz | **4 MHz** |
| SBI | v3.0 (OpenSBI 1.8) | **v1.0** (OpenSBI 1.2) |
| identity map | to 0xc000_0000 | to **0x2_4000_0000** |

**Neutral subsystems running unchanged:** the frame allocator (`memory::init`), per-core arenas
(`smp::percpu_init`), the capability table, and IPC routing. Their log lines are the neutral kernel's
own, not this arch's.

**Arch-side, working:** FDT parsing, a real `BootInfo` with firmware and device tree carved out, Sv39
paging (4 KiB pages plus 1 GiB identity leaves), an S-mode trap vector that names its faults, SBI, and
a 10 ms scheduler tick with full context save and resume.

**Still zero changes outside `kernel/src/arch/riscv64/`** - the neutral kernel, the services and the
SDK are untouched by this port.

## What QEMU could not have caught, and what that cost

Six differences so far, each invisible in the emulator by construction. They are listed together
because the pattern is more useful than any one of them: an emulator supplies DEFAULTS, and a default
that happens to match an assumption HIDES it rather than testing it.

1. **Load address.** QEMU 0x8020_0000, board 0x4020_0000. Linked wrong, jumped past entirely.
2. **The RISC-V Image header.** `booti` refuses a flat binary without it; QEMU `-kernel` takes an ELF
   and reads the entry from its header, so it never asks. A property of the BOOT PROTOCOL.
3. **The 16550 transmit FIFO.** Output stopped at exactly 16 characters, twice. QEMU accepts bytes as
   fast as they are written and has no FIFO to overrun.
4. **`reg-shift = 2`.** The DesignWare UART puts every register except offset 0 somewhere else - which
   is why THR worked and LSR would not have.
5. **`svadu`.** QEMU maintains the Accessed and Dirty bits in hardware; this board does not, so a leaf
   mapped with `A = 0` faults on first touch there and works perfectly in the emulator.
6. **`sscratch`'s reset value.** Not architecturally specified, and the firmware beneath runs on
   `mscratch`, so nothing has promised to leave it alone. The trap entry reads it to decide whether it
   is standing on a kernel stack, so a non-zero value at the first S-mode trap would build a frame at
   an address nobody chose. QEMU hands over a zeroed register - the assumption and the default agree,
   which is exactly the shape of the other five. Zeroed at `trap::init` rather than assumed.
   FOUND BY WRITING IT DOWN, not by a failure, which is the only cheap way any of these get found.

And one that is not QEMU's fault at all: the FDT header's `boot_cpuid_phys` reads 0 on this board while
`a0` and OpenSBI both say hart 1. Take the boot hart from the register, never from the tree.

## USERSPACE RUNS (2026-09-07, QEMU) - and what the board has NOT seen yet

`supervisor: ready`, nine services in the name-cap map, `fs` serving the file API, `net-stack`
serving its client API, the shell at a prompt. Two minutes: zero panics, zero wedges, zero faults,
ten spawns. Everything degrades where the hardware is absent - no AHCI disk, no e1000, no xHCI/EHCI -
and says so rather than hanging.

**Board-verified up to `usertask PASS` and the supervisor LOAD. Everything from `entering the
scheduler` onward has only run in QEMU.** That is the whole of the running userspace, and it is the
first thing to try in the morning.

Three bugs got it there, and the shape of each is worth more than the fix:

1. **`wait_for_interrupt` implemented its NAME, not its contract.** The neutral idle path's own
   comment says it "issues only `sti`" - its job is to UNMASK, not to halt. A `wfi` that does not
   unmask masks once and never unmasks: the timer stops and the machine dies with nothing to report.
   The order `wfi` then `csrs sstatus, SIE` is race-free, because `wfi` wakes on a pending interrupt
   regardless of `SIE`.
2. **A whole-gigapage clone destroyed what it was cloning for**, because the kernel's identity map
   and userspace overlap here (kernel from zero, services at 0x400000, both in gigapage 0).
3. **The clone read the LIVE root, not the kernel's.** A spawn is a syscall made by a task, so
   `finalize_service_address_space` cloned from the SUPERVISOR when the supervisor spawned anything -
   and the supervisor reaches the low gigabyte through a pointer table, which a leaf-copying clone
   skips. Every service it spawned got RAM but no UART.

**`qemu -d int` found the third one in a line after four rounds of reasoning got it wrong three
times.** `load_page_fault ... tval:0x0000000010000005`, repeating - the UART's LSR, in a loop,
silent because the thing that wanted to print was the fault handler. Reach for it earlier: a silent
hang with no output is exactly the case where the emulator can see what the kernel cannot say. The
fault reporter now prints the live PTE for any page fault, so the next one is a log line instead.

### Verified on the board (2026-09-08)

Everything above ran on the VisionFive 2 Lite, in two sittings, with ZERO faults both times:

- The full boot: `boot hart is 1`, five selftests, ten spawns, `supervisor: ready`, in about six
  seconds from the first byte and 236 ms from `entering the scheduler`.
- **The shell answers.** `about` typed at `gsh>` returned `Version 0.15.0 riscv64`. That closes the
  loop the port was opened for: the machine boots, runs services, and can be used.
- The DesignWare UART's RECEIVE path works at `reg-shift 2`. That was the one place the board could
  plausibly have differed from QEMU - the transmit side had proved the width logic on hardware, the
  receive side had not - and it did not differ.
- `xhci` spawn FAILS and says so: no PCI on this board, and the service is x86/aarch64-shaped.
  Expected, and it does not stop the boot.

### HARDWARE-VERIFIED, end to end (2026-09-08, VisionFive 2 Lite)

Everything below has now run on the board, not only in QEMU:

```
riscv64: usable harts 1 2 3 4
riscv64: boot hart is 1 (core 0)
smp: hart 2 ready as core 1 / hart 3 as core 2 / hart 4 as core 3
cores: 4
chaos max-carnage all-services 10:
  total: 10 rounds, 53 kills, 43 flooded, 10 mem-pressure, 10 spawns. kernel: alive.
  kernel: supervisor died   (x3, each respawned by the kernel)
  supervisor: adopted running events / fs / shell / nic-driver / net-stack
```

Zero kernel panics, zero wedges, zero kernel faults. The supervisor being killed and the KERNEL
respawning it - which then ADOPTS the still-running services instead of duplicating them - is
CLAUDE.md Test 15 holding here: the unkillable set is `{kernel}` alone on this ISA too.

**The hart ids mattered on the day.** QEMU numbers its harts 0..3, so inferring them from a count
works there by luck; this board numbers them 1..4 with hart 0 a disabled S7 monitor core. Reading the
ids from the device tree is what made the first four-core boot work rather than the second.

### SMP, and the three things this ISA does not hand you (2026-09-08)

Four harts, four cores, services scheduled across them. SBI HSM replaces the whole of x86's
real-mode trampoline: one firmware call releases a named hart at a named address. What it does NOT
do is give that hart a stack, an address space or a trap vector - it arrives exactly as the boot hart
did - so the AP entry repeats `_start`'s work, in the only order that survives: `satp`, then `stvec`,
then ready.

Each of these cost a boot, and each is a fact about RISC-V rather than about this kernel:

1. **SBI extension ids are ASCII-packed, and "HSM" is THREE characters** - `0x48534D`, not
   `0x48534D00`. The probe failed and the boot printed `firmware refused to start hart 1` three
   times, which is the loud-failure design earning its keep: a wrong constant produced a sentence.
2. **There is no S-mode register that says which hart you are.** `mhartid` is M-mode only. The id
   arrives in `a0` once and is lost unless parked per-hart; `tp` is where, and it is free because a
   kernel with no thread-local storage never touches it.
3. **A hart cannot look up which CORE it is, and the lookup lies rather than failing.**
   `lapic_to_core_id` matches only a core already marked READY - and a hart cannot mark itself ready
   until it knows which core it is. Circular, and the fall-through to 0 makes it SILENT: all four
   harts reported `ready as core 0`, four harts scribbled on one core's scheduler state, and the
   first service to start died of `CapError(CapInsufficientRights)` - a capability error caused by
   nothing to do with capabilities. ARM never meets this because MPIDR tells a core its own number.
   The assignment is now made by the starter and read by the started: told, not derived.

**And the IPI carries no vector.** An APIC interrupt says which of 256 things happened; SBI's
`send_ipi` says only "someone poked you". The vector travels out of band in a per-core pending MASK -
a mask and not a value, because two senders can arrive between one hart's poke and its handler, and
the second must not overwrite the first, which would lose a TLB shootdown and leave its initiator
spinning on an acknowledgement that never comes. `sip.SSIP` is cleared BEFORE the drain, so a vector
set in between costs a spurious wake rather than a lost one.

Hart IDS come from the device tree, never from a count: on this board they are 1..4 with hart 0 a
disabled S7 monitor core, so counting would try to start a different core design and skip hart 4.

### Storage: PCI works, the AHCI signature does not (2026-09-08)

PCI Express is enumerated through ECAM, the BARs are assigned, and the UNMODIFIED x86 AHCI driver
talks to a controller on RISC-V:

```
riscv64: pci ecam at 0x30000000, 2 device(s)
riscv64:   bdf 0x8 class 0x10601 0x8086:0x2922 bar0 0x40000000
block-driver: AHCI HBA v1.00 CAP=0xc0141f05 (6 ports, 32 cmd slots) GHC=0x80000000 PI=0x0000003f
block-driver: AHCI port 0: device present (DET=3) sig=0xffffffff
```

Reproduce with:

```
qemu-system-riscv64 -M virt ... -drive file=build/rvdisk.img,if=none,id=d0,format=raw \
  -device ahci,id=ahci0 -device ide-hd,drive=d0,bus=ahci0.0
```

**Where it stops, exactly.** `PxSIG` holds 0xFFFFFFFF until the device posts its initial D2H Register
FIS, and QEMU latches it only through its FIS-WRITE path - which requires `PxFB` programmed and `FRE`
enabled. This driver reads the signature to CHOOSE a port and programs the FIS area in `init_port`
afterwards, so the signature it needs cannot exist yet. Chicken and egg. On a PC the firmware reset
the port long before any of this ran, which is why the same code has always worked there.

A COMRESET does not help: it is the FIS AREA that is missing, not the reset. Tried and reverted.

**The correct fix is what Linux does: initialise the port fully - CLB, FB, FRE, ST - and read the
signature afterwards.** That is a restructure of a driver that is hardware-proven on x86, so it wants
doing deliberately with a way to test it on x86 too, not squeezed in for an emulator. And it serves
QEMU alone: this board has no SATA, so AHCI will never be its storage.

**And the board answered the PCI question on 2026-09-08:**

```
riscv64: no pci-host-ecam-generic in the device tree - no PCI
riscv64: cycle counter readable (rdcycle) - userspace cycle budgets mean what they say
```

So AHCI can never be this board's storage, and the ECAM work is QEMU-only in practice. The JH7110
DOES have PCIe controllers, but behind `starfive,jh7110-pcie` - a controller-specific binding with its
own reset, clock and link-training sequence - not the generic one Linux and this code match on. That
is a driver, not a property of the tree, and it is a separate project from storage.

The cycle counter is the happier half: `rdcycle` is readable on this board too, so the fix that made
cycle-denominated waits mean what they say is real on silicon and not an emulator convenience. That
was the uncertain one - `mcounteren.CY` under OpenSBI v1.2 could have gone either way, which is why it
is probed rather than assumed.

**What the board would need instead**, and neither is small:

- **SD/eMMC** is a HAZARD, not an option, for the same reason it is on the Pi: the card is the boot
  medium, GSFS's superblock lives at LBA 0 where the partition table is, and the ARM ports destroyed
  two boot cards learning it. If the VisionFive ever gets storage it must not be the boot card.
- **USB mass storage** needs a USB host driver for the JH7110's controller, which is a project of its
  own - the same one the Pi ports each spent weeks on.

So storage stays absent on this port, and `fs` comes up storage-unavailable, which is exactly what x86
reports with no disk attached and what ARM reports with no stick in. The file half of `selfcheck`
cannot pass here; the single failing assert is that, and it is not a defect.

**What DID come out of the attempt, and is kept:** PCI ECAM enumeration (the only way any PCIe device
on any RISC-V board will ever be found), BAR assignment, and a `read_cycle_counter` that returns
CYCLES. That last one was invisible until a driver's cycle budget met it: 400 million cycles is a
fifth of a second on a PC and forty seconds against a 10 MHz timebase, so the driver appeared to hang
while being entirely correct. Every cycle-denominated wait in userspace was a hundred times out on
this port and nothing had noticed, because nothing had waited on one yet.

### What is still stubbed, now that the shape is clear

- **STORAGE, on any RISC-V machine.** `block-driver` looks for an AHCI controller; QEMU `virt` offers
  virtio-blk and the VisionFive has SD/eMMC, and neither has a backend. So `fs` comes up, serves its
  API and reports zero sectors - honest, and it means the FILE half of `selfcheck` cannot pass
  anywhere on this port yet. The one assert that failed in QEMU (`assert contains done`, section 9)
  is exactly this and is not a defect: 47 of 48 passed, and the suite's own banner says it needs a
  flashed drive. `selfcheck` also does not COMPLETE under TCG - too slow with ten services - so the
  full run belongs on the board.
- **PLIC.** No device interrupts are routed to userspace, so no driver service can be interrupt-driven
  (12).
- **The idle tick is deliberately NOT slowed** (`boot::rearm_idle_timer` re-arms at the quantum, not
  at ~1 s). With no PLIC there is no RX interrupt, so the timer tick is the only thing that drains the
  UART and wakes a shell blocked in `ConsoleRead` - which makes the idle tick the keystroke latency. A
  second between key and echo is not a slow system, it is a broken one. This is the first thing to
  revert when the PLIC lands, and it is the reason to want it.
- **TLB shootdown across cores is UNPROVEN.** The IPI path delivers the vector and the neutral
  handler runs, but nothing has yet forced a cross-core shootdown and watched it acknowledge. The
  neutral kill path elides it for a pinned task, and `switch_context` fences on every address-space
  change, so the pinned model is covered - what is untested is an unmap that broadcasts. Worth
  forcing deliberately rather than waiting to meet it.

## The kernel is INSIDE the user address range on this port, so a range check proves nothing

x86 validates a syscall pointer with a range check against `USER_END`, and that check rejects a
kernel address. It is easy to read that as the check doing the work. It is not: x86's kernel lives
higher-half, so the rejection is a property of the LAYOUT, and it does not travel with the code.

This kernel is IDENTITY-MAPPED LOW - 0x8020_0000 on QEMU, 0x4020_0000 on the board - which is
squarely inside the user half. So `validate_user_ptr(&__kernel_start, 8)` answers **true**, and is
right to: that is a perfectly legal user virtual address, and a task may legitimately have its own
page mapped there in its own space. Found on 2026-09-07 by asserting the x86 property and watching
`deny-kernel-ptr=BAD`.

**The `U` bit is the entire boundary here.** Every copy in `arch/riscv64/uaccess.rs` therefore walks
the live page table and refuses a page without `U`, and the selftest asserts BOTH halves - the range
check passes for the kernel's own address, and the read is refused anyway - so a future change that
made the range check reject it cannot silently turn that claim into a test of nothing.

Both halves of the walk were then proved load-bearing by deleting them:

- Drop the `U` check: `deny-kernel-ptr=BAD`. The kernel reads its own memory on behalf of a user
  pointer, which is the shape of a privilege escalation rather than a crash.
- Drop the `W` check: the kernel FAULTS - `store/AMO page fault` at the task's read-only code page -
  and halts, because there is no kill path yet. Which is exactly why the walk happens BEFORE the copy
  on this port rather than the fault being caught during it, as x86 does.

## `sstatus.SUM` makes a missing `sscratch` latch a SILENT HANG, not a wrong answer

Two isolation rules meet here, and the combination is worth knowing before it is met by accident.

`sscratch` holds the kernel stack while U-mode runs, so a trap from user mode builds its frame on a
kernel stack rather than on whatever the user left in `sp` (`arch/riscv64/trap.rs`). Remove that latch
and the frame goes on the USER stack instead - which sounds like a correctness bug with a wrong value
at the end of it.

It is not. A user stack is mapped `U`, and **S-mode may not write a `U` page while `sstatus.SUM` is
clear** - which it is, by default, and this port never sets it. So the FIRST STORE of the trap entry
faults. That fault re-enters the trap entry, which stores again, and faults again. An unrecoverable
loop, before any handler runs, before `REPORTING` is consulted, before a single character is emitted.

Confirmed on 2026-09-07 by deleting `csrw sscratch, sp` from `user_entry_trampoline`: the boot stops
at the user-task selftest with NO output at all - not a `BAD`, not a trap report, nothing. The
prediction had been `own-kernel-stack=BAD`.

Two consequences:

- **No software check can catch a MISSING latch.** The machine is gone before any code observes it.
  What the `own-kernel-stack` check is actually for is a latch pointing at the WRONG stack, which does
  not fault and would otherwise surface much later as one task quietly corrupting another's.
- **`SUM` will have to be set, deliberately and narrowly, when the kernel first reads a user pointer**
  (`uaccess::read_user_bytes` is the seam member that will need it). Setting it for the whole kernel
  would silently remove the protection this failure just demonstrated is real.

## Sv39's address space has a HOLE in the middle, and the arithmetic will not tell you

An Sv39 virtual address is 39 bits SIGN-EXTENDED: bits 63:38 must all equal bit 38. So the usable
space is two halves with a gap between them, not one run from zero:

```
0x0000_0000_0000_0000 .. 0x0000_003F_FFFF_FFFF     the low half, 256 GiB
                  <a hole nothing can address>
0xFFFF_FFC0_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF     the high half
```

**A root index is nine bits, so index 256 is a perfectly good table slot - and it is the first index
of the HIGH half, not "256 GiB".** Computing a test address as 256 GiB (0x40_0000_0000) therefore
lands in root index 256, the walker fills it correctly, `translate` reads it back correctly, and the
access still faults - at an address that looks exactly like the one that was mapped. That happened
here on 2026-09-07 with the per-task address-space test, and it cost one boot: `stval 0x4000000000`,
a load page fault on a page whose PTE was demonstrably present.

Nothing about this is visible in the index arithmetic, which is why `sv39::va_is_canonical` now
refuses a non-canonical address at `map_page`, `unmap_page` and `translate` rather than letting it
fault at the use. Proved by putting the bad address back: the boot now prints `could not map the
private page` and CONTINUES, instead of halting on a fault about the wrong thing.

The three high addresses this boot uses are all in the low half and clear of each other: 128 GiB the
trap-vector fault probe, 192 GiB the user pages, 224 GiB the private task page.

## User mode, and what it still does not have

Reached 2026-09-07: code runs in U-mode on this ISA, cannot read a kernel page, and is preempted out
by the timer. `arch/riscv64/usermode.rs`, in the same shape as `arch/arm/usermode.rs` deliberately -
the RISC-V spelling differs, the increment does not.

The three claims are hardware's own answers rather than the kernel's: `sstatus.SPP == 0` at the
stub's `ecall` (written by hardware at every trap, unforgeable from U-mode); a load of a kernel
address refused by the MMU; and two timer interrupts taken while unprivileged. Two rather than one,
because one proves a tick can be taken from user mode and only the second proves the trap epilogue
put the `sscratch` latch back - a kernel that re-armed it wrongly passes at one tick and corrupts a
stack at two.

ARM proved its isolation half with `ATS1CPUR`, a non-faulting unprivileged translation probe. RISC-V
has no such instruction, so the honest equivalent is the real access, refused by the real MMU, with
the kernel insisting on the exact address it handed over before it forgives the fault.

WHAT IS NOT DONE, and what `spawn_supervisor` still needs:

- **Per-task address spaces** exist now (`context_switch::address_space_selftest`), but USER mode has
  not been run in one. The user-mode selftest still maps into the ACTIVE root, so "a task in U-mode,
  in its own address space" is the two halves not yet joined. Joining them is `spawn_supervisor`.
- **A syscall path.** `ecall` from U-mode currently reaches one gated selftest hook and otherwise the
  reporter. Routing it to the neutral `syscall::dispatch` is the next real seam member.
- **`sscratch` is per-hart, and there is one hart.** It is written by `enter_user` on whatever hart
  runs it; SMP needs it set per hart at that hart's own bring-up.
- **A fault is still fatal.** The reporter names the privilege it came from now, which is the first
  question anyone asks, but there is still no task to kill instead of the machine.

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
