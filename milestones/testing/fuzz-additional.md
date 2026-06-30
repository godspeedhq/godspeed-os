# Post-v1 Item 11 - Fuzz Tests (F1-F8)

**Status:** ✅ Complete  
**CI:** `osdev test fuzz` - runs as part of the `fuzz` workflow  
**Evidence:** 8/8 QEMU-level fuzz tests pass

---

## Overview

Fuzz tests verify that the kernel never panics on user-controllable input (§22,
Fuzz Tests table). The bar is absolute: any panic discovered by F1-F8 is a kernel
bug requiring a mandatory fix.

All eight fuzz surfaces were already fully implemented across three subsystems
(probe service, validator, and task/mod.rs) as part of prior milestone work. This
item confirms and documents them as a complete, named milestone.

---

## Test coverage

| ID | Surface | Probe mode | Pass condition |
|----|---------|-----------|---------------|
| F1 | Syscall args - random u64 in a0/a1/a2 (1M iters × 6 syscalls) | 30 | `fuzz: F1 pass` |
| F2 | Syscall numbers - random u64 as `nr`; must return UnknownSyscall | 31 | `fuzz: F2 pass` |
| F3 | ELF binaries - bit-flip mutations of a known-good ELF | kernel feature flag `test-bad-elf` | `fuzz: F3 pass` |
| F4 | Service contracts - malformed TOML; schema-invalid structures | validator contract-fuzz harness | `fuzz: F4 pass` |
| F5 | IPC message bodies - random bytes, random sizes up to 4 KiB | 32 + recv helper | `fuzz: F5 pass` |
| F6 | Embedded caps in messages - random cap structure in IPC | 33 + recv helper | `fuzz: F6 pass` |
| F7 | Cap generation field - random u64 as generation; must return CapRevoked/EndpointDead | 34 | `fuzz: F7 pass` |
| F8 | Memory request values - random sizes including 0, u64::MAX, >total RAM | 35 | `fuzz: F8 pass` |

---

## Implementation locations

| Fuzz test | Where it lives |
|-----------|---------------|
| F1, F2, F5, F6, F7, F8 | `services/probe/src/main.rs` - modes 30-35 |
| F3 | `kernel` - `test-bad-elf` feature gate in `src/loader.rs`; bad ELF injected at build time |
| F4 | `osdev/src/validator.rs` - `ContractFuzz` test kind; valid/invalid TOML generated in-process |

---

## Kernel invariant being verified

> **The kernel must never panic on user-controllable input.** (§22 Fuzz Tests)

All fuzz surfaces bottom out at the syscall boundary. Kernel code receiving
random input must return a defined error or silently discard - never panic,
never loop forever, never corrupt shared state.

The `F3 + F4` surface extends this to the spawn path: a corrupt ELF or invalid
contract must produce a loader error or schema-validation rejection, not a kernel
fault.

---

## Running

```
osdev test fuzz
```

Expected output: `8 passed  0 failed`

---

## Also verified at the brutal level

The brutal property tests (BP1-BP10, item 12 scope) run F1/F2/F5-F8 at 5× the
iteration count under concurrent load from the stress test supervisor. That
additional verification is tracked separately; this item covers the base 1×
coverage that must always pass.
