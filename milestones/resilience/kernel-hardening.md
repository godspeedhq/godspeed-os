# Milestone - Kernel Hardening ✅

**Status:** ✅ Complete - layered, boot-validated on the HP T630 where noted
**Theme:** Close every remaining *ambient-authority* gap and pin the memory-safety floor, so a capability is genuinely necessary and sufficient authority (§3.1) and `unsafe` stays bounded and audited (§18).

---

## Scope

v1/v2 established the capability model and the ring-3 boundary. This milestone is the security-hardening pass over the kernel that followed: it removes the implicit exceptions the model still carried (ambient introspection, ambient kill, ambient reboot, the un-enforced NX bit, an unbounded generation counter) and stands up the CI that keeps the `unsafe` surface honest. Each layer is small, explicit, and - where it touches hardware behaviour - verified on the T630.

---

## Achievements

- ✅ **W^X foundation (H4) - write-XOR-execute, enforced and asserted.** `EFER.NXE` (bit 11) is set **and asserted** on every core (`init_syscall: … NXE=1`), without which the page-table NX bits are silently ignored. A boot-time `audit_wx` (`arch/x86_64/boot.rs:241`) then asserts every audited region is W^X: `kernel-text W=0/NX=0`, `kernel-data` and `hhdm-ram W=1/NX=1`. A violation panics loudly at boot rather than shipping a silent hole (§3.12). Boot-validated on the T630 (`NXE=1` on all 4 cores, all three `wx-audit … ok`).
- ✅ **Generation-overflow guarantee (H7) - a stale cap can never be aliased.** Generation bump is `self.0.checked_add(1).expect("generation overflow")` (`capability/generation.rs:37`): the counter can never silently wrap and re-alias a destroyed resource's old caps. Pinned by a `#[should_panic(expected = "generation overflow")]` test (line 116).
- ✅ **Introspection is a capability, not ambient.** `InspectKernel` (system queries) and `TaskStat` are gated behind an `INTROSPECT` capability (`INTROSPECT_RESOURCE = 5`), closing the ambient-introspection exception (§3.1; `docs/introspection-capability.md`). The genuinely task-neutral hardware reads (own allocation, TSC, RTC/boot-datetime, fbcon geometry, input-ready, own console-foreground) stay ungated - they disclose nothing about another task.
- ✅ **Kill is a capability - `SERVICE_CONTROL`.** The `kill` syscall is gated by a `SERVICE_CONTROL` capability (`SERVICE_CONTROL_RESOURCE = 6`), minted only for `shell`, `supervisor`, and the test probes - closing the ambient-kill gap (`docs/service-control-cap.md`). Validated by holdings (both args carry the name, no slot to pass).
- ✅ **Reboot is a capability - `REBOOT`.** Machine reboot is gated by a `REBOOT` capability (`REBOOT_RESOURCE = 8`), held by `shell` (its `reboot` command) and `xhci`/`ehci` (Ctrl+Alt+Del), closing the ambient-reboot gap.
- ✅ **H9 syscall-surface audit - CLEAN.** A full audit of every handler in `syscall/dispatch.rs` confirmed: each privileged syscall validates its capability (by slot or by holdings) before acting; every user pointer is validated for the *exact* length accessed (`read_user_bytes` / `write_user_bytes` / `validate_user_ptr`); and malformed input degrades safely (a garbage cap slot → `CapNotHeld` via a safe `[T]::get`, an unknown syscall number → `-1`, never a panic or out-of-bounds). No security gap; two stale doc comments fixed (commit `7601b34`).
- ✅ **Unsafe-audit CI - every `unsafe` line accounted for.** `scripts/unsafe_check.py` enforces §18.4: it counts every non-comment `unsafe` line in `kernel/src/` and fails CI unless it matches the inventory in `docs/unsafe-audit.md`. `unsafe` stays confined to the four permitted layers (`arch/`, `memory/`, `capability/`, `smp/`) plus frozen grandfathered floors in `task/`/`syscall/`/`interrupt/` that may only shrink. Audit currently passes (447 accounted lines / 28 files).

---

## Commits / evidence

| Layer | Where | Evidence |
|-------|-------|----------|
| W^X foundation (H4) | `arch/x86_64/boot.rs` (`audit_wx`, EFER.NXE) | commit `7099cbe`; T630 boot: `NXE=1` ×4 cores, `wx-audit … ok` ×3 |
| Generation overflow (H7) | `capability/generation.rs:37` | `checked_add().expect`; `#[should_panic]` test (line 116) |
| Introspection capability | `capability/mod.rs:54` (`INTROSPECT_RESOURCE`) | `docs/introspection-capability.md` |
| Service-control capability | `capability/mod.rs:63` (`SERVICE_CONTROL_RESOURCE`) | `docs/service-control-cap.md` |
| Reboot capability | `capability/mod.rs:78` (`REBOOT_RESOURCE`) | gated `kill`/`reboot` syscalls |
| H9 syscall-surface audit | `syscall/dispatch.rs`, `syscall/CLAUDE.md` | verdict CLEAN; doc fixes commit `7601b34` |
| Unsafe-audit CI | `scripts/unsafe_check.py`, `docs/unsafe-audit.md` | §18.4; passes (447 lines / 28 files) |

> The DMA-after-free safety stack (page-table reclaim guard, BME-quiesce cure, DMA permanent-reserve) and the IOMMU DMA-confinement (H1) are their own milestones - see the sibling milestone `milestones/hardware/iommu-and-dma.md`.
