# 71. A v1 "no breaking changes" promise is not credible yet - and the path to one: reshape, freeze, gate, soak

**Status: OPEN - a decision recorded 2026-10-02, from the operator's question "can I safely release a v1
and promise no breaking changes in the future?". The answer was not yet, and why; this item is the plan.
The bar and the order were settled 2026-10-11 (below). Built so far: the one-way ratchet and the
SDK-dependency gate (step 2), the covered surface decided (step 1, 2026-10-26) and the surface gate (step 3,
2026-10-28). Still to do: the policy (step 4) and the soak (step 5).**

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
contributions are named as outside what v1 makes easy. (Decided 2026-10-23, `docs/driver-library.md`, "A
contributor's driver": the in-tree drivers stay on `gs::driver`, held there by `one_way_check.py`; a contributor's
driver package may call the SDK too, outside the promise and unchecked, and is told so.)

**Decided the same day: `gs::driver` is covered, grow-only.** The mechanisms it has (`wait`, `delay`,
`irq`) freeze with the rest of the surface; new mechanisms may be added, existing ones may not change.
That moves the driver gap into step 2: before the freeze, `gs::driver` gains the mechanisms drivers still
reach the SDK for, found repeated in real drivers as `docs/driver-library.md` requires. Two were found in
the 2026-10-11 survey:

- **An arena layout.** 107 `phys_at` calls across 17 files carved DMA arenas by hand-kept offset constants
  (every compiled driver's, until 2026-10-19 and 2026-10-20), with
  nothing checking that the pieces do not overlap or fit the granted arena. A layout of named, aligned,
  bounds-checked regions, fixed at start, fails loudly at spawn instead. **Built 2026-10-19** as
  `gs::driver::arena` (`Layout::take`, `Part`), with `gs::driver::registers`; AHCI, GENET, `nic-driver` and `audio-driver` converted first
  (`docs/driver-library.md` step 3).
- **Taking the grants.** Nine drivers each handle "no window or arena granted" in their own words. The
  shared part is taking both and saying plainly what is missing; what to do then stays the driver's.
  **Built 2026-10-19:** every compiled driver takes them through `gs::driver::registers` and `arena`, and
  the one-way ratchet counts the SDK's four grant calls, so a new driver starts at zero
  (`docs/driver-library.md` step 3). Since 2026-10-20 every compiled driver's arena is laid out by name too:
  `pwm-audio`, `dwmac`, `ehci`, `dwc2` and `xhci` followed the first four.
  **Two defects in that work, found by Audit 18 (`audits/documentation-audit.md`), fixed 2026-10-21 (built for
  the three ports, neither path reachable in QEMU, so each owes its board's card):**
  `nic-driver`'s `dwmac_main`, when the layout refuses a granted arena, logs the reason and then a second,
  wrong line, "no dwmac register window or DMA arena granted", though neither was missing; and
  `pwm-audio`'s `bring_up` zeroes and reach-checks an arena that is too small before the layout refuses
  it, where the size check it replaced came first. Neither changes what reaches the device. Now `dwmac_main`
  says which was missing, and `bring_up` lays the arena out, then checks the reach, then zeroes the two parts. **Hardware:**
  the Wyse card passed on 2026-10-20 (`269c86f3`: the AHCI, xhci, audio and RTL8168 layout lines, the
  keyboard, the disk, audio ready); the T630, the Pi 2, the Pi 4 and the VisionFive were owed theirs. (Note
  2026-10-23: all five have since run this code, in the step 4 cards of 2026-10-22 and a 1000-round chaos
  run each on 2026-10-23, every driver up and its device working; the two refusal paths themselves are
  reachable on none of them.)
- **The rest of what a driver asked the SDK for (2026-10-22, step 5).** By the operator's decision on closing
  the branch, as thin wrappers rather than mechanisms found repeated: `gs::driver::input` (`push`, `ready`),
  `power::set` and `device` (`board_mac`, `nic_vendor_device`, `emmc_base_clock_hz`, `pci_cfg_read`), and
  `gs::program::mode`. Counted by the one-way ratchet, every crate at zero. What a driver still takes from
  the SDK is `godspeed_sdk::hid` (a device class, so a domain library) and the `ServiceContext` and
  `Message` imports every service shares (`docs/driver-library.md` step 5).

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

**Done as a gate, 2026-10-26.** `scripts/sdk_dependency_check.py` (on the build path) lets a crate under
`services/` or `examples/` list `godspeed-sdk` only if `scripts/SDK-DEPENDENCY.baseline.txt` names it with a
reason; a new crate is on `gs` alone, and an exception that stops needing the SDK must leave the list. **25 of
42 crates are on `gs` alone** now: twelve moved that day (`audio-driver`, `block-driver`, `hw-enumerator`,
`nic-driver`, `observe`, `power`, `pwm-audio`, `recorder`, `time`, `wifi-driver`, `wifi-usb`,
`driver-skeleton`), whose SDK use was only the `ServiceContext` and `Message` `gs` already re-exports. The
17 exceptions split two ways, and the file says which is which:
- **By role, permanent:** the supervisor's spawn ABI; the shell, `control`, `ask` and `godspeed-pm` talking
  to the supervisor and the kernel's tables; `events`, the trace protocol's server; `chaos`, `probe` and
  `mem-pressure` as harness (18.1); the USB hosts' `usbdev`, `usbfn` and `hid`; `console`'s framebuffer;
  `copier`'s churn checksums.
- **Gaps to close in `gs`:** `fs` and `net-stack` read a bounded call's raw result (`DeadlineOutcome`,
  `DeadlineOutcomeInto`); the example `holder` reads `ReqOutcome`, which an example should get from
  `gs::call`; `mem-pressure` needs the SDK's `AllocError`.

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
   **Decided 2026-10-26 (operator), written in [`docs/compatibility.md`](../docs/compatibility.md):** covered
   are `gs` and `gs::driver` (grow-only), the `gsh` language and the record fields utilities emit (not their
   human-readable text), the on-disk formats (GSFS0008, the WiFi key store, the `/system` and `/users/default`
   layout), the contract schema, and - not in the proposal above - the package format and the platform file.
   The SDK, syscall numbers, service-to-service protocols and the kernel stay out.
2. **Reshape first** (the section above), so the surface frozen is the one meant to be kept.
3. **Make the promise a gate**, as this project enforces everything else: a checked-in snapshot of the public
   `gs` surface and the utility vocabulary, and a checker that fails the build when something is REMOVED or
   CHANGED rather than added. A breaking change then needs a recorded reason, the way an amendment does.
   **Begun 2026-10-05 for the "one way" half:** `scripts/one_way_check.py` ratchets each crate's raw-SDK
   calls that `gs` already covers (1138 across 28 crates at the start, a new crate held to zero), so the
   reshape in step 2 can only move forward.
   **Built 2026-10-28:** `scripts/surface_check.py`, on the build path, holds `scripts/SURFACE.baseline.txt` -
   1085 lines: the public `gs` items (`gs::driver` included, and the SDK items `gs` re-exports, such as
   `Table`), the utility names and subcommands, the record columns each producer builds, and the contract
   schema's keys, types, values and constraints. Growth is recorded with `--bless`; a removal or a change
   fails until `--accept-break "<reason>"` records it, dated, in `scripts/SURFACE-BREAKS.txt`. It runs 20
   mutants of the real sources on every build, so a gate that stopped seeing is caught. What it cannot see -
   meaning, the gsh language, `ServiceContext`'s methods - is listed in its header and in
   `docs/compatibility.md`.
4. **Write the compatibility policy down**, with its exceptions: security and soundness fixes may break;
   anything removed is deprecated for one release first.
5. **Soak**: a stretch of real work - the VisionFive and Pi 2 radios are the obvious candidates - with no
   change to the covered surface, so the freeze is shown to hold rather than asserted.

Until then the `v0.x` numbering is the honest one: it says the surface still moves, and it does.
