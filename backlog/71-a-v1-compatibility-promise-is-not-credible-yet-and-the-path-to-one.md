# 71. A v1 "no breaking changes" promise is not credible yet - and the path to one: reshape, freeze, gate, soak

**Status: OPEN - a decision recorded 2026-10-02, from the operator's question "can I safely release a v1
and promise no breaking changes in the future?". The answer was not yet, and why; this item is the plan.
Nothing is built for it.**

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

## The path to a credible v1

1. **Decide the covered surface, explicitly.** Proposed IN: the `gs` core (files, IPC, records, `net`,
   errors); the `gsh` language, the utility names and their documented outputs; the on-disk formats (GSFS,
   `/wifi.keys`); the contract schema. Proposed OUT (internal, or separately versioned): the SDK, the syscall
   numbers, service-to-service protocols, and the `gs::driver` tier - hardware support keeps growing, and a
   promise there would be broken.
2. **Reshape first** (the section above), so the surface frozen is the one meant to be kept.
3. **Make the promise a gate**, as this project enforces everything else: a checked-in snapshot of the public
   `gs` surface and the utility vocabulary, and a checker that fails the build when something is REMOVED or
   CHANGED rather than added. A breaking change then needs a recorded reason, the way an amendment does.
4. **Write the compatibility policy down**, with its exceptions: security and soundness fixes may break;
   anything removed is deprecated for one release first.
5. **Soak**: a stretch of real work - the VisionFive and Pi 2 radios are the obvious candidates - with no
   change to the covered surface, so the freeze is shown to hold rather than asserted.

Until then the `v0.x` numbering is the honest one: it says the surface still moves, and it does.
