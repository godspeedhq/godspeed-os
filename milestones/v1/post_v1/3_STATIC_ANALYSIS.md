# Post-v1 Item 3 — Static Analysis CI

**Status:** ✅ Complete  
**CI workflow:** `.github/workflows/build.yml` — `static-analysis` job (parallel to `build`)  
**Evidence:** `build/tests/post_v1/3_STATIC_ANALYSIS/`

---

## Overview

Three complementary tools run on every push / PR:

| Tool | Purpose | Failure mode |
|------|---------|--------------|
| `cargo audit` | Known CVEs in dependency tree | Hard fail — any advisory |
| `cargo miri test -p kernel --lib` | UB detection in pure-logic kernel tests | Hard fail — any Miri error |
| `cargo geiger --package kernel` | Unsafe surface map | Informational (enforcement is `unsafe_check.py`) |

Each tool covers a different class of risk. Together they close the gap between
"compiles clean" and "demonstrably safe":

- **cargo-audit** catches supply-chain vulnerabilities that no amount of in-tree
  analysis would find.
- **Miri** is an interpreter that detects undefined behaviour (use-after-free,
  misaligned reads, violated pointer provenance) in tests that run safely on the
  host — exactly what the kernel `lib` target provides.
- **cargo-geiger** produces a machine-readable unsafe surface map per crate.
  It complements `unsafe_check.py` (which counts lines) by surfacing dependency
  unsafe at a glance.

---

## Implementation

### `rust-toolchain.toml`

Added `miri` to the components list so the nightly toolchain always includes it:

```toml
components = ["rust-src", "rustfmt", "clippy", "miri"]
```

### `.github/workflows/build.yml` — new `static-analysis` job

Runs in parallel with the existing `build` job. The lib target compiles for the
host and does not require the service ELFs that `build` produces, so there is no
dependency between the two jobs.

Steps:

1. `rustup show` — activates the pinned nightly toolchain (same pattern as `build` job).
2. Cache `~/.cargo/bin` — avoids recompiling `cargo-audit` and `cargo-geiger` on
   every run. Cache key is OS + a fixed version token (`v1`); bump the token when
   pinning a new tool version.
3. `cargo install cargo-audit --locked` — `|| true` so a cache hit (binary already
   present) does not fail the step. `--locked` respects the tool's own `Cargo.lock`.
4. `cargo audit` — hard fail on any known advisory.
5. `cargo miri test -p kernel --lib` — runs all 32 kernel unit tests under Miri.
   Miri is available because it was added to `rust-toolchain.toml`.
6. `cargo install cargo-geiger --locked` — same install-or-skip pattern.
7. `cargo geiger --package kernel` — informational; output captured, non-zero exit
   is not fatal. The `unsafe_check.py` script (item 2) is the enforcement gate.

---

## Scope and limits

**What Miri covers:** the 32 unit tests in `kernel/src/lib.rs` — capability,
generation, rights, IPC message, and queue logic. These are the pure-logic modules
that can compile for the host. Miri catches provenance errors, integer overflow
(in debug), and misaligned access in this subset.

**What Miri does not cover:** any code that requires `#![no_std]` hardware
primitives — context switch, APIC, page tables, IDT. Those paths are unsafe by
necessity and audited by `unsafe_check.py` + the SAFETY comments; they cannot run
under Miri without a full hardware emulation layer.

**What cargo-audit covers:** the full workspace dependency tree. Even though the
kernel has minimal deps (bitflags, limine), osdev and the SDK pull in more. Any
advisory in any crate will fail the job.

**What cargo-geiger covers:** the kernel crate's unsafe usage per-file. Because
`unsafe_check.py` already enforces counts, geiger output here is a second opinion
and a per-PR diff signal — reviewers can see if a PR changed the unsafe surface
without reading every file.

---

## Relationship to other post-v1 items

| Item | Tool | What it enforces |
|------|------|-----------------|
| 1 — Coverage | `cargo llvm-cov` | Branch / line coverage ≥ threshold |
| 2 — Unsafe audit | `unsafe_check.py` | Per-file unsafe line count frozen |
| **3 — Static analysis** | **cargo-audit / Miri / geiger** | **CVE clean, UB clean, surface map** |
| 4 — Mutation testing | `cargo-mutants` (planned) | Test suite kills mutations |
| 5 — Property tests | QuickCheck / proptest (planned) | Universal invariants hold |
| 6 — Stress tests | QEMU long-run (planned) | No drift/leak under load |
