# 20. What the 2026-09-12 documentation audit found in CODE and CONFIG

**Severity:** mixed - one item broke a gate the constitution requires (`osdev validate`), three were
enforcement blind spots, two were cosmetic-to-minor.
**Status: ALL SIX FIXED on `portability-hardening` (2026-09-12).** Item 6 wants a VisionFive boot to
confirm; the other five are verified locally, each by forcing the guard to fire rather than by
observing it pass.

| # | Item | Commit |
|---|------|--------|
| 1 | `osdev validate` failed on six contracts | `b790cd7a` - schema now expresses `hw_pci_class` / `hw_pci_bar` / `hw_pci_irq` / `pci_cfg`; 32/32 pass |
| 2 | `release.yml` verified 7 of 8 stamp characters | `161930ca` |
| 3 | `arch_boundary_check.py` blind to two arches | `8aa2e5ed` - list derived from `kernel/src/arch/`, not restated |
| 4 | `riscv_build.py` ran none of three guards | `442285a6` - two of the three were WRONG for this port; see below |
| 5 | Dead `if` + its `unsafe` MPIDR read on every ARM tick | `3d43b27a` - `arm/mod.rs` 53 -> 52 unsafe lines |
| 6 | riscv64 spawned `xhci` after `block-driver` | `db3b800b` - verified in QEMU; **board boot BLOCKED by [26](26-visionfive-uboot-cannot-load-large-files.md)** |

**What the work actually found, beyond the six.** Three of the fixes turned out to be shallower than
the defect under them, and the pattern is the same each time: the instrument did not merely miss a
port, it was built on an assumption that had already been superseded.

- **`service_embed_check` would have raised a FALSE FAILURE** if wired in as-is. It compares the
  supervisor's managed set against `<arch>_built` in `kernel/build.rs`, but `riscv64_built` is
  `["supervisor"]` - this is the first port to finish step C, where the supervisor owns every image.
  The only port that has completed the migration was the one the checker would have failed. It now
  reads the supervisor's own roster on such a port, and its failure message names that file rather
  than sending the reader to a kernel list that is already correct.
- **`stack_fit_check` censused ZERO frames out of 412** on riscv64: its prologue pattern is ARM's
  `sub sp, sp, #N` and RISC-V uses `addi sp, sp, -N`. It now knows both, and - the general fix -
  RAISES when it recognises nothing at all, rather than reporting a pass it did not earn.
- **`shared_surface_check` counted PROSE.** It applied its regex to raw file text, so a comment
  mentioning `target_arch` counted as an arch-conditional site. That hid the reduction item 6 makes,
  and worse, would let a real reduction go unrecorded whenever it is described in a comment.

**And item 6 was fixed twice.** The first attempt widened `#[cfg(target_arch = "aarch64")]` to
`any(aarch64, riscv64)`; `shared_surface_check` refused it, correctly. Above the kernel `target_arch`
stands in for "which BOARD am I on", so naming riscv64 would have fixed one board and left the same
trap for the next. The spawn site now derives the host from `block-driver`'s peer list, which already
encodes the real question - and the surface goes 49 -> 48 rather than up.

**Deliberately NOT done**, and why: the schema's two dead keys (`hw_pio`, `spawn`) and the dead `ahci`
enum member stay. Removing them NARROWS the schema, which 13.5 makes a major version bump with a
documented migration - a deliberate v2, not something to slip into a gate fix. They are marked dead
in place.

The audit that produced commits `2a95f841`..`94abdde8` swept 293 markdown files and ~44,700 comment
lines. Those five commits fixed what was safe to fix - prose. Six findings were **not** prose, so they
were left untouched rather than folded in silently. They are recorded here, with the evidence re-checked
on this branch, so the split is visible rather than implied.

Three of the six are the same shape, and it is the shape this phase exists to remove: **an instrument
that does not cover the newest port.** `arch_boundary_check.py` cannot see two arches; `riscv_build.py`
runs none of the guards the ARM paths run; the supervisor's spawn order was extended for aarch64 and
not for riscv64. In each case the code was written when there were fewer ports and nothing failed when
a port was added - which is exactly the failure mode `feedback: a rule enforced on one build path is
enforced on none` describes.

---

## 1. `osdev validate` fails on six contracts (**the one that breaks a stated gate**)

CLAUDE.md §13.4 says contracts are validated by JSON Schema and §17 lists `osdev validate`;
CONTRIBUTING and §13.4 both treat it as a pre-PR gate. It does not pass:

```
FAIL services/block-driver   FAIL services/console    FAIL services/dwc2
FAIL services/hw-enumerator  FAIL services/nic-driver FAIL services/xhci
```

`contracts/schema/service.schema.json` sets `additionalProperties: false` on `capabilities` but never
declares `hw_pci_class`, `hw_pci_bar`, `hw_pci_irq` or `pci_cfg`, and its `hw_device` enum is
`["ahci","nic","xhci","ehci"]` while the two values actually used are `"framebuffer"` and `"dwc2"` -
the other three moved to `hw_pci_class` when the kernel started resolving devices by CLASS (step D).

**Why nobody saw it:** `build.yml` is the only workflow that runs `validate`, and it is paused;
`release.yml` never calls it. So the gate the constitution names has not run in CI for months.

Two smaller schema defects in the same file: `spawn` is described as `"init-only"` (init was removed in
Phase 5) and `log_write` as "send log messages to the `events` service" (§11.4's amendment exists to
deny exactly that - it writes the kernel ring and serial directly).

**Fix:** add the four missing keys, widen `hw_device`, correct the two descriptions. Then decide whether
`validate` belongs in `release.yml`'s source-only check loop, since a gate nothing runs is not a gate.

## 2. `release.yml` verifies 7 characters of an 8-character stamp

`42b0454c` changed both stampers to `git rev-parse --short=8`. The workflow's verify step still computes
`git rev-parse --short HEAD` (3 sites, lines 54/64/65) and greps for it. It passes only because a
7-character value is a prefix of the 8-character stamp, so the gate checks 7 of 8 characters.

Not broken, and mine - it was left inconsistent by my own commit. `--short=8` in those three places.

## 3. `arch_boundary_check.py` is blind to two arches while printing an unqualified pass

```python
_ARCHES = r"x86_64|x86|aarch64|arm|riscv64|riscv32"
```

`kernel/src/arch/` holds seven directories. `loongarch64` and `s390x` are absent from the pattern, so
`arch::loongarch64::` or `core::arch::s390x::` in a neutral file would pass silently - and the script
then prints "no named-arch references outside kernel/src/arch/", which reads as a guarantee it cannot
make. No such leak exists today; the check simply could not see one.

**Fix:** derive `_ARCHES` from the directory listing of `kernel/src/arch/` rather than restating it, so
the next port cannot be omitted. Restating a list the filesystem already owns is what went wrong.

## 4. `riscv_build.py` runs none of the three embed/stack guards, and says it runs all of them

`arm_build.py` and `pi4_build.py` each call `service_embed_check`, `embed_order_check` and
`stack_fit_check`. `riscv_build.py` calls none - `embed_order_check.py` appears in it only inside a
comment, which is what made this look covered. Worse, that comment asserts parity:

> line 1: "gated by the same checks every other build path runs"
> line 21: "`embed_order_check.py` is the guard, and it only guards a path that runs it."

The second sentence is exactly right and the file is the counter-example to its own rule.

Aggravating: `service_embed_check.py` HAS a `riscv64` ARCH_EXEMPT block, but its `__main__` loops only
`("arm", "aarch64")` - so riscv64 is unreachable from either direction.

This is the live risk of the three: `embed_order_check` exists because the build once shipped a
supervisor older than the services it embeds, and several ARM "confirmations" tested code that was not
running.

## 5. Dead `if` with a comment describing hooks that no longer exist

`kernel/src/arch/arm/mod.rs:2113` guards an **empty body** with
`if mpidr & 3 == 0 && !irq::usb_owned_by_userspace() { }`, under a comment about advancing USB
enumeration one transaction per tick. There are no periodic hooks; the MPIDR read is dead work on every
tick. Comment-only fixes could not touch it because deleting the `if` is a code change.

## 6. riscv64 spawns `xhci` after the `block-driver` that depends on it

`services/CLAUDE.md` records the storage chain as a DEPENDENCY order and the supervisor spawns the USB
host first on arm32 and aarch64 for that reason. On riscv64 `block-driver` names `xhci` as a peer
(`supervisor/src/main.rs:326`) but `xhci` is spawned ~95 lines later (line 328 vs the aarch64 early-spawn
block), so `block-driver` comes up without a cap to its host.

Survivable by design - §14.3 has the client reacquire by name - and the 22,872-round soak passed, so the
cost is one round of failure and recovery at boot rather than a broken disk. Recorded because the
comment guarding the aarch64 case argues against exactly this, one port over.

---

## Not in scope here, but from the same audit

The enforcement layer's coverage boundaries, each of which let one of the findings above survive:

- `doc_refs.py` does not scan `tests/`, `osdev/` or `examples/` - which is why five dead directory
  references in `tests/qemu/CLAUDE.md` and a wrong kernel-hook instruction in `examples/e1000/` went
  unnoticed.
- `site_check.py` globs `website/src` only, so `website/README.md` - the file that states the rule about
  which pages are hand-written, and gets it wrong - is read by nothing.
- `unsafe_check.py` has two roots, `kernel/src` and `services`, so `sdk/`'s ~125 `unsafe` lines are
  unscanned. Tracked separately as [18](18-unsafe-audit-misses-the-sdk.md).
- No living audit (`audits/kernel-audit.md`, `security-audit.md`, `userspace-audit.md`,
  `documentation-audit.md`) has an entry covering `riscv64`. A fourth architecture shipped and released
  without passing through the family §18.4 and §26.3 rest on. That is an operator decision, not a fix.
