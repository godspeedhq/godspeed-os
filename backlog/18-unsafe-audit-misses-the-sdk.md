# 18. The unsafe audit does not cover the SDK, and §18.4 says it covers everything

**Severity:** correctness of an enforcement check, not of running code. Nothing is known to be unsound;
what is wrong is that the mechanism which would tell us does not look here.
**Status:** open, found 2026-09-11 while adding one `unsafe` block to `sdk/rust/src/service_context.rs`
and noticing that `unsafe_check.py` passed without being told about it.

## What §18 promises

> §18.4 - `audits/unsafe-audit.md` lists **every** unsafe block. CI checks it matches source.

> §18.1 - permitted: `kernel/src/arch/`, `memory/`, `capability/`, `smp/`, **plus the SDK's audited
> hardware/ABI layer**: the syscall ABI (`raw_syscall`, inline `asm!`) and the MMIO/DMA accessor
> modules (`sdk/rust/src/mmio.rs`, `sdk/rust/src/dma.rs`), plus `sdk/rust/src/adversarial.rs`.

> §18.2 - forbidden: all of `sdk/` **except** the audited layer named in §18.1.

## What is actually checked

`scripts/unsafe_check.py` defines exactly two roots:

```python
KERNEL_SRC  = REPO_ROOT / "kernel" / "src"
SERVICES    = REPO_ROOT / "services"
```

`sdk/` is not one of them. So no SDK file is scanned, no SDK file appears in `audits/unsafe-audit.md`,
and an `unsafe` block added anywhere under `sdk/` passes CI silently.

## The count

```
  sdk/rust/src/service_context.rs    83 blocks    NOT in §18.1's permitted list
  sdk/rust/src/ipc.rs                 8 blocks    NOT in §18.1's permitted list
  sdk/rust/src/mmio.rs                9 blocks    permitted
  sdk/rust/src/dma.rs                10 blocks    permitted
  sdk/rust/src/adversarial.rs         8 blocks    permitted
  sdk/rust/src/syscall.rs             4 blocks    permitted (the raw syscall ABI)
```

**122 blocks are unaudited, and 91 of them are in files §18.2 forbids outright.**

## Why it is not simply a violation to delete

The 91 are not obviously wrong. `service_context.rs` and `ipc.rs` are where the SDK touches the
kernel-written `ServiceContextData` page and performs syscalls, which is the same *kind* of work §18.1
already blesses in `syscall.rs` - the ABI boundary. The likely truth is that §18.1's list was written
when that work lived in fewer files and was never updated as the SDK grew, so the text and the code
drifted apart without anyone choosing it.

That is a decision to make deliberately, and it is the reason this is a backlog item rather than a
quick edit. Two honest options:

1. **Amend §18.1** to name `service_context.rs` and `ipc.rs` as part of the audited ABI layer, then
   extend `unsafe_check.py` to scan `sdk/` and add all 122 blocks to the audit with their SAFETY
   arguments reviewed. The ratchet then holds for the SDK as it does for the kernel.
2. **Move the ABI-touching `unsafe` behind fewer, narrower accessors** so the permitted list stays
   short and the rest of the SDK becomes genuinely `unsafe`-free.

Option 1 is the smaller change and matches what the code already does; option 2 is the one that keeps
§18.1 meaningfully narrow. Either needs the 122 SAFETY arguments actually read, which is the work.

## Why this matters more than the number suggests

This is the same failure this project has hit twice before and written down both times: a rule enforced
on one path is enforced on none. `arm_build.py` ran no checkers for the whole arm32 effort; the x86
build path ran only `commandments.py` while the ARM paths ran six, which is how an
`arch::x86_64::` reference from a neutral layer survived for days. The SDK is the most widely shared
code in the repository - every service links it - so it is the worst place to have an unenforced rule.

Nothing here says the SDK is unsound. It says we do not currently have the evidence that it is not,
while §18.4 claims we do. Recorded per §26.7 rather than left to be discovered by whoever trusts the
claim.
