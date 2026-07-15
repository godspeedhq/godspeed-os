# Contributing to GodspeedOS

Thank you for your interest in GodspeedOS. This is a deliberately small, fully-understood capability
microkernel; the goal is a system one engineer can hold in their head. That goal shapes how
contributions are judged, so a few minutes of reading up front will save you a rejected pull request.

## Read these first

GodspeedOS is governed by a written constitution, and contributions are held to it:

- **[`COMMANDMENTS.md`](COMMANDMENTS.md)** - the Ten Commandments of Godspeed, the human-readable
  distillation. Start here; internalize the ten and you will rarely break a rule by accident.
- **[`CLAUDE.md`](CLAUDE.md)** - the full constitution: the invariants, the capability model, IPC, the
  scheduler, the memory model, the unsafe policy, and the contribution rules (section 21). When the
  constitution and the code disagree, the constitution wins.
- **[`examples/`](examples/)** - the pattern primer. Every example carries its own `CLAUDE.md`
  explaining *why* it is built the way it is, grounded in a Commandment. Copy `examples/00-hello` to
  start a service; the others teach IPC, capabilities, composition, persistence, and drivers.
- **[`GLOSSARY.md`](GLOSSARY.md)** - the abbreviations (TLB, DMA, IOMMU, AHCI, and the rest).

## Building and testing

See the [README](README.md) "Getting started". It is the same `cargo run -p osdev -- ...` flow on
Linux, macOS, and Windows (there is no Makefile - the `osdev` CLI handles the platform differences),
plus the one-time Limine setup.

## The bar: tried by fire

A contribution is not considered proven until it has passed through the fire - the seven trials of the
test suite (section 22): Identity, Property, Fuzz, Stress, Performance, Adversarial, and Chaos, each
with a harsher "brutal" variant (see the "Tried by Fire" section of `COMMANDMENTS.md`). A green unit
test is necessary, never sufficient. In particular, every service must survive `chaos max-carnage`
(Commandment II): if Chaos finds a bug, the bug already existed.

## Interdependent services wait on truth, not time

When one service depends on another - `fs` on `block-driver`, the shell on `fs`, any client on any
server - the dependent **blocks on its dependency's reply, never on a fixed amount of time.** This is
Commandment VIII made concrete: wait on truth (the reply, or the loud fact of the peer's death), never
on a timer, a yield count, or a tick.

The standard pattern is the SDK's `request_with_reply` (`sdk/rust/src/service_context.rs`): it sends
the request carrying a one-shot reply cap and blocks for the reply. It now waits on truth **without
ever hanging** - it is a synchronous kernel CALL (syscall 41), so if the peer dies after receiving the
request but before replying, the kernel wakes the caller with `ReplyDead` (the reply-side twin of
`EndpointDead`, CLAUDE.md section 8.6) instead of blocking it forever. On either `EndpointDead` or
`ReplyDead` the caller gets `None`, reacquires the peer **by name** through the kernel directory
(section 14.3), and retries. That is the whole discipline: block on the reply, and on failure
reacquire-by-name and retry.

Do **not** paper over a dependency that might be slow or restarting with `yield` a fixed number of
times, a `sleep`, or a tick-count deadline "to give it time to come up". That is waiting on time, and
it is always wrong here: too short and you give up on a peer that was about to answer; too long and you
have hung the system on a peer that already died. The cautionary tale is `fs` <-> `block-driver`: `fs`
issues every block read/write as a synchronous request and blocks for the reply. Before the reply-side
death-wake, a `block-driver` that died mid-request left `fs` blocked on a reply that would never
arrive - a hang that a timer would only have converted into a guess. The fix was to wait on the
*truth* of the peer's liveness (the generation/liveness the kernel already tracks), so `fs` wakes the
instant `block-driver` dies, reacquires it by name, and retries. Follow that shape; if you find
yourself reaching for a sleep to coordinate two services, you are solving the wrong problem.

## What gets a pull request rejected (CLAUDE.md section 21)

A pull request is rejected without further review if it:

- Introduces ambient authority, or bypasses the capability / generation check.
- Introduces global mutable state outside a single owning service, or a silent fallback at the kernel
  boundary.
- Adds service migration, work stealing, zero-copy IPC, or live code update (all permanently rejected).
- Breaks the restartability of a non-TCB service, or adds a syscall that does not validate a capability.
- Adds `unsafe` without a `// SAFETY:` comment, or outside the permitted layers (section 18).
- Changes the IPC fast path without a benchmark, or edits `CLAUDE.md` without a rationale in the commit.
- Uses an em-dash or en-dash anywhere - only the plain hyphen is permitted (a house writing convention,
  enforced repo-wide).

See section 21 for the full list. Reviewers ask: does this respect the constitution, leave the kernel
small, present a convincing unsafe argument, include a test, and make the system more understandable?

## A note on scope

Features are pulled into existence by a real need - an invariant, an identity test, a demonstrated
operational problem - never added speculatively because another system has them (section 26.2). The
default answer to "should we add this?" is to simplify, reduce scope, and preserve the invariants. A
smaller coherent system is preferred over a larger impressive one.

## Adding an architecture

GodspeedOS is one arch-neutral codebase behind a single seam, `crate::arch::imp`. A new instruction
set architecture is **bounded to `kernel/src/arch/<isa>/`** - you write that directory and, apart from
two `#[cfg(target_arch)]` lines and the build plumbing, nothing else in the kernel changes. Five ISA
families (x86-64, AArch64, RISC-V, LoongArch, s390x) and both word sizes have been proven this way;
the proof is `docs/multi-arch.md`.

If you are porting, **read [`kernel/src/arch/CLAUDE.md`](kernel/src/arch/CLAUDE.md)** - it is the map:
the seam, the surface a port must expose, the exact five-place checklist, and the per-arch bring-up
gotchas found by actually booting. Two rules matter most, and both are load-bearing:

- **No inline `asm!` and no named-arch reference (`arch::x86_64::`, `core::arch::<isa>::`) outside
  `arch/`.** This is enforced by `scripts/arch_boundary_check.py` in CI - a violation means a neutral
  file made an arch-specific assumption, and the fix is to add an `arch::imp` primitive, never to
  special-case an arch at the call site.
- **Never use `core::sync::atomic::AtomicU64` directly - import `portable_atomic::AtomicU64`.** That
  one dependency is the entire cost of 32-bit support (32-bit RISC-V has no 64-bit atomic); reaching
  for the `core` type is the one easy way to silently break word-size portability.

`cargo check -p kernel --target <triple>` is the boundary test: any error *outside* `arch/<isa>/` is a
leak; errors *inside* it are just your stub naming the surface you still owe.

## Credit and attribution

Contributors are credited through **git history** - every commit carries its author, permanently and
accurately. Please do **not** add personal names to source files or program output. GodspeedOS shows a
single collective notice everywhere - `Copyright (C) 2026 Bankole Ogundero and the GodspeedOS
contributors` - and the phrase "and the GodspeedOS contributors" already includes you. Under the
project's licensing (the Linux model: no copyright assignment) you keep the copyright to what you
write; the collective notice is the project's shared face, not a transfer of your ownership.

**The year is the creation year, hardcoded on purpose - never read from the clock.** A copyright year
denotes when the work was authored (2026), a fixed fact; it is *not* the current year. The RTC is
available (the `date` command reads it), so wiring the notice to it would be easy - and wrong: a
machine booted in 2030 would then print "Copyright (C) 2030," which is false and changes with the
viewer's clock. Leave it a literal. When a later year sees substantial development, bump it to a
**range** - `Copyright (C) 2026-2027 ...` - as a deliberate edit (the end year is the last year of
real change, a build-time authorship fact, still not a clock read). Because the notice is one shared
string, change **every** copy together: `about` and `version` output (`services/shell/src/main.rs`),
`NOTICE`, `LICENSE` / `sdk/LICENSE`, and the canonical credit line in `utilities/0_conventions.md`
(rule 5/6); a shell test pins the exact string, so a partial bump fails loudly.

## License

By contributing, you agree your contributions are licensed under the project's terms: the OS is
**GPL-2.0-only** (root `LICENSE`); the SDK and the examples are **Apache-2.0** (`sdk/LICENSE`). New
source files should carry the matching `SPDX-License-Identifier` header for their directory.

Welcome aboard. Build something that survives the fire.
