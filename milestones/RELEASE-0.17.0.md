# 🧭 v0.17.0 - a port is bounded to one directory, and now it is measured

*Prepared release note. Use as the annotated tag message at merge:*
`git tag -a v0.17.0 -F milestones/RELEASE-0.17.0.md`

---

🧭 v0.17.0 - a port is bounded to one directory, and now it is measured

CLAUDE.md 4.1 sets the bar for an architecture port: **not complete when it boots, complete
when architecture-neutral code no longer knows that the ISA was added.** Nothing measured
that. This release makes it measurable, moves the number hard, and then says plainly what is
still unproven. 🔬

## 📉 What a fifth ISA costs: before and after

Measured against the branch point with the same rulers, not remembered.

| | before | after |
| --- | --- | --- |
| arch-conditional sites outside `arch/` | **143** | **46** 🎉 |
| of which, in the NEUTRAL KERNEL | 14 | **2** |
| files involved | 16 | **11** (1 kernel, 3 SDK, 7 services) |
| `services/supervisor/src/main.rs` | 49 | 4 |
| `services/nic-driver/src/main.rs` | 26 | 10 |
| `services/block-driver/` (3 files) | 21 | 0 |
| `services/shell/src/main.rs` | 7 | 0 |
| `arch::imp` seam members, all answered by all ports | 122 | 131 |
| checker scripts (`scripts/*check*.py`) | 11 | 13 |
| a test that BUILDS a fresh ISA | none | `scripts/scaffold_check.py` ✨ |
| `unsafe` in services, examples, osdev | 0, by grep | 0, refused by rustc in 34 crates 🔒 |

The two sites left in the neutral kernel are both `target_pointer_width` on one constant,
where a 32-bit address space genuinely cannot hold a 4 GiB virtual address. They are counted
rather than exempted, so a reader can see them and see why. 👀

## 💡 How the reduction was found

The number is not the interesting part. Above the kernel, `target_arch` almost never means
"which instruction set" - it means **"which BOARD am I on"**, and those two come apart.
`nic-driver` picks its MAC by ISA, so a different NIC on a board of the same ISA drives the
wrong silicon. The supervisor asked "am I ARM?" when it meant "does my disk hang off a USB
host?". Five per-arch `USB_IMAGES` tables lived in one file because nobody had named the fact
they all encoded. Name each fact once, in a `build.rs` table or a seam member every arch
answers, and the arms collapse. 🧹

## 🆕 New in this release

- 🗺️ **`docs/porting.md`** - the map. What you write (one directory plus five wiring files), a
  tree marking every file write-it / add-to-it / do-not-touch with its count, all 46 edges by
  kind with what each would take to close, the five checkers and what each answers, and the
  rule: *if you find yourself editing anything else, stop and ask why.* Its numbers AND its
  tree are verified against the ratchet in both directions, so a wrong count fails and a
  ratcheted file missing from the tree fails too.
- 🧪 **`scripts/scaffold_check.py`** - the bounded-port test. Builds three scaffold ISAs
  (32-bit, 64-bit control, big-endian) and reports how far each gets, because a count is a
  proxy and a build is not.
- 🩺 **`scripts/line_ending_check.py`** - a boot config that picks up CRLF fails the build. A
  CRLF `extlinux.conf` renders a perfect U-Boot menu and boots nothing, because the trailing
  carriage return becomes part of every filename. Cost two card reflashes to learn. 😅
- 🔒 **`#![deny(unsafe_code)]`** on all 34 crates under `services/`, `examples/` and `osdev/`.
  18.2 already forbade unsafe there; the compiler enforces it now rather than a grep noticing
  afterwards.
- 📏 `shared_surface_check.py` covers the NEUTRAL KERNEL, which had no ratchet at all while
  userspace had one.
- ⚠️ **"How an arch implementation HALTS THE MACHINE"** in `kernel/src/arch/CLAUDE.md` - five
  ways the arch layer takes the whole machine down, each with the incident that taught it, and
  the warnings written onto the three scaffold stubs at the exact function that causes each.

## ⚙️ Behaviour changes, all hardware-confirmed

- NET_DEVICE (syscalls 42-44) is no longer granted to any service. It had had no caller since
  GENET moved into `nic-driver` in 2026-08.
- riscv64 spawns its USB host before the disk that lives on it, so the first capacity probe no
  longer fails and recovers.
- The TSC floor defaults to the wall-clock value with x86 opting in. Three ports had been
  silently rejected by an x86-shaped constant; every failure path in that calibration is now
  loud and the retry is bounded.
- `ehci` is no longer reachable on riscv64, which had fallen inside a `not(any(arm, aarch64))`
  gate on a board that has never had an EHCI image.
- The kernel boot banner and the shell's `version` now agree on the Pi 2: both say `arm32`.
  They used to disagree, `arm` and `arm32`, in the two lines whose whole job is to identify
  the machine.

## 🖥️ Validated on all five machines

Each with three self-check runs, a hundred rounds of `chaos max-carnage`, hot-plug, ping and
filesystem work.

| machine | ISA | selfcheck | chaos | faults |
| --- | --- | --- | --- | --- |
| 🍓 Raspberry Pi 2 | arm32 | 452 / 0 | 100 rounds, 554 kills | 0 ✅ |
| 🍓 Raspberry Pi 4 | aarch64 | 461 / 0 | 100 rounds, 626 kills | 0 ✅ |
| 🌟 VisionFive 2 Lite | riscv64 | 461 / 0 | 100 rounds, 583 kills | 0 ✅ |
| 🖧 HP T630 | x86-64 (AMD) | 461 / 0 | 100 rounds, 658 kills | 0 ✅ |
| 🖧 Dell Wyse 5070 | x86-64 (Intel) | 461 / 0 | 100 rounds, 584 kills | 0 ✅ |

**Four of the five report the same count**, and that is not a coincidence: the shared surface
shrank far enough that the same tests exist on all of them. 🎯 The Pi 2 differs because it
embeds a smaller service set.

## 🚧 What is NOT proven

Stated here rather than left for someone to discover. The bounded-port test measures **M1
only**: a fresh ISA COMPILES. Whether one boots, reaches steady state, or spawns a supervisor
is listed and not asserted. And no new ISA was added in this release, so the surface was
shrunk rather than tested in the direction that matters.

The experiment that would settle it is named in `docs/porting.md`: drive `loongarch64` from M1
to M4 and count every file outside its own directory that has to change. That number is the
answer, and nothing else is. 🔭
