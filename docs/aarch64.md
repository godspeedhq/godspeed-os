# AArch64 Port (Raspberry Pi 4) - Design and Plan

> **Status: BUILT and running on hardware** (this line said "design, not built" until 2026-08-25).
> The Pi 4 boots the arch-neutral kernel, the supervisor spawns services, USB is a userspace `xhci`
> service driving keyboard and mass storage, and GENET ethernet transmits and receives - all recorded
> in `CLAUDE.md`'s amendments and in `docs/multi-arch.md`. Audit 4 flagged this same line as stale on
> 2026-08-12 (finding A4-9) and it was recorded rather than closed; closing it now.
>
> What is still design rather than built is called out per-section below; the *port* is not.
> Non-normative until the constitution is amended (see
> "Constitution amendments needed" below). Target board: **Raspberry Pi 4 Model B, 4 GB, run in
> AArch64 (64-bit).** This doc captures the bring-up plan and, more importantly, the *measured*
> arch-boundary punch-list that makes the port bounded work rather than a guess.


> ## STATUS: milestones 1-20 on hardware; 21 - THE gsh PROMPT RUNS (QEMU) (2026-08-04)
>
> **GodspeedOS boots on a Raspberry Pi 4 Model B and prints over the PL011.** First AArch64 silicon.
>
> ```
> GodspeedOS aarch64: PL011 alive at EL1 (Raspberry Pi 4 / BCM2711, PL011 @ 0xFE201000)
> aarch64: neutral kernel linked; arch/aarch64 stubs pending real bodies. halting.
> ```
>
> `EL1` is the load-bearing word: the Pi's armstub hands the primary core over at **EL2**, and `_start`
> drops to EL1 before any Rust runs. At EL2 the EL1 registers it writes are settable but do not govern
> it, and FP/SIMD is gated by `CPTR_EL2` rather than `CPACR_EL1` - so Rust's NEON `memcpy` would trap
> despite `CPACR_EL1` saying otherwise. The message reports the level reached rather than merely that
> something printed, so a silent failure of the drop cannot be mistaken for success.
>
> **Boot path taken:** the stock GPU bootloader (§5's second option), not the UEFI+Limine lean. It needs
> nothing on the card but the image, and does not foreclose UEFI - that would be a different linker
> script and entry, not a change above the arch layer.
>
> **Built with `python scripts/pi4_build.py`**, which exists because the two aarch64 board variants share
> one cargo artifact path: building `virt` after `pi4` silently replaces it, and the resulting image is
> linked at `0x4008_0000` with its UART at `0x0900_0000`. That is unbootable on a Pi and looks exactly
> like a code bug. The script builds only the Pi 4 variant and **verifies `.text` is at `0x80000`**
> before emitting an image. It cost one hardware round trip to learn.
>
> **Done, each verified on the board:**
>
> | Milestone | Evidence |
> |---|---|
> | 1. Boot + UART | `PL011 alive at EL1` - the EL2 drop worked |
> | 2. MMU | `MMU ON, TTBR0_EL1=0x89000 - this line is running translated`; identity map, 4 KiB granule, 39-bit VA, 2 MiB blocks, 20 KiB of tables |
> | 3. Exception vectors | `VBAR_EL1=0x80800`, proven by a real `brk #0` reporting EC `0b111100` on vector 4 |
> | 4. GIC-400 + timer | `CNTFRQ_EL0=54000000 Hz`, 100 Hz tick, `timer IRQs DELIVERING - 10 ticks` |
> | 5. Context switch | Two kernel tasks ping-ponging; witnesses in callee-saved integer AND `d8`-`d15` verified on resume |
> | 6. EL0 + `svc` | Dropped to EL0, syscall round trip checked in both directions, clean exit; ticks 13 -> 15 across the excursion, so IRQs stayed live at EL0 |
> | 7. Memory map | `source = device tree (authoritative)`; two banks, 1968 MiB usable, the 76 MiB GPU split correctly excluded |
> | 8. Neutral frame allocator | The first arch-neutral code on this board: `crate::memory::init` unmodified, 1968 MiB free, 64 frames distinct, aligned, read-back verified, all returned |
> | 9. Neutral scheduler + preemption | Three never-yielding kernel tasks round-robined by `scheduler::run` under the 100 Hz tick; on the board, counters 194/194/195 and lines torn mid-word by the tick |
> | 10. Per-task page tables | A private address space built, `TTBR0_EL1` swapped, a page read back through the new mapping, kernel still reachable, every frame reclaimed |
> | 11. TTBR1 split | Kernel linked high / loaded low, relocates PC **and** SP into `TTBR1`, then RETIRES the low map - `TTBR0_EL1` empty and free for a task |
> | 12. EL0 task, own address space | A task table of ONE frame, no kernel entries; separation enforced by hardware |
> | 13. SDK syscall ABI | `x8` = number, `x0`-`x2` = args, `svc #0`; the `logger` service compiles for AArch64 |
> | 14. Real `page_tables::PageTable` | `loader.rs` can load a service ELF; reclaim proven (`4 pages reclaimed`) |
> | 15. User-copy seam | Range check + `ldtrb`/`sttrb` (EL0 permissions) + fault fixup, **which fired on the board** |
> | 16. Real syscall dispatch | `Log` with no cap **REFUSED** (`-2`); unknown number returns a defined error |
> | 17. EL0 task under the scheduler | Kernel tasks 95/95/95 with an EL0 task interleaved, 55 EL0 ticks, no panics |
> | 18. **A real service runs** | `logger: ready` - compiled Rust, loaded by the neutral ELF loader, one spawn, slot 0, no faults |
>
> The timer evidence is a rate check, not just a delivery check: the log timestamps put 10 ticks 111 ms
> apart, which is 100 Hz. A wrong reload would still have delivered ten interrupts.
>
> Also done: BCM2711 PL011 init (bounded TXFF wait, baud divisors deliberately untouched), GPIO14/15
> muxed to ALT0 with a pull-up on RX. Note BCM2711 replaced the Pi 2's `GPPUD`/`GPPUDCLK` strobe with
> direct 2-bit-per-pin registers at `0xE4` - porting the old code verbatim would compile and do nothing.
>
> **The hard-won one: an EL0-accessible region is PXN at EL1.** The kernel may not execute what
> userspace can reach - ARM's equivalent of x86 SMEP. Granting EL0 access to the 2 MiB block holding
> the kernel's `.text` therefore makes the KERNEL non-executable, and the core dies on its next
> instruction fetch with no way to report it (the handler cannot fetch its vector either). It cost two
> hardware round trips, presenting first as `tlbi vmalle1` hanging and then as the MMU enable hanging -
> the same fault landing wherever the new mapping took effect. EL0 code and stack now live in a
> linker-placed, 2 MiB-aligned `.el0` region. A real consequence: the EL0 task cannot call kernel print
> functions at all, and reports through a syscall argument instead.
>
> **Milestone 7 - the memory map, and where it comes from.** The ARM is not told how much RAM it has.
> Two sources are read, both **before the MMU and caches come on** (the GPU reads the request buffer
> straight out of RAM, so asking early removes the cache-maintenance question rather than answering it):
>
> - **The device tree**, pointer in `x0` at entry - authoritative, full 64-bit banks. `_start` was
>   throwing this pointer away: its very first instruction (`mrs x0, CurrentEL`) clobbered `x0`. It is
>   now stashed in `x19`, which survives the `eret`, and handed to Rust.
> - **The mailbox** `GET ARM MEMORY` tag - a deliberately weak fallback. It returns a 32-bit base and
>   size, so it cannot describe RAM above 4 GiB and under-reports a >1 GiB board. Which source was used
>   is **printed**, because "960 MiB" means something different depending on whether it is the machine's
>   RAM or merely all the fallback could describe.
>
> The device tree is **untrusted firmware input**, parsed accordingly: every offset bounds-checked
> against the header's own `totalsize` before it is read, the walk iteration-bounded, and anything that
> fails to parse yielding no map rather than a partially-believed one. Root `#address-cells` /
> `#size-cells` are read, not assumed - they set the width of every field in `reg`, and guessing them
> mis-parses every address on a board that differs.
>
> **The map is clamped to what the identity map actually reaches**, and that clamp is the part worth
> knowing about. The identity map covers the low 4 GiB; an 8 GiB Pi 4's firmware reports banks above it.
> Recording those as usable would hand the allocator RAM with no translation - a fault much later,
> blamed on whatever touched it. Dropping capacity is the safe direction, and the drop is reported.
> QEMU's `raspi4b` is fixed at 2 GiB, so the condition cannot be reached by configuration; the
> `memmap-clamp-test` feature injects synthetic over-limit banks and both paths were observed firing
> (an 8 GiB bank truncated, a 6 GiB bank dropped). Commandment IX - a guard never seen firing is not
> evidence that it fires.
>
> Worth knowing: the on-disk `bcm2711-rpi-4-b.dtb`'s `/memory` node is a **zero-size placeholder**
> (`reg = <0x0 0x0 0x0>`) which the firmware patches at boot, so a parser tested only against the file
> would conclude the board has no RAM.
>
> **On the board (Pi 4 Model B rev 1.5, 2 GB, BCM2711 - decoded from `board revision 0xb03115`):**
>
> ```
> mailbox: ARM memory base 0x0 size 948 MiB
> aarch64: device tree pointer (x0 at entry) = 0x2eff1c00
> memmap: source = device tree (authoritative)
> memmap:   0x0..0x80000          usable       512 KiB
> memmap:   0x80000..0x400000     kernel image 3584 KiB
> memmap:   0x400000..0x3b400000  usable       966656 KiB
> memmap:   0x40000000..0x80000000 usable      1048576 KiB
> memmap: usable RAM 1968 MiB across 4 regions
> ```
>
> **The two sources disagree by a factor of two, and that is the whole justification for parsing the
> device tree.** The mailbox reported 948 MiB; the device tree reported 1968 MiB across **two banks**.
> Taking the fallback would have cost 1020 MiB - 52% of the machine's usable RAM - and it would have
> looked entirely reasonable in the log. Note that under QEMU the two sources *agreed* (960 MiB each),
> so emulation could never have revealed this: it is exactly the class of thing only silicon shows.
>
> The 76 MiB hole between the banks (`0x3b400000..0x40000000`) is the default `gpu_mem` split, which the
> firmware excludes from `/memory` for us. Two banks also means the multi-bank path - `#address-cells=2`,
> `#size-cells=1`, several `reg` pairs in one property - is hardware-exercised rather than reasoned
> about. The clamp correctly did **not** fire: every bank on a 2 GB board is below the identity-map
> limit, which is why it needed the injected test to be proven at all.
>
> **Milestone 8 - the first arch-neutral kernel code on this board.** `crate::memory::init` is the same
> allocator the x86 build has used since v1, reached through the `BootInfo` the seam defines and
> compiled without modification. It is the demarcation claim tested on a second ISA in the place it is
> easiest to get wrong.
>
> On the board:
>
> ```
> memory: kernel phys [0x80000, 0x400000) hhdm=0x0
> allocator: frame bitmap 64 KiB x2 covers 524288 frames (2048 MiB), carved at phys 0x7ffe0000
> memory: frame allocator ready (1968 MiB free)
> aarch64: frame allocator OK - 64 frames distinct, aligned, read-back verified, all returned (503904 free)
> ```
>
> The numbers close: 503904 frames x 4 KiB is exactly the 1968 MiB the map reported, and the free count
> returned to its starting value after every frame was released.
>
> **The board exercised something QEMU structurally could not.** The bitmap is carved from the top of
> the largest usable region, and on hardware that is the *second* bank - `0x7ffe0000`, inside
> `0x40000000..0x80000000`. QEMU's `raspi4b` has a single 960 MiB bank, so under emulation the carve
> always landed in low RAM. The high bank is therefore not merely described by the map, it is written
> and read back 128 KiB at a time by the allocator's own bookkeeping before a single frame is handed
> out. A memory map that had over-claimed the second bank would have failed here rather than at some
> later allocation.
>
> `hhdm_offset = 0` is the **correct** value, not a missing one: the low 4 GiB is identity mapped, so a
> physical address is already addressable. The allocator distinguishes "identity" from "caller forgot"
> via `page_tables::PHYS_IS_IDENTITY`, which this port had to set `true` (it was `false`, inherited from
> the stub). Same posture as the 32-bit ARM port, opposite to x86.
>
> **The selftest writes every frame, then verifies every frame - two passes, deliberately.** A single
> write-then-read pass cannot detect **aliasing**, because two distinct physical addresses backed by the
> same RAM each read back correctly the instant after being written. Separating the passes means an
> alias overwrites the earlier frame's value and the verify catches it. That is the failure this port is
> actually exposed to: a memory map claiming RAM the board does not have shows up as address wrap or
> aliasing on real silicon far more often than as a clean fault. Proven to fire by corrupting one frame.
>
> **A latent bug found on the way in:** `serial_write_byte` and `serial_write_bytes_lockfree` - the path
> `kprintln!` uses - wrote straight to the PL011 data register with no `TXFF` poll, while the arch's own
> `put_byte` polled correctly. Harmless while this file was a boundary stub that nothing called; the
> moment neutral code began logging it would drop bytes as soon as the 32-entry FIFO filled, and it
> would have read as a kernel fault rather than a UART one. Both now route through `put_byte`, and `\n`
> is expanded to `\r\n` since the neutral log emits bare LF.
>
> **Milestone 9 - the neutral scheduler preempts tasks that never yield.** Milestone 5 proved a switch
> between tasks that *called* the switch, which is cooperative scheduling and not what a service does: a
> service blocks on `recv` and never yields, so only the timer can take the core from it. Three kernel
> tasks that deliberately spin are now round-robined by the **neutral** `scheduler::run` under the
> 100 Hz tick. A 45 second run gave 926 / 936 / 925 lines - fair share within 1% - with no panic.
>
> Two arch-side pieces were load-bearing, and both failed in ways that looked like something else:
>
> - **A first-entry trampoline.** The scheduler masks IRQs before its initial `switch_context`, so a
>   task whose `lr` pointed straight at its entry begins with `DAIF.I` set and, never yielding, never
>   has it cleared. Observed exactly: **task A ran correctly and forever while B and C starved** - which
>   reads as a broken scheduler when in fact it was never given a tick. x86 solves this the same way
>   (`task_entry_trampoline` does `sti` then `ret`); this port carries the entry in `x19` instead of on
>   the stack, because `ret` here jumps to `lr` rather than popping.
> - **The GIC EOI moved before the switch.** The neutral tick switches tasks *inside* the IRQ handler
>   and may never return on this task's stack. The GIC keeps a priority active until the interrupt is
>   retired, so an EOI deferred past the switch leaves the timer permanently active and blocks every
>   later interrupt: one tick, then silence.
>
> Also here: `enable`/`disable_interrupts`, `local_irq_save`, `wait_for_interrupt` and
> `read_cycle_counter` became real (they were no-op stubs). Note `DAIF` is a **mask**, so its polarity
> is inverted from x86's `IF` - an easy place to write a plausible inversion that deadlocks the machine
> at the first critical section. With a real cycle counter, `liveness_deadline_cycles` is now answered
> too, so the **cross-core wedge watchdog is armed and says so** (`a core dark for 625000000 counter
> ticks panics`) - the 32-bit port ran its entire bring-up silently undefended because that value keyed
> off an unrelated stub.
>
> **A packaging bug worth recording:** the image jumped from 1.5 MB to **18 MB**. `objcopy -O binary`
> emits every byte up to the last allocated section, and the linker script placed `.el0` *after* `.bss`
> - so the neutral scheduler's large static arrays were materialised into the file as literal zeros the
> firmware had to read off the card every boot. Harmless while `.bss` was a few KiB of arch state.
> `.bss` and the boot stack now come last, the image ends at `__el0_end`, and `__kernel_end` still spans
> everything the kernel owns so the allocator is not handed the stack it is standing on.
>
> **Milestone 10 - per-task page tables, and the limit they exposed.** An address space is a private L1
> plus **its own copies** of the four kernel L2 tables. Sharing them (what the 32-bit port does) saves
> 20 KiB and buys an aliasing hazard: a table split in one space is seen by all of them, and reclaim
> then has to tell a table the task owns from one it merely points at. Copying means nothing is aliased,
> so reclaim frees everything the root reaches with no ownership test. The selftest finally exercises
> the `TTBR0_EL1` swap milestone 9 shipped unexercised - install the new table, read a value back
> through the new mapping, confirm kernel memory is still reachable under it, switch back, check every
> frame returned.
>
> **The limit: this port cannot give a task a VA below 4 GiB, and that blocks real user tasks.** The
> kernel is identity-mapped across the whole low 4 GiB, so a task page there lands inside a kernel 2 MiB
> block and would shadow the kernel's own view of that physical range. `USER_STACK_TOP` (0x8000_0000)
> sits directly above the frame allocator's bitmap, so this is the *normal* case for a real task, not a
> corner. `map` refuses it loudly. The 32-bit port escapes the same collision only by accident of scale:
> its RAM identity map ends at 1 GiB, well below its 2 GiB user range.
>
> **The fix is TTBR1, and it is the next milestone.** AArch64 splits translation between `TTBR0_EL1`
> (low VA) and `TTBR1_EL1` (high VA). Putting the kernel in TTBR1 leaves TTBR0 *entirely* to the task:
> no kernel entries in a task table, no collision possible, no kernel-copying half to this file, and the
> same kernel-high / user-low shape x86 already has (so `hhdm_offset` and `PHYS_IS_IDENTITY` stop being
> special-cased). The cost is relinking the kernel at a high address and jumping across the transition
> during boot, plus mapping the peripherals through the high half. Deliberately not attempted as a
> tail-end change to a boot path that is nine milestones deep and hardware-proven.
>
> Speculative block-splitting code that would have papered over the collision was written and then
> **deleted** rather than kept (§26.2): it only exists to serve a layout this port is going to abandon.
>
> **Milestone 11 - the TTBR1 split, which removes the limit milestone 10 hit.** The kernel is now
> LINKED high and LOADED low (VMA `KERNEL_VA + 0x80000`, LMA `0x80000`): the firmware still drops a flat
> binary at `0x80000`, but every symbol is a high address. Early in boot the kernel relocates itself into
> `TTBR1` and then **retires the low identity map**, leaving `TTBR0_EL1` empty. A task page below 4 GiB
> can no longer shadow the kernel, because the kernel is not there.
>
> ```
> aarch64: TTBR1 high half LIVE - phys 0x418530 reads/writes as 0xffffff8000418530
> aarch64: kernel RUNNING FROM THE HIGH HALF (TTBR1), PC above 0xffffff8000000000
> aarch64: stack relocated too, SP = 0xffffff80004980a0
> aarch64: low identity map RETIRED - TTBR0_EL1 is free for a task
> memory: kernel phys [0x80000, 0x499000) hhdm=0xffffff8000000000
> ```
>
> Three things make the transition safe, and all three are in the code rather than in anyone's head:
>
> - `_start` reaches symbols only through `adrp`/`add`, which is **purely PC-relative** - the same
>   instruction gives the right LOW address with the MMU off and the right HIGH one after. (Confirmed
>   accidentally, when a selftest printed a low address for a static before the jump.)
> - Between enabling the MMU and jumping, **both halves translate at once**. There is no instant where
>   the core executes from an address that does not resolve.
> - **Peripherals move first.** A device register is still named by its physical address, and the UART
>   must survive the step or the next failure reports nothing at all.
>
> `hhdm_offset` is now `KERNEL_VA_BASE` and `PHYS_IS_IDENTITY` is `false` - exactly the arrangement x86
> has with Limine's HHDM, so the neutral allocator's existing handling applies unchanged.
>
> **Two mistakes here are worth keeping.** The relocation asm used a hardcoded `x9` as scratch while
> letting the compiler allocate `{base}` - and it chose `x9` too, so `mov x9, sp` destroyed the base and
> `orr x9, x9, x9` left SP exactly as it was. The kernel then ran on perfectly from its still-mapped low
> stack, printed several more lines, and only died when the low map was retired - whereupon the
> exception handler faulted on its own push and recursed, walking `FAR` down by one 272-byte frame each
> time. Nothing pointed at the asm. **Printing SP settled it in one run**, which is why the boot now
> reports SP permanently: a PC that fails to relocate stops the machine instantly, but an SP that fails
> to relocate works fine until something unrelated takes the ground away.
>
> Separately, the high-half selftest first compared an address against *itself* - it took the symbol's
> address as the "high" side, but pre-jump codegen yields a low one - and passed while testing nothing.
> It now constructs the alias explicitly.
>
> **The EL0 demo is retired by this, on purpose.** It put its code in a linker-placed `.el0` region
> reached through the kernel's identity map, which only worked while kernel and user shared one address
> space. EL0 cannot reach the high half at all, so it now takes an instruction abort from the lower EL
> (`ESR 0x8200000e`) - reported cleanly by the vectors. That is the split working. Its replacement is a
> real EL0 task with its code and stack mapped into its **own** TTBR0 space at a low VA it genuinely
> owns; every piece needed for that already exists.
>
> **The board found a bug QEMU could not.** With `hhdm_offset` finally holding a real value, the neutral
> `protect_kernel_page_table_frames` began running for the first time on this port - and it walks **x86
> PML4 tables** by their x86 format, treating indices 256..512 as the kernel half. It followed a garbage
> descriptor and took a data abort (`ESR 0x96000004`, `FAR 0xc60e972657000`), symbolized to that
> function inlined into `boot_high`.
>
> It had been harmless only because it early-returns when `hhdm == 0` - a *different claim* that merely
> happened to hold on every non-x86 port. The real condition is whether a **bootloader** placed the
> kernel's page tables in RAM the allocator would otherwise hand out, so that is now what is asked:
> `page_tables::BOOTLOADER_PLACED_TABLES`, `true` on x86 (Limine does exactly that) and `false`
> everywhere else, where the kernel builds its own tables in `.bss` inside the image the memory map
> already excludes. The 32-bit ARM port was relying on the same accident and is now explicit too.
>
> With that fixed the board runs the whole stack clean: **1954 MiB** free, allocator and page-table
> selftests both passing, the timer at the board's real 54 MHz, the watchdog armed, and the scheduler
> round-robining three never-yielding tasks at **153 / 153 / 153** lines (counters 152 / 152 / 152).
> No exceptions, no warnings.
>
> **Milestone 12 - an EL0 task in its own `TTBR0` space.** `PageTable::new` now allocates **one** frame
> and copies nothing: a task table holds the task and nothing else, because the kernel is in `TTBR1`
> where no `TTBR0` switch can disturb it and no EL0 access can reach it. That deleted both the 20 KiB
> kernel-copy per address space and the collision class it carried. The task is frames, not linker
> sections - a code page and a stack page with a position-independent payload copied in.
>
> Page 0 is now reserved: the allocator handed out physical frame 0 as a page-table root, printing
> `TTBR0=0x0`, which works only by coincidence and collides with the `cr3 == 0` sentinel
> `switch_context` uses for "no address space".
>
> **Milestone 13 - the SDK syscall ABI, and a design correction.** `raw_syscall` now has an AArch64 arm:
> number in `x8`, arguments in `x0`-`x2`, result in `x0`, via `svc #0` - the same shape Linux uses here.
>
> Milestone 6 read the syscall number from `ESR_EL1.imm16` and justified it as userspace being unable to
> lie about which call it made. **That was wrong twice.** `svc #N` encodes `N` in the instruction, so it
> must be a compile-time constant - a real ABI whose caller picks the number at runtime cannot express
> it at all. And there was no security property to lose: a task is entitled to request any syscall
> number, and what stops it doing something it should not is the capability check inside the handler,
> which trusts neither the register nor the immediate. The EL0 demo blob was updated to the register
> convention, so the one piece of EL0 code that exists proves the ABI that services will actually use.
>
> Nothing is truncated on this path, unlike the 32-bit ARM one: registers are 64-bit, so a `u64`
> argument passes whole and the whole class of "does this value exceed 32 bits" bugs does not arise.
>
> The SDK builds for `aarch64-unknown-none`, `x86_64-unknown-none` and `armv7a-none-eabi`, and the
> **`logger` service compiles for AArch64** with `e_entry` correctly resolving to `service_main` (the
> `#[no_mangle]` trap that produced `e_entry = 0` on the 32-bit port). `adversarial.rs` gained AArch64
> arms too, which the SDK's clean compile would NOT have caught - those functions are `pub` and only
> fail when a service calls them. AArch64 has no trapping integer divide (like ARMv7), so
> `fault_divide_by_zero` is `udf #0`; and it has a genuine non-canonical analog in the **VA hole**
> between the `TTBR0` and `TTBR1` ranges, which no mapping can ever make valid.
>
> **Milestone 14 - `ptables` becomes the real `page_tables::PageTable`.** `loader.rs` was calling the
> `unimplemented!()` stub, so no service ELF could load no matter what else worked. The
> hardware-proven implementation is now behind the neutral signature, with the arch-native form kept as
> `map_raw` for the EL0 path - so the bring-up path and the real ELF path share one flag translation
> rather than two that could drift.
>
> The translation has one decision worth naming: **`PCD | PWT`**. On x86 those disable caching for MMIO;
> the faithful AArch64 equivalent is not "uncached Normal" but the **Device** attribute, which
> additionally forbids the reordering, merging and speculative repetition that make a wrongly-typed MMIO
> mapping misbehave in ways no fault points at.
>
> Two hooks gained bodies and one is deliberately still empty: `free_page_table_root` frees the table
> tree, `reclaim_user_frames` frees the leaf pages, and `finalize_service_address_space` is a genuine
> no-op because - unlike the 32-bit port - there is no kernel map to clone into a new space.
>
> Reclaim is **proven rather than assumed**, because it is the one that fails silently: an empty stub
> would leak every page of every task that ever died and show up only as the machine slowly running out
> of memory. The selftest maps extra pages, reclaims the space, and checks both the count and the
> allocator's free total - `4 pages reclaimed, all frames returned`.
>
> **Milestone 15 - the user-pointer copy seam.** Where the kernel touches memory a task chose the
> address of. Three defences, because each catches what the others cannot:
>
> 1. **A range check** - pointer and length inside the user half, addition checked for overflow.
> 2. **`ldtrb` / `sttrb`, not `ldrb` / `strb`** - the *unprivileged* load and store. Executed at EL1 they
>    apply **EL0 permissions**, so the hardware refuses a kernel address even if the range check were
>    wrong. Defence in depth at no cost: same instruction count, same speed, and a bug in check (1) stops
>    being exploitable.
> 3. **A fault fixup** - a range-valid pointer can still be unmapped, and the abort lands at EL1 looking
>    exactly like a kernel bug. Unguarded that halts the machine, which is a denial of service any
>    service could trigger by passing a bad pointer. Vector 4 became recoverable, and the fixup covers
>    *only* the faulting instruction - so a kernel bug faulting anywhere else still halts loudly.
>
> Reads are **copied** into a per-core buffer rather than borrowed: handing the kernel a pointer into
> user memory leaves every later read racing the task, which can change the bytes between validation and
> use.
>
> **The bug this cost is worth keeping.** The per-core state was first indexed via `current_core_id()`,
> which needs tables that are not up when the first copy runs - and, far worse, the **fault handler**
> calls into this module. Indexing an unallocated arena from a fault handler faults again, and the
> second fault happens while reporting the first, so the machine went **completely silent** with nothing
> printed at all. A fault handler must not depend on initialisation order; it now indexes a fixed array
> by `MPIDR_EL1.Aff0`, which needs no setup and cannot fail.
>
> The selftest drives all four outcomes, including the two that only fire on bad input, and **counts
> recovered faults** so "the unmapped pointer survived" is backed by evidence the fixup fired rather than
> by something upstream having quietly rejected the pointer.
>
> **Milestone 16 - real syscall dispatch.** `svc` now reaches the **neutral** `syscall_handler`, the same
> function x86 and arm32 call, with the same numbering. Bring-up numbers sit above every real one so the
> ranges cannot collide, and disappear with the demo.
>
> From real userspace, through the real ABI, the EL0 task proves two things:
>
> ```
> [EL0] real syscall Log(no cap) -> 2 (negative = REFUSED, no ambient authority),
>       unknown syscall -> 1 (defined error, not a crash)
> ```
>
> - **`Log` with no capability is refused** (`-2`, `CapNotHeld`). §3.1 enforced end to end on AArch64:
>   authority comes from holding a capability, never from being the caller. The kernel does not log; it
>   says no.
> - **An unknown syscall number returns a defined error** (`-1`), not a fault (§22 Fuzz F2).
>
> The neutral subsystems moved into the main boot rather than only the scheduler demo, because a real
> syscall reaches `current_task_lookup_cap` and therefore per-core state - reaching that before it
> exists is exactly how milestone 15 went silent.
>
> The unknown-number test needed fixing *after it appeared to pass*: `0xBEEF` is above the bring-up base,
> so the call went to the demo handler and proved nothing about the neutral path. `WARN unknown svc
> #48879` in the log is what gave it away.
>
> **Milestone 17 - an EL0 task under the neutral scheduler.** Every earlier EL0 excursion was a
> one-shot. This one the **scheduler** enters, the timer preempts, and kernel tasks share a core with:
>
> ```
> [EL0 task] tick 0 (scheduled, preemptible, in its own address space)
> sched: [from a kernel task] EL0 task has reported 255 ticks - EL0 and EL1 tasks are sharing the core
> ...                                                509 ...  749 ...  998
> ```
>
> The progress report comes from a **kernel** task, so "both are running" is one observation rather than
> two that might not overlap. Over 40 s: kernel tasks 609/614/613 lines, EL0 ticks rising steadily, no
> panics.
>
> `TaskContext::new_user` bridges a real mismatch: `switch_context` ends in `ret` and stays at EL1, while
> entering EL0 needs an `eret`. The context therefore points at a trampoline that performs the `eret`,
> carrying the user entry and stack in `x19`/`x20` - the same shape x86 uses, differing only in where the
> values ride. Its `sp` is the task's KERNEL stack, load-bearing beyond first entry: after the `eret` it
> stays in `SP_EL1`, so it is the stack every later trap from this task lands on.
>
> **The bug this found had been sitting there for four milestones.** `syscall_slot` returned
> `null_mut()`, and `prepare_ring3_switch` - which runs *only* for tasks marked `is_user` - writes
> through it. Nothing had ever been marked user, so the stub was unreachable and invisible; the first
> scheduled EL0 task turned it into a null write on the context-switch path. A stub reachable down
> exactly one path stays silent until something takes that path.
>
> **Writing code needs cache maintenance - found on hardware, invisible in QEMU.** The first board boot
> of milestone 17 died with `ESR` EC `0b001110` (Illegal Execution State, `PSTATE.IL` set), and the log
> showed the scheduled task printing the *one-shot demo's* messages.
>
> The instruction and data caches are not coherent on ARM. The payload was copied into a frame the
> previous EL0 task had just executed from and freed; the allocator handed the same frame straight back,
> the I-cache still held the old blob, and the core ran the previous task's instructions until it fell
> off the end. QEMU models no separate I-cache, so it had passed there a dozen times.
>
> `mmu::sync_instruction_cache` now cleans the range to the point of unification and invalidates the
> I-cache over it, with line sizes read from `CTR_EL0` rather than assumed. **This is not a demo fix:**
> the kernel writes code every time it loads a program, so the ELF loader needs it the moment it loads a
> service's text - and it would have been a far more confusing bug to meet there.
>
> **Milestone 18 - a REAL service runs.** Not a hand-written blob: `services/logger`, compiled from Rust
> against the SDK for `aarch64-unknown-none`, embedded in the kernel image, and loaded by
> `task::spawn_service_with_config` - the exact machinery the supervisor's spawn syscall uses.
>
> ```
> sched-spawn: spawning the logger service through the NEUTRAL spawn path
> task: 'logger' spawned OK on core 0 (slot 0)
> sched-spawn: entering scheduler::run(0) - watch for 'logger: ready'
> logger: ready
> ```
>
> That one call exercises nearly everything the port has built: the **ELF loader** parsing real program
> headers, **`page_tables::PageTable`** (the loader creates and maps through it), **`sync_instruction_cache`**
> (the loader writes a service's text, so the I-cache must not hold what those frames held before), the
> kernel stack pool, capability wiring, the service context page, **`TaskContext::new_user`**, and then
> the **SDK ABI** and **syscall dispatch** the moment the service makes its first call.
>
> `logger: ready` therefore means far more than the logger working: a compiled Rust service ran on this
> board and talked to the kernel. Services are embedded incrementally, exactly as the 32-bit port does -
> `aarch64_built` in `kernel/build.rs` currently lists `logger`; the rest keep the placeholder.
>
> **The same cache bug again, in the loader - and the lesson is the interesting part.** Milestone 18
> passed in QEMU and looped on the board: the boot re-entered itself until the endpoint table filled, 95
> task slots later. The logger's text had landed in frames the one-shot EL0 demo just freed; the I-cache
> still held the old blob, so the "logger" ran the DEMO's code, hit its exit syscall, and switched to a
> `CTX_KERNEL` saved during boot.
>
> The cache fix had been written the day before - and applied only to the two hand-written payload
> copies, not to the ELF loader, **even though its own commit message said the loader would need it**.
> Fixing the instances rather than the class left it open exactly where it mattered.
>
> Two fixes, because the bug had a cause and an amplifier:
>
> - **Cause:** `finalize_service_address_space` - the arch hook the neutral loader already calls per
>   service - now syncs the I-cache over every page of the new address space. At the hook, it covers
>   every service the loader will ever produce.
> - **Amplifier:** `CTX_KERNEL` was a resurrection point. The exit syscall now refuses when the demo is
>   not running, so this class of mistake costs one loud refusal instead of a boot loop.
>
> The hook reports its page count (`67 pages I-cache synced`) because QEMU cannot demonstrate the fix -
> a silent hook is how this shipped once already. With both fixes the board runs it clean: one spawn,
> slot 0, `logger: ready`, no faults.
>
> **Milestone 19 - serial INPUT.** Output has been one-way since milestone 1; this is the first time the
> port can be typed at, and the shell cannot exist until it can. The PL011 receive FIFO drains into a
> ring the neutral `ConsoleRead` path pops from, driven both by the timer tick and directly by a blocked
> reader (a starved ISR would otherwise strand a byte in the FIFO with a reader asleep waiting for it).
>
> ```
> aarch64: serial RX ready - 256 byte ring, PL011 error bits discarded (DR[11:8]), ...
> echo: got 'g' (total 1) ... "gsh test 123" - 12 of 12, in order
> ```
>
> **The 32-bit port's lesson is inherited, not rediscovered:** the PL011 reports receive errors in the
> SAME read as the data (`DR` bits 11:8), so masking them off silently promotes line noise to input. On
> the Pi 2 a GPIO HAT held RX low - a continuous break - and each one enqueued a spurious `0x00` until a
> full-screen editor repainted 966 times while the document changed twice. Flagged bytes are discarded
> here from the outset.
>
> A persistently overflowing line is switched off after a **duration** of unbroken overflow, not a byte
> count: a count does not measure how long a condition has persisted, and choosing one is how you get a
> threshold that either fires on a fast typist or never fires at all.
>
> **Milestone 20 - cross-service IPC.** `ping` and `pong`, both compiled Rust services, spawned by name
> through the same path the supervisor's spawn syscall takes, talking over kernel IPC:
>
> ```
> pong: received "1" ... pong: received "3962"
> ```
>
> 3962 messages, in order, no gaps.
>
> **The bug that stood in the way is worth knowing about, because it is invisible with one task.**
> `SP_EL0` is a **single register shared by every EL0 task**, and the kernel never runs on it - so
> nothing else saves it. The trap frame did not either. With one user task that is survivable, which is
> exactly why it stayed hidden from milestone 12 through 19; with three services, whichever ran last
> left its user stack pointer behind and the next task to `eret` built its stack frame on another
> task's stack, writing past the top of the mapped region.
>
> The frame now carries `SP_EL0` (offset 264, inside the 272 bytes the vectors already reserved), and it
> is restored **only when returning to EL0** (`SPSR.M[3:0] == 0`). Restoring it unconditionally was
> tried and is wrong: an exception taken at EL1 captures whatever `SP_EL0` happens to hold - another
> task's, or zero - and re-imposing that on the way out clobbers the task actually at EL0.
>
> Finding it needed one diagnostic and one disproof. The register dump gave `dest = sp + 0x307b`,
> `src = sp + 0x1040` at the faulting `memcpy`, which pinned `sp` exactly - and from there the frame
> arithmetic said the entry stack pointer had been `0x8000_2020` rather than `0x8000_0000`. An earlier
> hypothesis (that the syscall number in `x8` collided with AAPCS64's indirect result register) was
> **disproved** by the fault surviving the change unaltered.
>
> **On the board: 63,579 messages in 2 minutes 12 seconds, zero faults.**
>
> **The fault I called "QEMU-only" was real, and I was wrong to lean that way.** Emulation reported a
> kernel abort in `aarch64_exception_return` after anywhere between 257 and 12,088 messages; the board
> ran 63,579 clean, so an emulation artefact looked likeliest. It was a genuine race:
>
> The return path writes `ELR_EL1` and `SPSR_EL1` and then runs a dozen more instructions before the
> `eret`. **An interrupt in that window makes the hardware overwrite both**, destroying the user context
> just loaded - and the `eret` then returns into `aarch64_exception_return` itself, at whatever EL the
> stale `SPSR` names. Exception ENTRY masks interrupts, so the window is closed for the common path; it
> is open for a task resumed after blocking, because the scheduler re-enables interrupts around the
> switch. The sequence now masks IRQ and FIQ for its whole length (the `eret` restores `DAIF` from
> `SPSR` regardless, so it costs nothing).
>
> After the fix: three runs of **~107,000 to 109,000 messages each, zero faults**, where the same build
> previously died in the hundreds. The lesson is worth more than the fix: *"does not reproduce on
> hardware in two minutes"* is not *"not a real bug"* - the emulator's different timing made a narrow
> race far more visible, which is the opposite of the pattern every other bug in this port followed.
>
> > **Milestone 21 - the `gsh` prompt.** The real bootstrap: the kernel makes its **one** direct spawn
> (§11.1, the supervisor), and the supervisor spawns everything else from its own manifest - logger,
> pong, ping, and the shell with a `CONSOLE_READ` cap.
>
> ```
> supervisor: ready
> shell: ready (type 'help')
> version
> GodspeedOS 0.9.1 aarch64 (5cd0e00)
> ```
>
> A command typed on the serial console, executed, answered. That is the whole path: keystroke -> PL011
> receiver -> ring -> `ConsoleRead` syscall -> shell -> command -> console output.
>
> Three stubs stood between "the shell spawned" and "the shell works", and each failed silently:
>
> - **`input_ready()` returned `false` forever.** On x86 the prompt is gated on a signal raised when
>   `xhci` comes up, because a USB keyboard is the input path there. Here the input path is the PL011,
>   which has been up since milestone 1, and `xhci` is a placeholder that never spawns - so nothing
>   would ever have raised it and the shell would have waited for a prompt that never came.
> - **`console_write_bytes_gated` was a no-op.** The shell spawned, ran, and produced nothing, because
>   every byte it printed went into a function that discarded it. It is distinct from the kernel log
>   path on purpose, so a full-screen app can own the display while kernel logging continues.
> - **`tsc_ticks_per_quantum()` returned `0`**, which the neutral `cycles_to_ticks` takes literally and
>   collapses *every* timed wait to one 10 ms tick. A service asking for a second slept 10 ms. The
>   32-bit port ran its entire bring-up with the same stub and the same silent 100x error.
>
> The `probe-*` and `brutal-*` spawn failures in the log are expected: those are test services not yet
> built for AArch64, so they hold the placeholder ELF and fail loudly with `LoadFailed(TooSmall)` -
> which is the supervisor behaving correctly, not a port fault.
>
> **`ping`/`pong` are opt-in (`pi4-demo-services`), and that is a usability fix rather than a tidy-up.**
> They pace with `yield_cpu`, not a sleep, so they emit ~500 log lines a second - on the board that was
> 41,743 lines in 87 seconds. The shell came up correctly underneath all of it, but a prompt scrolling
> past faster than a human can read is not a working prompt. Cross-service IPC is already
> hardware-proven at 63,579 messages, so the demo is now something you turn on to watch IPC rather than
> something every image carries.
>
> **Not done:** hardware verification of the prompt, `arch::init` + the `kernel_main` handoff, PSCI SMP,
> and the drivers the storage/network/USB commands need (SD/EMMC, GENET, VL805 xHCI).
>
> **Known unknown:** the image that worked fixed two things at once - the link address *and* the PL011
> init. The wrong link address alone was fatal, so that was necessary; whether the firmware had already
> enabled the UART, making our init merely redundant, is untested.

## 1. Why the port is bounded (measured, not asserted)

The whole bet is that the microkernel isolates hardware to `kernel/src/arch/x86_64/`, so the
arch-neutral majority (capabilities, IPC, scheduler logic, services, SDK, tooling, tests) carries over
unchanged. That was audited before the port precisely so any AArch64 failure is unambiguously an
arch-layer bug, not a pre-existing one.

A static survey of the current tree (`grep` for `arch::x86_64::` and inline asm outside the arch dir)
measures how sealed the boundary actually is:

- **126** direct `arch::x86_64::` references across **16** arch-neutral files.
- **23** inline-asm sites outside `arch/x86_64/`.
- **Zero** arch references in `capability/`; **zero in code** in `ipc/` (two doc-comments only).

The verdict: the two most constitutional subsystems - the capability table and IPC - are completely
arch-clean, exactly where the "business as usual above arch" thesis has to hold. Every leak lives in
the **CPU plumbing**, which *is* the hardware interface and was always going to be rewritten:

| Area | `arch::x86_64` refs | asm sites | Nature |
|------|--------------------:|----------:|--------|
| `task/scheduler.rs`   | 34 | 9 | context switch, per-cpu, timer, halt |
| `syscall/dispatch.rs` | 31 | 0 | user-copy, `read_cycle_counter` (TSC) |
| `main.rs`             | 20 | 1 | boot orchestration (BootInfo/init/ap_init) |
| `task/mod.rs`         | 11 | 0 | spawn plumbing |
| `smp/*`               | 12 | 12 | CR3 read/write, `invlpg`, `pushfq;cli`/`popfq` |
| `memory/*`            |  5 | 1 | CR3 read (allocator) |
| `loader/control/interrupt-route/log` | ~13 | 0 | page tables, serial, IOAPIC/EOI |

The **23 asm sites reduce to ~5 operations**, each with a clean AArch64 analog:

| x86 asm | Operation | AArch64 analog |
|---------|-----------|----------------|
| `mov {}, cr3` / `mov cr3, {}` | read/write page-table base | `mrs/msr TTBR0_EL1` |
| `invlpg [addr]` | invalidate one TLB entry | `TLBI VAE1` + `DSB`/`ISB` |
| `pushfq; pop; cli` | save flags + disable IRQs | `mrs {}, DAIF` + `msr DAIFSet, #2` |
| `push; popfq` | restore IRQ flags | `msr DAIF, {}` |
| (context switch reg save) | callee-saved + PC/SP/page-base | x19-x30, SP, `SPSR`/`ELR`, `TTBR0` |

## 1.1 The HAL contract (measured surface, categorized)

Extracting the *distinct* symbols behind those 126 references (`grep -hoE "arch::x86_64::[\w:]+"`)
gives ~90 names - but they fall into three very different buckets, and the split matters for scoping:

**(A) True arch primitives - reimplement per arch.** The irreducible hardware surface:

- **MMU:** `page_tables::{PageTable, VirtAddr, entry_for_va, unmap_4k_strided, reclaim_user_frames,
  get/set_hhdm_offset, harden_hhdm_nx}` -> VMSAv8-64 tables, `TTBR0/1`, broadcast `TLBI`.
- **Context switch:** `context_switch::TaskContext` -> x19-x30/SP/`ELR`/`SPSR`/`TTBR0`.
- **Syscall + user-copy:** `syscall_entry::{syscall_slot, USER_END, user_copy_active,
  clear_user_copy_active, init_percore_syscall_arena, init_percore_arenas}` -> `SVC` + `VBAR_EL1`,
  EL0/EL1 fault discrimination (the C1/C2/V1 twin).
- **Boot lifecycle:** `init, ap_init, ap_count, ap_boot::start_all_aps, halt_all_cores,
  hardware_reset, boot::{init_gdt_arenas, set_tss_rsp0, audit_wx}, BootInfo` -> PSCI/spin-table,
  `PSCI SYSTEM_RESET`, EL1 setup.
- **CPU + timer:** `read_cycle_counter, _rdtsc, __cpuid(_count), init_timer,
  boot::{tsc_ticks_per_quantum, TSC_DEADLINE_MODE, rearm_tsc_deadline, get_lapic_id,
  get_apic_virt_base}` -> `CNTPCT`/`CNTFRQ`, `MIDR`/ID regs, generic-timer compare.
- **IRQ flags + controller:** `disable_interrupts, wait_for_interrupt, interrupts::{send_eoi,
  idle_can_halt, fire_test_irq}, ioapic::{init, mask/unmask_vector, set_redir, set_level_route,
  set/bsp_lapic_id}` -> `DAIF`, GIC-400 distributor/CPU-interface, SGIs.
- **Serial byte in/out:** `serial_write_byte, serial_write_bytes_lockfree, com2_init,
  com2_try_read_byte, uart_rx_drain_fifo` -> PL011 MMIO. (Only the raw byte in/out is arch; see B.)

**(B) Misfiled arch-neutral logic - RELOCATE, do not reimplement.** These live in
`arch/x86_64/mod.rs` but are pure state machines with no x86 dependency beyond calling a (A) primitive:
`console_foreground_allows, claim/release_console_foreground(_if_owner), CONSOLE_READ_WAITER,
set_console_echo, console_boot_complete, console_write_bytes_gated, console_push_byte,
input_ready/set_input_ready`, and the `uart_rx_{pop, poll, drain_now}` ring buffer. Moving these to a
neutral `kernel/src/console.rs` (calling arch only for the actual byte) **shrinks the arch contract by
~15 symbols** and is a safe, compile-verifiable refactor that pays off on *every* future arch. A
genuine simplification the survey surfaced (§26.13).

**(C) Optional / board subsystems - stub or board-specific, not blocking the core:**

- `iommu::{detect, bringup, confine_device, release_device, drain_event_log}` -> **no usable SMMU on
  the Pi**, so these become no-ops and DMA drivers are trusted-on-machine (§6.4 already machine-dependent).
- `pci::{init, xhci_bios_handoff, program_xhci/ehci_msi, route_ehci_intx, ehci_flr_probe, *_BDF,
  NIC_*}` -> Pi 4 has a BCM2711 PCIe controller (for the VL805); a Pi 3 has none. Board-gated.
- `rtc::{read_datetime, now_epoch_monotonic, epoch_secs, boot_datetime, capture_boot_time}` ->
  **the Pi has no battery-backed RTC.** These degrade to "no wall clock" (date/uptime lose their RTC
  source; uptime can move to the generic timer, wall-clock date needs NTP or a DS3231 add-on later).
  A real, board-level gap to design for - not a blocker for the identity suite.
- `fb_commit` + a `bootcon::init(FbParams{..})` (the framebuffer slice, its PHYSICAL base for the
  `console` service's grant, geometry, channel shifts) -> Limine framebuffer vs the Pi VideoCore mailbox
  framebuffer. (`fb::dims_packed` is gone: terminal geometry belongs to the `console` service, not the
  kernel - `docs/console-service.md` 9.7.)

**Scoping takeaway:** the true per-arch reimplementation (bucket A) is ~40 symbols in the well-known
categories above; ~15 (bucket B) are a one-time neutral relocation that helps every arch; and bucket C
is stub-or-defer. That is the real size of "supporting the architecture."

## 2. Phase 0 - seal the boundary on x86 FIRST (before any ARM)

> **Status (2026-07-14, `feat/aarch64-prep`): the seam, the bucket-A sweep, AND the asm isolation are
> DONE, and the boundary is now ENFORCED.**
> - **Seam + sweep:** `arch/mod.rs` exposes `imp` (a `#[cfg(target_arch)]` alias of the current arch
>   module); all **126** arch-neutral references swept `arch::x86_64::` -> `arch::imp::` (compiler-
>   guaranteed identical; identity 24/0).
> - **Asm isolation:** all **23** inline-asm sites in the neutral layers (`smp/`, `memory/`, `task/`,
>   `main.rs`) moved behind `arch::imp` primitives - `read/write_page_table_base` (CR3), `invalidate_tlb_page`
>   (invlpg), `local_irq_save/restore` (pushfq;cli/sti), `switch_to_boot_stack` (rsp), plus the existing
>   `enable/disable_interrupts`. The `unsafe` asm consolidated into the permitted arch layer
>   (audits/unsafe-audit.md); the host lib gets a no-op `arch::imp` stub (lib.rs). Identity 24/0.
> - **Enforcement:** `scripts/arch_boundary_check.py` (CI-wired, alongside `unsafe_check`/`contract_check`)
>   FAILS on any `asm!`/`naked_asm!`, any named-arch reference (`arch::x86_64::` etc.), OR any
>   `core::arch::<arch>::` intrinsic (e.g. `__cpuid`) outside `kernel/src/arch/`. So the demarcation
>   cannot silently rot: a future RISC-V/AArch64 port is BOUNDED by construction - implement
>   `arch/<new>/` to the `imp` surface, touch zero neutral files, and CI guarantees no neutral file
>   smuggled in arch-specific code (asm, named-arch module, or intrinsic).
> - **Clear failure for a not-yet-built arch:** `kernel/src/arch/aarch64/mod.rs` is a `#[cfg]`-gated
>   stub with a `compile_error!` that names the plan and the surface to fill. A `--target
>   aarch64-unknown-none` build fails with that message instead of a cryptic "file not found for module
>   aarch64"; it is inert on the x86 build (byte-identical binary).
>
> - **IPI-send extraction (2026-07-14):** `smp/ipi.rs` was the last file in a *permitted* layer still
>   holding APIC MMIO (the ICR programming for a targeted `send_ipi` + the shootdown broadcast). Moved to
>   `arch/x86_64/boot.rs` as `send_ipi_to_lapic(lapic_id, vector)` + `broadcast_ipi_all_but_self(vector)`;
>   `smp/ipi.rs` now resolves core->LAPIC and holds only the neutral shootdown *protocol* (per-core ack
>   masks, request/wait), calling the arch seam for the actual send. **`smp/ipi.rs` is now APIC-MMIO-free
>   - arch owns ALL hardware MMIO.** Identity 24/0 (9A cross-core IPC + the shootdown exercise the moved
>   paths). So the boundedness claim is now clean: a port reimplements `arch/`, full stop.
> - **Dash guard (2026-07-14):** `scripts/dash_check.py` (CI-wired) enforces CLAUDE.md §21 (ASCII hyphen
>   only, no em/en dash) mechanically instead of by hand-grepping each diff.
>
> **Remaining soft-spots (documented, not blocking):**
> - The IPI *vector numbers* (`WAKE_RECEIVER=0xF0`, `TLB_SHOOTDOWN=0xF1`, `SCHEDULER_TICK=0xF2` in
>   `smp/ipi::vectors`) are x86 IDT vectors passed through to `arch::imp`. A GIC port maps the IPI *kind*
>   to an SGI id (0-15), so those three numbers want to become abstract kinds that arch resolves - a
>   small change best finalized alongside the GIC impl (rule of three), not now.
> - **Bucket-B relocation** (§1.1): the misfiled arch-*neutral* console/UART state machines in
>   `arch/x86_64/mod.rs` -> a neutral module. Shrinks the arch *implementation* file; does not affect
>   boundedness (neutral either way). Pure code-motion, deferred for a live operator.

Do the de-x86-ification as a refactor on the x86 side, verified by the identity suite (24/24 = zero
behavior change), *before* writing AArch64. Then adding `arch/aarch64/` is "implement the same surface"
instead of "also patch 126 call sites while debugging on hardware you can't see." It is 100 % on the
existing x86 target, needs no Pi, and does not touch `main`.

**Design fork (pick one):**

- **cfg-module alias** - `arch/mod.rs` selects `x86_64` or `aarch64` as `imp` via
  `#[cfg(target_arch)]`; call sites become `arch::imp::...` (or a flat re-export `arch::...`). Minimal,
  boring, mechanical - a large but low-risk sweep of the 126 sites. **Recommended for v1** (§26.13:
  discipline over cleverness; smaller and boringer wins).
- **`Arch` HAL trait** - define the ~40-operation surface as a trait, one impl per arch, call through
  it. Cleaner long-term boundary, more upfront design, easier to enforce "no arch leak" (the trait *is*
  the contract). A reasonable later refinement once two arches exist to generalize from.

Either way the surface to formalize is the bucket-A list in §1.1.

**Safe execution order (each step compile- and identity-verifiable on x86, no big-bang):**

1. **Relocate bucket B** (§1.1) - move the console/foreground/echo/input-ready/UART-ring state machines
   out of `arch/x86_64/mod.rs` into a neutral `kernel/src/console.rs`, calling arch only for the raw
   byte. Shrinks the contract ~15 symbols; pure code motion, zero behavior change.
2. **Introduce the seam** - `arch/mod.rs` selects the arch impl (cfg-alias) or defines the `Arch` trait.
3. **Sweep the remaining bucket-A references** through the seam, in reviewable chunks (per subsystem:
   scheduler, syscall, smp, memory, loader), with an identity run between chunks - not one 126-site
   commit. The 23 inlined asm ops in `smp/`+`memory/` become calls to arch primitives in this step.
4. **Verify:** identity 24/24 (no behavior change) is the gate for each chunk.

## 3. The AArch64 arch layer (`arch/aarch64/`, what Phase 1+ implements)

Mapped from the x86 surface, in dependency order:

1. **Boot + early init.** Entry at the firmware's load address; set up SP, clear BSS, get RAM size +
   framebuffer. Two boot-path options (section 5).
2. **MMU.** AArch64 translation tables: `TTBR0_EL1`/`TTBR1_EL1` split, the VMSAv8-64 descriptor format
   (different bits than x86 PTEs), memory attributes via `MAIR_EL1`, granule/size via `TCR_EL1`, ASIDs.
   TLB maintenance is `TLBI` + `DSB ISB` barriers, and it **broadcasts** across cores - which
   *simplifies* the shootdown path (often no IPI needed vs the x86 IPI shootdown). W^X and the
   kstack-guard map cleanly onto the descriptor AP/UXN/PXN bits.
3. **Exceptions + syscalls.** A single vector table at `VBAR_EL1` (16 entries: sync/IRQ/FIQ/SError x
   current/lower EL x width). Syscalls are the `SVC` instruction -> a synchronous exception. **This is
   where the recent C1/C2/K3/A14 hardening has its twin:** "ring-3 fault kills the task, ring-0 halts"
   becomes "was the exception from EL0 or EL1" (read `SPSR_EL1.M`). Re-establish - do not re-audit - the
   fault-kills-the-task-not-the-kernel invariant in the AArch64 sync-exception handler.
4. **Context switch.** Save/restore x19-x30, SP, `ELR_EL1`/`SPSR_EL1`, `TTBR0_EL1` (the address space);
   FP/SIMD state if used. The naked-fn shape carries; the register set changes.
5. **Interrupt controller: GIC-400 (GICv2 on the Pi 4).** Distributor + CPU interface; IPIs are
   **SGIs**. Replaces LAPIC/IOAPIC + the ICR-based IPI. More standard than the older Pi's BCM controller.
6. **Timer: the ARM generic timer.** `CNTFRQ_EL0` gives a known frequency, `CNTP_TVAL`/`CNTP_CTL` drive
   the tick. This *removes* the x86 TSC-calibration pain (the AMD `CPUID 0x15/0x16` mess on the T630).
7. **UART: PL011** (the Pi's primary UART). Small MMIO backend for `serial_write_byte` and RX.
8. **SMP bring-up: PSCI** (`CPU_ON` via `SMC`/`HVC`) on the Pi 4 firmware, or the spin-table fallback.
   Replaces the x86 real-mode INIT+SIPI trampoline (cleaner - no real-mode).

## 4. Board specifics - Raspberry Pi 4 Model B (BCM2711)

Confirm the physical board first: **Pi 4** = two micro-HDMI, USB-C power, 2xUSB3 + 2xUSB2. (There is no
4 GB Pi 3 - a 4 GB board is a Pi 4.)

- **Peripheral base `0xFE000000`** (BCM2711 low-peripheral mode); be aware of low- vs high-peripheral
  addressing.
- **GIC-400 (GICv2)** - a standard GIC, unlike the older Pi's bespoke BCM interrupt controller.
- **Ethernet = GENET**, a **memory-mapped** gigabit NIC (not USB-attached). So **net-stack does NOT
  gate on USB** - bring the network up first, independently. GENET is a new userspace driver (not
  RTL8168/e1000), but it is MMIO + DMA rings, the shape `nic-driver` already knows.
- **USB3 = a VL805 xHCI behind the BCM2711 PCIe.** Bring up a **PCIe controller** first (new), then
  **the existing `xhci` driver has a real shot at porting** - it is spec-based (drove QEMU qemu-xhci and
  the T630 controller). That replaces "write DWC2 from scratch" (the older Pi's long pole) with "PCIe +
  reuse xhci."
- **Storage = SD/EMMC** (no SATA/AHCI). `block-driver` becomes an EMMC driver, or USB mass storage once
  xhci is up.
- **4 GB + DMA ranges.** Some legacy peripherals can only DMA into the low 1 GB (bus addresses), so
  their DMA arenas must live in low memory. Fits the existing "reserved DMA arena per driver" model -
  just constrain where the arena is allocated.
- **No usable SMMU for these peripherals**, so **H1/§6.4 does not travel**: DMA-capable drivers go back
  to trusted-on-this-machine, announced loudly at boot (the machine-dependent posture the spec already
  allows). The same binary is least-privilege where an IOMMU confines it and trust-critical where none
  does - now literally true across x86-with-IOMMU and this Pi.

## 5. Boot path decision (open)

- **UEFI + Limine-aarch64.** The Pi 4 UEFI firmware (TianoCore) is mature. Keeps the handoff shape
  **identical to x86** - memory map, framebuffer, SMP topology handed over, minimal new parsing.
  Preserves the "arch layer is a reimplementation, not a new world" framing. Slightly off the stock Pi
  path (requires the RPi4 UEFI firmware on the SD card).
- **Bare GPU bootloader + DTB.** Stock Pi path: the VideoCore firmware loads `kernel8.img` and jumps to
  `0x80000` with the DTB pointer in `x0`. You get RAM size + framebuffer from the **VideoCore mailbox
  property interface** and hardcode the single known peripheral base - so full Device-Tree parsing can
  be deferred. No Limine dependency.

**Lean: UEFI + Limine-aarch64 if the firmware cooperates**, to keep the handoff identical to x86.

## 6. Bring-up order

1. Boot handoff (UEFI+Limine or GPU+DTB) -> reach `kernel_main` with a memory map.
2. GIC + generic timer + MMU + EL0/EL1 exceptions + PL011 UART.
3. SMP via PSCI (all 4 A72 cores ready).
4. **Identity suite green on the arch core** - this is the definition of "the port is done", because
   everything the 24 tests exercise above the arch line is already-hardened code.
5. Drivers, in this order: **GENET (network first, USB-independent)** -> **PCIe** -> **xhci reuse** ->
   **EMMC**.

## 7. Constitution amendments needed (before this is normative)

The spec is written single-arch in a few places; adding AArch64 turns these into "on x86 ...; on
AArch64 the analog is ...", with the rationale in the commit (§21):

- **§11.2 / Appendix A** - the Limine + real-mode INIT+SIPI trampoline is x86-specific; AArch64 uses
  PSCI/spin-table and (optionally) Limine-aarch64.
- **§6.4 (H1 IOMMU)** - AMD-Vi is x86-specific; on the Pi 4 there is no usable SMMU, so DMA drivers are
  trusted-on-this-machine (the machine-dependent posture already generalizes).
- **§9 / §10 arch notes** - CR3->TTBR, IPI-shootdown -> broadcast TLBI, ring 0/3 -> EL0/EL1.

## 8. What is NOT re-audited

The point of the pre-port audits: the arch-neutral layers do not get re-audited per arch. When the 24
identity tests pass on the Pi 4, the capability model, IPC, restartability, and every service's business
logic are the same code that already passed on x86 and hardware-soaked on the T630. The port's risk is
entirely in the arch layer and the new board drivers - which is where this plan concentrates the effort.
