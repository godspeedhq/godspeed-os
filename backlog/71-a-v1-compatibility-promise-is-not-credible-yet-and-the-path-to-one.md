# 71. A v1 "no breaking changes" promise is not credible yet - and the path to one: reshape, freeze, gate, soak

**Status: OPEN - a decision recorded 2026-10-02, from the operator's question "can I safely release a v1
and promise no breaking changes in the future?". The answer was not yet, and why; this item is the plan.
The bar and the order were settled 2026-10-11 (below). Built so far: the one-way ratchet (step 3, half),
every service on `gs` (step 2, all but the driver gap).**

## Why not yet, measured rather than felt

The test of a compatibility promise is whether the surface it covers has stopped moving. In the two days
before this was written, on one branch (`feat/wifi-driver`), each of these would have broken someone under
a v1 promise:

- **The shell's behaviour:** `wifi list | match WPA2` became a refusal (records take `where`), `wifi list |
  sort` began to require a column, `wifi | count` began to refuse.
- **Service protocols:** `time`'s `OP_SET` and `net-stack`'s op 10 were retired; every radio request gained a
  tag (`docs/wifi.md` 58); the record wire codec gained a cell type (`Value::Signed`, tag 3).
- **The SDK:** the reply-cap reclaim changed on twelve paths (`backlog/67`) - a soundness fix that changed
  behaviour; two syscalls (`DevicePower` 54, `CpuClock` 55) and a privilege bit arrived.
- **What is planned would move it again:** the stdlib/SDK reshape below changes the public surface.

No mature project promises "no breaking changes" without exceptions either: Rust's own guarantee excludes
soundness and security fixes, and `backlog/67` was exactly one of those.

## The stdlib/SDK question it depends on

Asked the same day: why can drivers not use the standard library? **The answer the operator settled on
(2026-10-05): they must - there is one way to write a service or a driver, and it is `gs`.** A driver is
written on `gs` like any other service, and where it needs a hardware mechanism `gs::driver` does not have
yet, that is a gap in `gs::driver` to close (`docs/driver-library.md`), not a reason to reach past it. The
SDK stays the internal layer that holds the audited `unsafe` (CLAUDE.md 18.1) - `gs` is
`#![deny(unsafe_code)]` - and is free to change underneath. Today MMIO and DMA accessors are still reached
through the SDK because `gs::driver` has no safe mechanism for them yet; that is recorded as the gap, not
as the rule. (This paragraph used to say the opposite - that hardware forces the SDK and an SDK import marks
hardware code - and that reading steered new code toward the raw SDK, so it is replaced rather than kept.
`docs/stdlib-design.md`'s "MMIO and DMA stay in the SDK, permanently" is superseded in part, below.)

The proposal was first written here as a wholesale re-export of the SDK's safe hardware wrappers under
`gs::driver`. **The operator's guidance the same day replaced that with a slower and better method, now
adopted in `docs/driver-library.md`:** `gs::driver` holds DEVICE-NEUTRAL mechanisms only (never a
device-class API), each one added after it is found repeated in real drivers, with Wi-Fi discovering and
audio as the independent test; a driver that seems to need `unsafe` asks which safe mechanism is missing.
Two mechanisms are built (`gs::driver::wait`, `gs::driver::delay`) and nine drivers converted to them (`docs/driver-library.md`). The end state is the same one this item
needs - drivers on `gs`, the SDK an internal layer free to change - reached one mechanism at a time rather
than by re-export. Domain libraries like `sdk/wifi` stay outside the standard library and build on
`gs::driver`. The `stdlib-design.md` section it moves is marked superseded in part, which is a recorded
design change, not a constitutional amendment: no `unsafe` moves.

## The bar, in the operator's words (2026-10-11)

> I want contributions to be easily grokable from api interfaces and stdlibs that don't change. Once it's
> robust enough then I can release v1.

So the promise is for the CONTRIBUTOR: someone should be able to write a service, a utility or a driver by
reading stable interfaces, without reading the kernel or the SDK. That has one consequence for step 1 below
that the earlier draft did not face. The draft leaves `gs::driver` OUT of the covered surface, but a driver
contributor works through `gs::driver` and, where it has no mechanism yet, the SDK's `ctx.mmio()` and
`ctx.dma_region()` (13 and 12 call sites across nine drivers on 2026-10-11). Under this bar, either the
built `gs::driver` mechanisms are covered, with the set allowed to GROW but not change, or driver
contributions are named as outside what v1 makes easy.

**Decided the same day: `gs::driver` is covered, grow-only.** The mechanisms it has (`wait`, `delay`,
`irq`) freeze with the rest of the surface; new mechanisms may be added, existing ones may not change.
That moves the driver gap into step 2: before the freeze, `gs::driver` gains the mechanisms drivers still
reach the SDK for, found repeated in real drivers as `docs/driver-library.md` requires. Two were found in
the 2026-10-11 survey:

- **An arena layout.** 107 `phys_at` calls across 17 files carve DMA arenas by hand-kept offset constants
  (`dwmac`'s `TX_RING_OFF`, and `block-driver`'s, `nic-driver`'s and `audio-driver`'s offsets until 2026-10-19), with
  nothing checking that the pieces do not overlap or fit the granted arena. A layout of named, aligned,
  bounds-checked regions, fixed at start, fails loudly at spawn instead. **Built 2026-10-19** as
  `gs::driver::arena` (`Layout::take`, `Part`), with `gs::driver::registers`; AHCI, GENET, `nic-driver` and `audio-driver` converted first
  (`docs/driver-library.md` step 3).
- **Taking the grants.** Nine drivers each handle "no window or arena granted" in their own words. The
  shared part is taking both and saying plainly what is missing; what to do then stays the driver's.

**The principle that decides what goes in `gs` (settled 2026-10-11).** The SDK answers *what can this
kernel do, and how is it reached safely*: the syscall ABI and the audited `unsafe` (CLAUDE.md 18.1),
mirroring the kernel and changing when it changes (this branch retired four syscalls and their wrappers,
`backlog/fixed/80` K14). `gs` answers *how a Godspeed program is written*: the vocabulary a contributor learns,
shaped so the easy path keeps the model, and the part that must hold still. Two layers so the kernel can
move without moving contributors. So the test for `gs` is not "is it safe?" - the SDK's types already are -
but **"must a contributor name it and rely on it?"**

Applied to registers, that reverses what this paragraph first said ("register access stays on the SDK's
`Mmio`; wrapping it would be the re-export already declined"). A driver must hold its register window and
its DMA arena, so **the window and arena TYPES come through `gs::driver`, frozen**: the taking-the-grants
mechanism hands them back, with the calls drivers already make (`read32`, `write32`, `len`, and for DMA
what the arena layout leaves), built on the SDK's `Mmio` and `Dma` underneath. The ~850 register sites keep
their shape and change their import. This is not the wholesale re-export declined above: it is two types a
contributor cannot avoid naming, reached through the mechanism that hands them out, while the SDK beneath
stays free to change. It adds no safety - `Mmio` already bounds- and alignment-checks every access - and is
not meant to: it is about what a contributor depends on. **Typed registers** (each register declared once
with its width and access, named bit fields) would add readability and compile-time checks, but they are a
large API frozen the day it lands; they wait until after v1, to be judged by the found-in-real-drivers test
once the arena conversion has shown what one costs. No userspace cache maintenance exists to extract:
arenas are uncached on the ARM boards and x86 DMA is coherent.

**Step 2 is done when no service or driver crate depends on the SDK directly** - only `gs` and the domain
libraries (`sdk/wifi`, `sdk/audio`) do, and the SDK is an internal layer. That is the finished form of the
one-way ratchet, and a gate can check it from the `Cargo.toml` files. Measured on 2026-10-11: **all 25
service crates** list `godspeed-sdk` (2 of the 15 examples do), and what they name from it is mostly an
import path `gs` already re-exports (`service_context`, 108 uses) plus `hid` (20), `trace` (18), `churn`
(5), `record`, `mmio` and `ipc` (2 each) and `adversarial` (2, `probe` only). Each is either moved behind
`gs`, or - like `adversarial`, the test-only fault module 18.1 designates - named as an exception with its
reason where the gate can see it.

**The order agreed:** (1) close `fix/backlog-80`; (2) a branch for the two `gs::driver` mechanisms and the window and arena types they hand back, proved
on two unlike drivers (AHCI on x86, GENET on the Pi 4) and then the rest one at a time; (3) a v1 groundwork
branch for steps 1, 3 and 4 below - the covered surface confirmed, a checked-in snapshot of the public `gs`
surface (including `gs::driver`) and the utility vocabulary with a checker that fails on removal or change,
and the compatibility policy; (4) the soak, one or two releases in which that checker never has to accept a
breaking change; then v1.

## The path to a credible v1

1. **Decide the covered surface, explicitly.** Proposed IN: the `gs` core (files, IPC, records, `net`,
   errors); the `gsh` language, the utility names and their documented outputs; the on-disk formats (GSFS,
   `/wifi.keys`); the contract schema. Proposed OUT (internal, or separately versioned): the SDK, the syscall
   numbers, service-to-service protocols, and the `gs::driver` tier - hardware support keeps growing, and a
   promise there would be broken. **Amended 2026-10-11: `gs::driver` is IN, grow-only** (the decision
   above): hardware support grows by adding mechanisms, which the promise allows, and a contributor
   writing a driver needs the mechanisms it has to hold still.
2. **Reshape first** (the section above), so the surface frozen is the one meant to be kept.
3. **Make the promise a gate**, as this project enforces everything else: a checked-in snapshot of the public
   `gs` surface and the utility vocabulary, and a checker that fails the build when something is REMOVED or
   CHANGED rather than added. A breaking change then needs a recorded reason, the way an amendment does.
   **Begun 2026-10-05 for the "one way" half:** `scripts/one_way_check.py` ratchets each crate's raw-SDK
   calls that `gs` already covers (1138 across 28 crates at the start, a new crate held to zero), so the
   reshape in step 2 can only move forward. The snapshot of the public `gs` surface is still to build.
4. **Write the compatibility policy down**, with its exceptions: security and soundness fixes may break;
   anything removed is deprecated for one release first.
5. **Soak**: a stretch of real work - the VisionFive and Pi 2 radios are the obvious candidates - with no
   change to the covered surface, so the freeze is shown to hold rather than asserted.

Until then the `v0.x` numbering is the honest one: it says the surface still moves, and it does.
