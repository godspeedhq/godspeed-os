# 60 - the riscv32 size study stopped compiling, and `docs/multi-arch.md` still says 0 errors

**Status:** OPEN - two defects in a stub nothing builds, plus the documentation claim that hid them.
The claim is CORRECTED on main; the two code defects are recorded here rather than fixed, because
neither can be verified until the first is.
**Found:** 2026-09-27, while deleting `wip/riscv32-experiment`. The branch was a weak-model run whose
`build.rs` edits reverted three unrelated fixes, so it was removed - but it had closed real gaps, and
deleting it would have deleted the only trace of them.

## 1. The stub does not compile: 37 errors

`docs/multi-arch.md` recorded RISC-V 32-bit as **"Compiles - 0 errors"**, and said so twice. It was
true when measured. It is not true now:

```
$ cargo build -p kernel --target riscv32imac-unknown-none-elf
error: could not compile `kernel` (bin "kernel") due to 37 previous errors
```

All 37 are missing `arch::imp` seam members, not word-size problems - `PciDevice`, `MAX_DEVICES`,
`find_by_class`, `program_msi`, `program_msix`, `msi_dest_lapic`, `copy_user_to_kernel`,
`publish_bsp_lapic_id`, `note_irq`, `core_irq_debug`, `serial_unlocked_emit_count`, `pci_cfg_read32`,
`MSI_POOL_BASE`, `MSI_POOL_LEN`. The neutral kernel grew callers; the stub was never updated, because
nothing builds it.

**Nothing was hiding this, and that is the part worth being clear about.**
`scripts/arch_seam_check.py` reports it on every run:

```
scaffold:  riscv32      12 member(s) behind - no board targeted; rv32 is a size study, not a port
```

12 distinct members, 37 error instances - the same fact counted two ways, not a contradiction. The
scaffolds are tracked and deliberately not gated, which is right: holding a size study to the seam
would make every neutral-kernel addition a three-stub chore for ports nobody intends to boot. What
failed was not the gate. It was a MEASUREMENT written into prose as though it were a property, where
nothing could keep it current - the same defect as the hand-counted arch-conditional sites in
`CLAUDE.md` §4.1, and it is corrected the same way: the doc now names the live instrument instead of
restating a number.

**What this does NOT refute** is the conclusion the number was evidence for. The 37 errors are all
missing seam members; not one is a word-size failure, and `portable_atomic`'s shim path is unaffected.
The neutral kernel being 32-bit-clean was proven twice over - by arm32 natively and by riscv32's shim -
and arm32 went on to run userspace on hardware. The claim stands; its riscv32 measurement is stale.

## 2. The boot stub zeroes BSS with a 64-bit store

`kernel/src/arch/riscv32/mod.rs`, in `_start`:

```
"sd   zero, 0(t0)",
"addi t0, t0, 8",
```

`sd` is store-doubleword: **RV64I only.** RV32I has no such instruction, and the stride is 8 where a
32-bit word is 4. The fix is `sw` and `addi t0, t0, 4`, which is what the deleted branch had.

**Not verified, deliberately, and this is why it is recorded rather than fixed.** Compilation fails at
the 37 errors above, which is before codegen, so the assembler has never been asked to reject this. The
ISA fact is not in doubt; the observation that this tree rejects it is unmade. Fixing an untestable line
in a boot path to satisfy an argument is how a stub acquires a second bug, so item 1 comes first.

While in there: the same doc comment says "Later: Sv39 MMU". Sv39 is a 64-bit paging mode; RV32 uses
**Sv32**. Same cause - the file was written by copying the riscv64 stub.

## What the deleted branch had, if anyone picks this up

`wip/riscv32-experiment` (`0611358a`, deleted 2026-09-27) carried the `sw`/4-byte fix, a
`kernel-riscv32.ld`, the `build.rs` link-arg block for the target, and stubs for most of the 12 missing
members. It is **not** a starting point: it sat 341 commits behind main and its `build.rs` changes
reverted three unrelated things - it deleted `copier` from the arm and aarch64 service lists, put the
riscv64 userspace directory back on the kernel's own profile (`riscv_build.py` always builds release,
so that reads a directory nothing writes), and removed the "listed but missing is a hard error" guard
whose own comment explains that the placeholder boots to `LoadFailed(TooSmall)` and reads like a
corrupt image. Everything of value in it is described above; write it fresh.

## Next step

Decide whether the size study is still worth keeping current. Two honest options:

1. **Bring it back to compiling** - add the 12 seam stubs, then the `sw` fix becomes verifiable and the
   doc can carry a real measurement again. Bounded work, entirely inside `arch/riscv32/`, needs no
   hardware: `qemu-system-riscv32` is available.
2. **Retire it and say so.** Its conclusion is already banked and arm32 carries the native-atomic half
   on real hardware, so the shim path is the only thing riscv32 uniquely proves. If nobody will keep it
   compiling, a scaffold that has not built in months is worth less than a sentence saying the study
   was done, what it showed, and that the stub is frozen.

Either is defensible. What is not is the state this was in: a doc asserting 0 errors, a stub at 37, and
the difference visible only to whoever ran a build nobody runs.
