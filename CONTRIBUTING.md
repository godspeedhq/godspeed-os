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

## Where do I start?

Most contributions fall into a handful of shapes. Find yours and begin at the file named - each is a
short, local doc that points onward. Every directory carries its own `CLAUDE.md` explaining what lives
there and why, so when in doubt, open the one nearest the code you are editing.

| I want to ... | Start here |
|---------------|-----------|
| Understand the philosophy before touching anything | [`COMMANDMENTS.md`](COMMANDMENTS.md), then skim [`CLAUDE.md`](CLAUDE.md) |
| Write a new userspace service | [`GETTING_STARTED.md`](GETTING_STARTED.md) - the 5-minute quickstart (copy [`examples/00-hello/`](examples/00-hello/), read its `CLAUDE.md` + [`sdk/rust/CLAUDE.md`](sdk/rust/CLAUDE.md)) |
| Learn one pattern (IPC, caps, composition, persistence, drivers) | The matching `examples/*/CLAUDE.md` (index: [`examples/README.md`](examples/README.md)) |
| Add or change a syscall | `kernel/src/syscall/CLAUDE.md`, then `syscall/mod.rs` |
| Add a new CPU architecture | [`kernel/src/arch/CLAUDE.md`](kernel/src/arch/CLAUDE.md) (the seam + the five-place checklist) |
| Change capability or generation logic | `kernel/src/capability/CLAUDE.md` |
| Touch IPC or routing | `kernel/src/ipc/CLAUDE.md` (and bring a benchmark - the fast path may not change without one) |
| Add or change a test | `tests/CLAUDE.md`, then the category under `tests/qemu/` |
| Know the full law and the instant-reject list | [`CLAUDE.md`](CLAUDE.md) (the constitution; section 21 is the reject list) |
| Check my change against the anti-patterns before I open a PR | [`docs/anti-patterns.md`](docs/anti-patterns.md) - the field guide to constitutional violations, each paired with the correct pattern |
| Find something to work on, or check a bug is not already known | [`backlog/`](backlog/README.md) - open items, each with its evidence, what is RULED OUT, and the next step |

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
- Weakens a gate instead of the rule it enforces - un-wires a checker, widens an exemption, baselines a
  finding rather than fixing it, or makes a diagnostic vaguer. See the next section.

See section 21 for the full list. Reviewers ask: does this respect the constitution, leave the kernel
small, present a convincing unsafe argument, include a test, and make the system more understandable?

## Contribute anywhere; a gate may only get stronger

**Every part of this project is open to you, `scripts/` included.** The enforcement layer is written in
Python rather than Rust, and that makes it neither second-class nor off limits: it is the code that
decides whether the next change is allowed, so improving it is among the most useful things you can do
here. Close a blind spot, make a diagnostic clearer, mechanise a rule the constitution states and
nothing checks, retire a baseline entry that is no longer needed. `osdev conform --list` names every
rule and its code, and `py scripts/conform.py --explain GS0403` gives the long form for one.

**What you may not do is make a gate weaker.** That is the whole of it, and it is an asymmetry rather
than a prohibition: a rule may be improved, argued with, or repealed. It may not be quietly taught to
stop noticing. Each of these is a weakening, however reasonable it looks in a diff:

- **Deleting a checker, or removing one from `EXTRA_CHECKS`** in `osdev/src/main.rs`. A checker on one
  build path is a checker on none - eight documentation checks ran only at release once, which meant
  they ran too late to help anybody.
- **Widening a path exemption** so a rule stops looking at the code it was written for.
- **Adding a baseline entry instead of fixing what it found.** Baselines ratchet one way: a count may
  fall freely and may not rise without a recorded reason. Putting a finding in a baseline to get a
  green run is the one thing a ratchet must never be used for.
- **Using a `conform-ok` marker to silence a rule** rather than to record a genuine exception. A marker
  must name its rule and carry a real reason, and every honoured suppression is counted and printed -
  they are built to be visible, so do not treat one as a way to go quiet.
- **Making a diagnostic vaguer.** `CLAUDE.md` 22.7 puts it in terms: **a gate that fires with an
  unhelpful message is a finding, not a pass.** That is why `tests/conformance/ui/` holds the exact
  text of every diagnostic and `py scripts/conform.py --selftest` compares the render against it. If
  you change a message, run it and read the diff.

**One question settles a hard case:** after your change, does the gate still refuse what it was written
to refuse? A green run is not the answer to that - a check that has stopped working is also green, and
this project has caught its own instruments agreeing with it more than once.
`scripts/commandments_redteam.py` exists to answer it properly: it breaks each rule on purpose, so a
dead check is caught rather than mistaken for a clean tree. Run it deliberately and on a **committed**
tree, because it plants violations in real files and restores them with `git checkout`.

**And if a rule is genuinely wrong, the door is open - it is a different door.** The rules are written
down in [`CLAUDE.md`](CLAUDE.md) and [`COMMANDMENTS.md`](COMMANDMENTS.md) precisely so they can be
changed on the record: amend the text with a written rationale, and the checker follows it. That is how
most rules here reached their present form, and several have been narrowed or corrected outright when
the machine disagreed with the document. What the project will not take is the other version - the law
still saying one thing, the gate no longer noticing, and nobody finding out until someone trusts the
document.

Before you open a pull request, run **`osdev conform --check`**. It runs every gate a build runs and
gives you one verdict instead of eighteen. It needs Python 3.8 or newer on your `PATH` as `python`,
which is a declared dependency of this project alongside Rust and QEMU.

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
