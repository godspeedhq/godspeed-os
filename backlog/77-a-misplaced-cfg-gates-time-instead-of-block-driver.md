# 77. A misplaced `#[cfg]` in the supervisor gates `time`'s spawn, not `block-driver`'s

**Status: OPEN - found 2026-10-06 by the documentation audit, read in the code, not yet changed. A change
to which services a build starts, so left for the operator. Predates this branch: the same attribute is on
`main`.**

## What the code says

In `services/supervisor/src/main.rs`, above the storage chain:

```rust
#[cfg(any(feature = "bare-metal", feature = "blockdev", feature = "identity-only"))]
// block-driver: core 0 on ARM, unpinned elsewhere. ...
// ... (eleven lines of comments) ...
ensure_mapped(&ctx, &mut name_map, "time", 0xFFFF);
```

An attribute attaches to the next STATEMENT, and comments are not one. So the gate meant for
`block-driver` gates `time`, and `block-driver`'s own `ensure_mapped` further down has no gate at all.

## What it costs

- **Every board image: nothing.** Board images are `bare-metal`, which the gate includes, so `time` is
  started there exactly as intended.
- **The full QEMU build and the test builds without `blockdev` or `identity-only`** (perf, stress,
  adversarial, chaos, fuzz): `time` is not started at boot, while `block-driver` is. `time` is in
  `MANAGED`, but the reconcile and convergence skip a name absent from the name map, so nothing starts
  it later either. A shell or `net-stack` there asking for the clock gets no answer.
- The comment under the attribute also describes a placement that no longer exists: it says the kernel's
  `ServiceConfig.preferred_core` decides `block-driver`'s core, and the IMAGES row's `board::BLOCK_CORE`
  decides it now.

## The fix, when the operator wants it

Move the attribute and the `block-driver` paragraph to sit directly above `block-driver`'s
`ensure_mapped`, and rewrite the paragraph: its core is its IMAGES row's `board::BLOCK_CORE` (2 where
`dwc2` is, 1 elsewhere), read by both the boot and the restart paths. That changes two builds' spawn sets:
`time` starts in every build, and `block-driver` stops being started in builds outside the gate - which
is what the gate always said. Each test build that changes should be run once after it.
