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

## Credit and attribution

Contributors are credited through **git history** - every commit carries its author, permanently and
accurately. Please do **not** add personal names to source files or program output. GodspeedOS shows a
single collective notice everywhere - `Copyright (C) 2026 Bankole Ogundero and the GodspeedOS
contributors` - and the phrase "and the GodspeedOS contributors" already includes you. Under the
project's licensing (the Linux model: no copyright assignment) you keep the copyright to what you
write; the collective notice is the project's shared face, not a transfer of your ownership.

## License

By contributing, you agree your contributions are licensed under the project's terms: the OS is
**GPL-2.0-only** (root `LICENSE`); the SDK and the examples are **Apache-2.0** (`sdk/LICENSE`). New
source files should carry the matching `SPDX-License-Identifier` header for their directory.

Welcome aboard. Build something that survives the fire.
