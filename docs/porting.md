# Adding an ISA: the seam, the edges, and the rule

This is the honest map. It tells you what you must write, what you will unavoidably touch outside
that, and how to tell the difference between the two without asking anyone.

**The rule, and everything else on this page exists to make it usable:**

> Write `kernel/src/arch/<isa>/`. Touch the five wiring files listed under
> [The seam](#the-seam-what-you-write). Expect to touch the [edges](#the-edges-what-you-will-touch-anyway).
> **If you find yourself editing anything else, stop and ask why.**

That last sentence is the whole point. It is not a warning about carelessness - it is a diagnostic.
Every file outside the seam that a port has to edit is a place where some earlier port left an
assumption behind, and the right response is to fix the assumption rather than to add your ISA to a
list. Six checkers exist to tell you when that is happening; they are in
[How you know where you are](#how-you-know-where-you-are). One of them,
`scripts/port_scope_check.py`, enforces this paragraph literally: it reads the tree below, works out
from your own diff that you are adding an ISA, and names every file you touched that the tree marks
do-not-touch.

---

## The seam: what you write

**One directory, and five wiring files.** The directory is the work; the five below are a line or a
block each, and none of them is a neutral kernel file.

**The directory.** **`kernel/src/arch/<isa>/`** - the implementation. Start from the nearest existing stub
   (`riscv32/`, `loongarch64/` and `s390x/` are deliberately kept as three DIFFERENT probes: a
   32-bit one, a clean 64-bit control, and a big-endian one). This is the only directory in the
   kernel where new `unsafe` and inline `asm!` belong (CLAUDE.md §18.1).

**And the five wiring files.**

1. **`kernel/src/arch/mod.rs`** - two `#[cfg(target_arch = "<isa>")]` lines: `pub mod <isa>;` and
   `pub use <isa> as imp;`.

2. **`kernel/kernel-<isa>.ld`** - a new linker script for your load address and PHDRS.

3. **`kernel/build.rs`** - a target-matching block that passes `-T` for that script.

4. **`.cargo/config.toml`** - a `[target.<triple>]` block with the rustflags your target needs.

5. **`rust-toolchain.toml`** - add your triple to `targets` if a shipping build will need it.

### What the seam actually is

`arch::imp` is **131 members** that the neutral kernel calls. You do not get a hand-written list of
them, deliberately: `scripts/arch_seam_check.py` DISCOVERS the set from what neutral code actually
uses, so it cannot go stale the way a checklist does. Run it and it names every member your arch has
not answered yet.

The compiler is the other half of the same answer. `cargo check -p kernel --target <triple>` inside
`arch/<isa>/` is just your stub being incomplete - the errors are the surface you still owe. **An
error OUTSIDE `arch/<isa>/` is a different thing entirely: it is a boundary leak, and the fix is a
new `arch::imp` member, not a special case at the call site.**

### Before you write the trap handler

`arch/<isa>/` **is** kernel, so a mistake there does not kill a service - it kills the machine, and
three of the four ways it does so are SILENT. Read
[**"How an arch implementation HALTS THE MACHINE"**](../kernel/src/arch/CLAUDE.md) before the trap
handler, not after the first mystery halt. It covers, with the incident that taught each:

- a fault report that can itself fault (re-enters forever: no output, no panic, one core dark);
- a hardware wait with no bound, or a bound whose result nobody reads;
- code published as DATA with no instruction-cache sync (executes a dead service's text, and only on
  RESPAWNS, so boot looks fine);
- a watchdog quantum stubbed to `0`, which does not mean "no limit" - it meant no wedge detection at
  all for a whole port's bring-up;
- a `halt_all_cores` that halts only the caller.

The three scaffold stubs carry the warning at the exact function that causes each, so a port
inherits it rather than rediscovering it.

### Two rules the boundary rests on

- **No inline asm and no named-arch reference outside `arch/`.** `scripts/arch_boundary_check.py`
  fails the build on `asm!`/`naked_asm!`, `arch::<specific>::`, or `core::arch::<specific>::` in any
  neutral kernel file. Its arch list is derived from the directory listing, so your arch is covered
  the moment its directory exists and there is nothing to remember.
- **Never `core::sync::atomic::AtomicU64` - use `portable_atomic::AtomicU64`.** RV32A has no 64-bit
  atomic, so the `core` type does not exist there. That one dependency is the entire cost of 32-bit
  support.

---

## The edges: what you will touch anyway

Zero was the goal and zero is not reached: there are **46 arch-conditional sites outside `arch/`**. They
are listed here by kind, with what each would take to close, because a number without a reason is
just a number. The per-file counts are `SHARED-SURFACE.baseline.txt`, which the ratchet owns.

### 2 in the neutral kernel - both deliberate, neither is an ISA question

`kernel/src/task/scheduler.rs` picks `TASK_HEAP_VA_START` on `target_pointer_width`. A 32-bit address
space cannot hold a 4 GiB virtual address; the width IS the question, and no seam member would
improve it. **If you are 64-bit, you touch nothing here. If you are 32-bit, you touch nothing here
either** - the existing arm gives you the answer.

This was 14 sites before 2026-09-13. The other 12 were per-device interrupt vectors, the panic-path
serial flush, the framebuffer memory type and the sub-tick sleep timer - all now seam members, all
answered by every arch including the scaffolds.

### 11 in the SDK - the designated seam, and mostly correct

| what | why it is here |
|------|----------------|
| `sdk/rust/src/syscall.rs` (4) | `raw_syscall`, one body per ISA. A trap instruction and its register convention are properties of the instruction set and of nothing else. §18.1 names this file by hand. **You will write one of these. It is expected.** |
| `sdk/rust/src/adversarial.rs` (6) | Deliberate ring-3 faults for §22 A14/C2 - a non-canonical read and a trapping divide, which have no portable spelling. **You do not need these to boot**; see `backlog/24` for why none of them currently runs off x86 anyway. |
| `sdk/rust/src/ipc.rs` (1) | A timeout clamp on `target_pointer_width`, not on your ISA. If you are 64-bit it does not apply; if you are 32-bit it already covers you. |

### 33 above the kernel - where the real work is left

**Read this part before you assume a booting kernel means you are done.** CLAUDE.md §4.1: *an ISA
port is not complete when it boots. It is complete when architecture-neutral code no longer knows
that the ISA was added.*

| where | what it asks | what you must do |
|-------|--------------|------------------|
| `services/supervisor/build.rs` (9) | which USB host, which service images, whether PCI config space is reachable | **Add one arm to each table.** This is the designed place to answer, and it sets `has_xhci` / `has_dwc2` / `has_hw_enumerator` so `main.rs` needs nothing. |
| `services/block-driver/build.rs` (4) | is the disk on a USB host, and which service owns it | **Add one arm.** Same shape. |
| `services/shell/build.rs` (2) | what to call your arch in `version` | **Nothing** - it is derived from `CARGO_CFG_TARGET_ARCH`. Only add a line if your ISA needs a project-specific name, as arm32 does. |
| `services/supervisor/src/main.rs` (4) | which peers `block-driver` and `nic-driver` need | **Add an arm** if your storage or NIC sits behind a USB host. |
| `services/nic-driver/src/main.rs` (10) | which MAC driver to run | **Add an arm, and know that this is the worst one.** Three of the four PORTS pick their NIC by instruction set, which fails on its own terms: a different NIC on a board of the same ISA drives the wrong silicon. `backlog/21` has the fix (a kernel query reporting which controller the boot probe found) and the reason it is not done. |
| `services/hw-enumerator/src/main.rs` (3) | mechanism #1 or ECAM config-space selector | **Add an arm** if you have PCI. A port that is neither fails to COMPILE, which is deliberate - a wrong default would silently address the wrong registers. `backlog/25` has the clean form. |
| `services/net-stack/src/main.rs` (1) | is this counter a CPU cycle count or a wall clock | **Nothing.** The default is the wall-clock floor, which is what every non-x86 port has turned out to need. If calibration fails it now says so in one line rather than surfacing as "ping feels slow" three layers away. |

---

## The whole map, as a tree

Every file below is either something you write, something you add one line to, or something you do
not touch. The counts are arch-conditional sites, and they come from
`SHARED-SURFACE.baseline.txt` - the ratchet's own file - so this tree cannot quietly drift from
what is enforced. `scripts/facts_check.py` compares the two on every run.

```text
godspeed/
├── kernel/
│   ├── src/
│   │   ├── arch/
│   │   │   ├── <isa>/                              ★ YOU WRITE THIS, and essentially only this.
│   │   │   │                                         131 `arch::imp` members; the compiler and
│   │   │   │                                         arch_seam_check.py name every one you owe.
│   │   │   └── mod.rs                              + 2 lines: `pub mod <isa>;`
│   │   │                                                      `pub use <isa> as imp;`
│   │   ├── task/scheduler.rs               [ 2 ]   - NOT yours. `target_pointer_width` on one
│   │   │                                             constant; a 32-bit space cannot hold a 4 GiB
│   │   │                                             VA, so the width IS the question.
│   │   └── everything else                 [ 0 ]   - do not touch. If you must, that is a finding.
│   ├── kernel-<isa>.ld                             + new file: your load address and PHDRS
│   └── build.rs                                    + one target block, passing -T for it
├── .cargo/config.toml                              + one [target.<triple>] block
├── rust-toolchain.toml                             + your triple, if a shipping build needs it
│
├── sdk/rust/src/                                   the seam CLAUDE.md 18.1 designates by hand
│   ├── syscall.rs                          [ 4 ]   + YOUR `raw_syscall`. One body per ISA: the trap
│   │                                                 instruction and its register convention. This
│   │                                                 one you WILL write, and it is expected.
│   ├── adversarial.rs                      [ 6 ]   - not needed to boot. §22 fault primitives; see
│   │                                                 backlog/24 for why none runs off x86 anyway.
│   └── ipc.rs                              [ 1 ]   - nothing. Keys on register width, not on you.
│
└── services/                                       above the kernel, where the real work is left
    ├── supervisor/build.rs                 [ 9 ]   + ONE ARM per table. The designed place to
    │                                                 answer; it sets has_xhci / has_dwc2 /
    │                                                 has_hw_enumerator so main.rs needs nothing.
    ├── supervisor/src/main.rs              [ 4 ]   + one arm IF your storage or NIC sits behind a
    │                                                 USB host (block-driver / nic-driver peers).
    ├── block-driver/build.rs               [ 4 ]   + one arm: is the disk on USB, and whose host.
    ├── nic-driver/src/main.rs              [10 ]   + one arm, AND KNOW THIS IS THE WORST ONE. It
    │                                                 picks the MAC by instruction set, so a
    │                                                 different NIC on a board of YOUR ISA drives
    │                                                 the wrong silicon. backlog/21 has the fix.
    ├── hw-enumerator/src/main.rs           [ 3 ]   + one arm IF you have PCI. A port that is
    │                                                 neither mechanism #1 nor ECAM fails to
    │                                                 COMPILE, deliberately. backlog/25.
    ├── shell/build.rs                      [ 2 ]   - nothing. Derived from CARGO_CFG_TARGET_ARCH.
    │                                                 Add a line only for a project-specific name,
    │                                                 as arm32 has.
    ├── net-stack/src/main.rs               [ 1 ]   - nothing. The default is the wall-clock TSC
    │                                                 floor, which every non-x86 port has needed.
    └── every other service                 [ 0 ]   - do not touch.
```

**Legend.** `★` write it. `+` add to it, and the guide above says what. `-` do not, and if you find
yourself doing so, that is [the rule](#adding-an-isa-the-seam-the-edges-and-the-rule).

**Totals, and they are the honest ones.** 46 arch-conditional sites outside `arch/`: 2 in the neutral
kernel and 44 above it. Of the 44, **15 are "add one arm to a build table"** - designed, expected,
and cheap. The other 29 split into the SDK's syscall body you will write anyway (4), the seam the SDK
is designated for (7), and 18 across FOUR service files - `nic-driver` (10), `supervisor/src/main.rs`
(4), `hw-enumerator` (3) and `net-stack` (1). Two of those four have an open backlog entry saying what
would close them for good: `backlog/21` for `nic-driver` and `backlog/25` for `hw-enumerator`. The
other two do not, and that is honest rather than an omission - the supervisor's four are peer lists
whose only alternative is a second copy of a fact `block-driver/build.rs` already owns, and
`net-stack`'s one needs nothing from a porter at all.

## How you know where you are

Six checkers, and each answers a different question. None of them is optional and all of them run
against your tree without hardware.

| run this | it tells you |
|----------|--------------|
| `python scripts/arch_seam_check.py` | which `arch::imp` members your arch has not answered yet |
| `python scripts/arch_boundary_check.py` | whether neutral kernel code names an ISA or contains asm |
| `python scripts/shared_surface_check.py` | whether you GREW the arch-conditional surface. It refuses the build and names the file. It counts arch-conditional SITES, so it sees an added `#[cfg(target_arch)]` and nothing else - an ordinary edit to a neutral file adds no site and passes |
| `python scripts/port_scope_check.py` | **whether you edited anything outside the scope above.** This is the one that catches "I edited something I should not have", and until 2026-09-13 nothing did: the four checkers around it all ask whether a RULE was broken, and an ordinary edit to `kernel/src/ipc/routing.rs` breaks none of them. It reads the tree on this page, notices from your diff that you are adding an ISA, and names every out-of-scope file with the reason this page gives. An unavoidable edit is cleared by a `Port-Scope: <path> - <reason>` trailer on any commit, which is this page's "write down why" made mechanical |
| `python scripts/scaffold_check.py` | how far a fresh ISA actually gets with only `arch/<isa>/` written - the bounded-port test itself |
| `python scripts/line_ending_check.py` | whether a file a BOOTLOADER reads has picked up CRLF. A Windows checkout produces it silently, and U-Boot then reads the trailing CR as part of every FILENAME - a perfect menu that boots nothing (`backlog/26`) |

The first two prove no rule is broken. `scaffold_check` is different in kind: it BUILDS the scaffold
arches and reports how far each got, because a count is a proxy and a build is not. Its ladder:

    M1  compiles     the kernel builds for the target: every seam member answered
    M2  boots        reaches its UART under QEMU and prints
    M3  kernel up    neutral kernel steady state - memory, scheduler, IPC, capabilities
    M4  userspace    SPAWNS THE SUPERVISOR

Only M1 is measured today, and the script says so rather than assuming the rest. M4 is the real bar,
because bringing up userspace is the kernel's whole job and it is where the above-kernel surface first
bites.

---

## When you find yourself editing something else

Three questions, in order:

1. **Is this a property of the DEVICE or of somebody's design?** (CLAUDE.md §26.14.) Register order,
   timing, magic values, what a bit means - the device, and it belongs in `arch/<isa>/`. Where state
   lives, who owns it, what happens on failure - ours, and a cfg is almost never the answer.

2. **Is the ISA really the question, or is it standing in for the BOARD?** Above the kernel it is
   almost always the board: "which USB host is my disk behind" is not "which instruction set am I".
   A board question belongs in a `build.rs` table where it is asked ONCE, or better, in a runtime
   fact the kernel already knows and could report.

3. **Would a second board of MY OWN ISA break this?** If yes, the axis is wrong and adding your arm
   makes it worse for the next person, not better. Say so in the commit, or record it in `backlog/`
   rather than papering over it (§26.7).

If the honest answer is that the edit is unavoidable, make it - and **write down why, at the place a
reader would otherwise wonder.** Every edge listed above was once somebody deciding that, and the
ones that carry their reason are the ones that got fixed later.

---

## See also

- `kernel/src/arch/CLAUDE.md` - the implementer's reference: the public surface in detail, the SMP
  memory-ordering contract, per-arch bring-up notes found by actually booting, and the
  driver-porting doctrine.
- `docs/multi-arch.md` - the evidence: which ISAs boot, on what hardware, with what proof.
- `CLAUDE.md` §4.1 - the law this page serves, and the standing figure.
