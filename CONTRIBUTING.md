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
| Write a new userspace service | [`GETTING_STARTED.md`](GETTING_STARTED.md) - the 5-minute quickstart (copy [`examples/00-hello/`](examples/00-hello/), read its `CLAUDE.md` + [the standard library guide](website/src/stdlib.md); [`sdk/rust/CLAUDE.md`](sdk/rust/CLAUDE.md) is the layer underneath) |
| Learn one pattern (IPC, caps, composition, persistence, drivers) | The matching `examples/*/CLAUDE.md` (index: [`examples/README.md`](examples/README.md)) |
| Add or change a syscall | `kernel/src/syscall/CLAUDE.md`, then `syscall/mod.rs` |
| Add a new CPU architecture | [`kernel/src/arch/CLAUDE.md`](kernel/src/arch/CLAUDE.md) (the seam + the five-place checklist) |
| Change capability or generation logic | `kernel/src/capability/CLAUDE.md` |
| Touch IPC or routing | `kernel/src/ipc/CLAUDE.md` (and bring a benchmark - the fast path may not change without one) |
| Add or change a test | `tests/CLAUDE.md`, then the category under `tests/qemu/` |
| Know the full law and the instant-reject list | [`CLAUDE.md`](CLAUDE.md) (the constitution; section 21 is the reject list) |
| Check my change against the anti-patterns before I open a PR | [`docs/anti-patterns.md`](docs/anti-patterns.md) - the field guide to constitutional violations, each paired with the correct pattern |
| Find something to work on, or check a bug is not already known | [`backlog/`](backlog/README.md) - open items, each with its evidence, what is RULED OUT, and the next step |

## Learn from a real change

Every kind of contribution below has already been made here, by the people who wrote the rules, and
passed the same gates yours will. So do not start from a description: start from the real thing. Each
row names code you can read and run in the tree today, and a merged commit that made exactly that kind
of change - its message says what was found, what was done and how it was verified, and its diff shows
every file that kind of change has to touch. Nothing here is a toy written for the purpose.

| I want to ... | Read and run this | Then read this commit | What to copy from it |
|---|---|---|---|
| **Write a service** | [`examples/counter`](examples/counter/) (restart with state), then [`services/power`](services/power/src/main.rs) - 247 lines, a whole real service on `gs` | [`7dfe3fbd`](https://github.com/godspeedhq/godspeed-os/commit/7dfe3fbd) - every service moved onto `gs` | Use `gs` and nothing under it: `scripts/one_way_check.py` holds every crate at zero raw SDK calls that `gs` covers |
| **Write a driver that streams** (a DMA ring kept fed) | [`services/pwm-audio`](services/pwm-audio/src/main.rs) - the Pis' jack, about 950 lines | [`13be2d55`](https://github.com/godspeedhq/godspeed-os/commit/13be2d55), then [`3ac042c2`](https://github.com/godspeedhq/godspeed-os/commit/3ac042c2) - the Pi 4's one underrun per tone | Measure before fixing: a one-line instrument first, the fix second, each with a prediction, each tested on the board |
| **Support new hardware in a driver** | [`services/audio-driver`](services/audio-driver/src/main.rs) - Intel HD Audio, its `PLAYABLE` table of verified codecs | [`3787e6dc`](https://github.com/godspeedhq/godspeed-os/commit/3787e6dc) (a new codec, and the mixer its path needs), [`533003cd`](https://github.com/godspeedhq/godspeed-os/commit/533003cd) (the vendor's bring-up, as Linux does it) | Copy what the reference driver writes, never a value built from bit names (`kernel/src/arch/CLAUDE.md`); read every register back and log it |
| **Write a driver from scratch** | [`examples/driver-skeleton`](examples/driver-skeleton/) and [`examples/e1000`](examples/e1000/) | - | The grant, the interrupt, the re-arm, the serve loop, restart from cold |
| **Change the kernel** | [`kernel/src/task/mod.rs`](kernel/src/task/mod.rs) `pci_dev`, [`kernel/src/arch/x86_64/pci.rs`](kernel/src/arch/x86_64/pci.rs) `bar_len` | [`7956c174`](https://github.com/godspeedhq/godspeed-os/commit/7956c174) (K1, a driver's window is its BAR), [`39746569`](https://github.com/godspeedhq/godspeed-os/commit/39746569) (K2, the device the supervisor names) | New `unsafe` only in `arch/`, recorded in `audits/unsafe-audit.md`; every other port answers the new seam member; QEMU first, then a board |
| **Add or change a shell utility** | [`utilities/57_audio.md`](utilities/57_audio.md) (the spec) and [`utilities/0_conventions.md`](utilities/0_conventions.md) | [`0c31c4d4`](https://github.com/godspeedhq/godspeed-os/commit/0c31c4d4) - Ctrl+Alt+Up, Down and M | Spec first, then the shell, its help, its test and the docs in one change |
| **Add a test scenario** | [`osdev/src/shell_test.rs`](osdev/src/shell_test.rs) `boot_audio`, run by `osdev test audio` | [`39746569`](https://github.com/godspeedhq/godspeed-os/commit/39746569) - a decoy controller placed FIRST on the bus, so the test fails if the wrong device is granted | Build the situation that would expose the bug, not the one that passes |
| **Fix documentation** | [`audits/documentation-audit.md`](audits/documentation-audit.md) | [`fc95b1d7`](https://github.com/godspeedhq/godspeed-os/commit/fc95b1d7) (an audit of one area), [`72188226`](https://github.com/godspeedhq/godspeed-os/commit/72188226) (a sweep of code comments) | Read each claim against the code and the hardware; a dated record stays as history and gets a dated note |
| **Improve a gate** (`scripts/`) | [`scripts/docs_index_check.py`](scripts/docs_index_check.py), [`scripts/commandments_redteam.py`](scripts/commandments_redteam.py) | [`ae1b6fc4`](https://github.com/godspeedhq/godspeed-os/commit/ae1b6fc4) (two new checkers, from an audit), [`576e7e85`](https://github.com/godspeedhq/godspeed-os/commit/576e7e85) (a rule found false, the gate and the docs corrected together) | A new check comes with a probe that breaks it on purpose; a gate may only get stronger (below) |

### Chaos finds

`chaos max-carnage all-services <rounds> yes` kills services at random, round after round, under memory
and spawn pressure, and the system must recover every time. Run it on your board and read the log, not
only the summary line - most of what it finds is in the counters. Four real bugs it found, each fixed
and each commit saying how it was caught:

| Commit | What chaos exposed |
|---|---|
| [`c51245c6`](https://github.com/godspeedhq/godspeed-os/commit/c51245c6) | A fresh `xhci` on the T630 stopped completing commands. Looking for why found that every IOMMU-confined device shared one domain ID, which the hardware caches translations by; the commit says plainly that this was found, not proven to be the whole cause |
| [`bdc7adaa`](https://github.com/godspeedhq/godspeed-os/commit/bdc7adaa) | On the T630 the idle path kept restarting a timer countdown, so a core could go without a tick until the liveness watchdog panicked, 6 s after a 1000-round run |
| [`6737ae52`](https://github.com/godspeedhq/godspeed-os/commit/6737ae52) | On the Pi 2 the kernel's page-table arena ran out with RAM free: after a 1000-round run every new program was refused |
| [`b2a54655`](https://github.com/godspeedhq/godspeed-os/commit/b2a54655) | The supervisor was refused a reply mailbox at every one of its 13 respawns in a chaos run: the kernel gave a dead task's mailbox back only to the services the supervisor restarts, never to the supervisor itself |

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

## Break it: an open invitation

GodspeedOS claims that **the kernel is the only thing that cannot be restarted** - every service,
the supervisor included, dies and comes back. That is a claim about RECOVERY, and the honest way to hold
it is to let anyone try to make it false. So: if you have a machine with cores and hours to spare, run
`chaos` against GodspeedOS for as long as you like - a million rounds, a billion - and if it breaks,
tell us. A break found by a stranger is worth more than a thousand rounds we ran ourselves, because it
is a kill order we did not think of.

This is about recovery, **not security**. `chaos` holds the authority to kill services, so whatever it
breaks - a kernel panic included - is a recovery bug and belongs here, in public. A panic that an
unprivileged service or a network peer can cause, a capability bypass, a way to gain authority you were
not granted, or anything else that is an attack rather than a failure to recover, is not reported here -
do not post it in a public issue; report it privately as [`SECURITY.md`](SECURITY.md) describes.

### What counts as a break

- **A kernel panic**, or a **liveness wedge** (a core goes dark and the watchdog fires) - the one thing
  that must never happen.
- **A service that never comes back** - dead at the end of the run, and still dead after the
  supervisor's sweep has had time to find it.
- **A recovery that comes back wrong** - `selfcheck` fails after the soak, a file reads back different
  from what was written, the network answers wrongly after it has answered rightly.
- **A resource that keeps going** - free memory falling run over run, restarts that start failing and
  keep failing. Recovery that leaks is recovery on a timer.

What does **not** count: a fault in the hardware itself, an image you modified, and recovery that is
slow but bounded - a service that takes several seconds to return after a storm is doing its job.

### How to run it

On real hardware, at the prompt:

```
gsh> chaos max-carnage all-services 1000000 yes
```

In QEMU, from a checkout, one boot running the storm repeatedly: `osdev test chaos-repro:<rounds>:<iterations>`
(the serial lands in `build/tests/chaos_repro_serial.log`).

**A powerful machine buys parallel runs, not a faster one.** A round is a sweep that kills, floods and
waits for recovery, and it takes about two seconds - a Raspberry Pi 4 ran 547 rounds in 21 minutes on
2026-10-09 - so one run of a billion rounds is not a weekend; it is decades. What a machine with
many cores CAN do is run dozens of instances at once, each soaking independently, and that is genuinely
valuable. Use a separate checkout for each QEMU instance: the test harness writes fixed paths under
`build/`, so two runs in one checkout overwrite each other. QEMU's timing is not a board's - a break
under QEMU still counts, and is worth saying it was QEMU.

### What to send

Every `all-services` run prints its **seed** when it starts and again in its report. A report we can act
on has:

- the **seed** and the **round** it broke at;
- the **commit**, from the first line of the boot banner (`GodspeedOS 0.22.0 x86_64 (f54aafef) - kernel`);
- the **machine** - the board, or the QEMU command line and `-smp`;
- the **serial log**, the whole of it. It matters more than anything else on this list: `events log
  boot` holds only the boot, and the supervisor's `hardware events` record restarts whenever chaos kills
  the supervisor.

**What a seed does and does not do.** `chaos max-carnage all-services <n> seed <s>` replays a run's
random DRAWS, not the run: one draw is made per live service each round, and which services are live
depends on restart timing across the cores, so two runs on one seed part ways at the first round whose
timing differs. Rerunning your seed and seeing no failure does not mean the bug is gone; send it anyway.
The seed narrows a break from "somewhere in a billion rounds" to one run's decision stream, which is
what the person fixing it needs.

A break you find is credited the way every contribution is: in git history. The commit that fixes it
carries a `Reported-by:` trailer with your name, so the record of who found it is permanent and sits
next to the fix.

## Interdependent services wait on truth, not time

When one service depends on another - `fs` on `block-driver`, the shell on `fs`, any client on any
server - the dependent **blocks on its dependency's reply, never on a fixed amount of time.** This is
Commandment VIII made concrete: wait on truth (the reply, or the loud fact of the peer's death), never
on a timer, a yield count, or a tick.

The standard pattern is the standard library's `gs::call::request_within(&ctx, peer, &msg, secs)`
(`stdlib/rust/src/call.rs`): it sends the request carrying a one-shot reply cap and blocks for the
reply, bounded by `secs`. It waits on truth **without ever hanging** - it is a synchronous kernel
CALL (syscall 41) in its bounded form, `CallDeadline` (syscall 50, CLAUDE.md section 8.2), so if the
peer dies after receiving the request but
before replying, the kernel wakes the caller with `ReplyDead` (the reply-side twin of `EndpointDead`,
CLAUDE.md section 8.6) instead of blocking it forever. What the caller gets back says which failure it
was, and they are not handled alike:

- **`Error::Unreachable`** - the request never left (a stale cap, a peer mid-restart). Nothing
  happened. `request_within` has already reacquired the peer **by name** through the kernel directory
  (section 14.3) and sent once more for you.
- **`Error::PeerDied`** - the request arrived and the peer died before answering. **It may have
  happened.** Reacquire (`gs::cap::reacquire`), and ask again only if the operation is safe to repeat.
- **`Error::OutcomeUnknown`** - no reply before the deadline. **It may have happened**, exactly as above.

That is the whole discipline: block on the reply, and on failure reacquire by name - re-sending only
what `retry_is_safe()` says, or what you know is safe to repeat.

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

**Adding a user-facing command?** `utilities/0_conventions.md` §2a lists the eight places a new verb has
to be registered and the rule that decides whether its spec belongs in `utilities/` or in `docs/` - both
are enforced by checkers, and both were written down only after a verb was implemented without them.

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
gives you one verdict instead of twenty-four. It needs Python 3.8 or newer on your `PATH` as `python`,
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
  `arch/`.** This is enforced by `scripts/arch_boundary_check.py` on every `osdev build` - a violation means a neutral
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
