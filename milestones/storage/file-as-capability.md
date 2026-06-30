# Milestone - A File Is a Capability (P2) ✅

**Status:** ✅ Complete - hardware-proven on the HP T630 (AMD GX-420GI), part of the `selfcheck`
**163/0** run (commit `93fd4bd` / merge `165afb9`). Pinned as §22 Test 14.

---

## Scope

The §7 "north star" made literally true: **a file is a real, kernel-minted capability**, not a
service-level token. This required the kernel to gain *delegated resource capabilities* (§7.10, P2)
- a resource whose *meaning* is owned by a service (`fs`) while the kernel still mints, validates,
routes, and revokes its caps with the **exact same machinery as an endpoint cap**. The kernel learns
nothing about files; it routes an opaque `ResourceId` and nothing more, so the §4.4 "no filesystem
logic in the kernel" anti-scope holds even as "a file is a capability" becomes true.

This is the mechanism that later generalizes to "a socket is a capability" (networking, §docs) - the
kernel gains a reusable delegation primitive, not a file feature.

---

## Achievements

- ✅ **Delegated resource capabilities in the kernel.** A service owns a band of opaque
  `ResourceId`s; the kernel registers each (generation 0, Alive) with the owning endpoint and mints a
  cap with chosen `Rights`. Three syscalls: `resource_mint` / `resource_invoke` / `resource_revoke`
  (`kernel/src/syscall/dispatch.rs:961 / 1004 / 1076`).
- ✅ **Minting is explicit authority, never ambient (§3.1).** `resource_mint` requires a
  `RESOURCE_MINT` capability, granted only to services that legitimately issue resources (`fs`).
- ✅ **Open → a real cap; use = send; revoke = generation bump.** `fs` mints a (narrowed) file cap on
  `Open` (recording `ResourceId → file`); a holder *uses* it by `send`ing - the kernel validates
  generation + required right and routes the message to `fs`'s endpoint **badged with the
  `ResourceId`**, so the owner knows which resource without the kernel knowing what it means; `fs`
  revokes on delete/close and every outstanding cap goes stale.
- ✅ **The three distinguishing cap properties hold end-to-end** (§7.3):
  - **Unforgeable** - a fabricated handle is rejected (`CapNotHeld` / `CapInvalid`), never accepted.
  - **Non-escalating - at *both* layers.** A READ-only file cap's WRITE is rejected by the **kernel**
    (`CapInsufficientRights` on the invocation) *and* by **`fs`** (a write op carried under a
    read-validated badge → `FS_DENIED`, enforcing `op ≤ right`). Defence in depth.
  - **Revocable** - after close/delete (and on rename), the next use returns `CapRevoked`.
- ✅ **The invocation badge is unforgeable.** The validated `(resource_id, right)` reaches `fs` as a
  kernel-set `Message` field (`LastRecvBadge` syscall), so a client cannot fake a file-cap invocation
  over its ordinary `fs` send cap.
- ✅ **Exercised end-to-end + hardware-proven.** Shell `fcap <file>` drives every property above;
  `osdev test file-cap` is **9/9** green in QEMU, and `fcap` is part of the T630 `selfcheck` **163/0**
  run on a real AHCI SSD - no panic.

---

## Files / evidence

| Area | Where |
|------|-------|
| Delegated-cap syscalls | `kernel/src/syscall/dispatch.rs` - `handle_resource_mint` (961), `handle_resource_invoke` (1004), `handle_resource_revoke` (1076) |
| Spec | CLAUDE.md §7.10 (delegated resource capabilities), §22 Test 14, §24 glossary ("delegated resource capability") |
| Owning service | `services/fs/` - maps `ResourceId → file`; mints on `Open`, revokes on delete/close/rename |
| Badge | `LastRecvBadge` syscall (`dispatch.rs:130`) - kernel-set, unforgeable |
| Test | `osdev test file-cap` (9/9); shell `fcap <file>`; doc `utilities/…fcap…` (commit `78bb05d`) |
| Hardware | T630 `selfcheck` 163/0 (commits `93fd4bd`, merge `165afb9`) |
| Correctness follow-up | global generation counter restored per-service monotonicity (P2/P7/P8), `ca60b1c` / merge `f09c3f5` |

---

> **Why it matters:** the kernel never learned what a file *is* - it routes opaque resources - yet a
> file gained every property of a capability: unforgeable, non-escalating, scoped, revocable,
> generationed. The filesystem chapter closes with the constitution's authority model intact.
