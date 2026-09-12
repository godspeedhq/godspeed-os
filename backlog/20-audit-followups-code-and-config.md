# 20. What the 2026-09-12 documentation audit found in CODE and CONFIG

**Severity:** mixed - one item breaks a gate the constitution requires (`osdev validate`), three are
enforcement blind spots, two are cosmetic-to-minor.
**Status:** open. Scoped on `portability-hardening`, split out of the documentation
audit because that pass was deliberately restricted to documentation and comments.

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
