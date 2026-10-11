# 77. A misplaced `#[cfg]` in the supervisor gates `time`'s spawn, not `block-driver`'s

**Status: CLOSED 2026-10-11 - the attribute was REMOVED on 2026-10-10 (`68922715`, `backlog/80` V5) and
this entry was left saying OPEN. `time` now starts in every build; verified below.** Found 2026-10-06 by the
documentation audit.

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

## Closed (2026-10-11)

The fix was not the one proposed above. The attribute was removed rather than moved (`68922715`, under
`backlog/80` V5): every build has started `block-driver` all along and its suites run with it, so gating
it now would have taken it away from builds that use it. The comment where the attribute sat in
`services/supervisor/src/main.rs` says so, and names `board::BLOCK_CORE` as `block-driver`'s placement,
which also corrects the stale `preferred_core` paragraph.

Evidence that `time` starts in a build outside the old gate - the adversarial build, run 2026-10-11 on
`feat/ask` (`build/tests/6_ADVERSARIAL/A10-kernel_addr_as_syscall_arg_no_panic.log`):

```
task: 'time' spawned OK on core 1 (slot 20)
supervisor: name-map + time (endpoint cap slot 32)
supervisor: ready
```

`osdev test adv` 15/0 on that build. This entry stayed OPEN for a day only because the commit that fixed
it did not update it.
