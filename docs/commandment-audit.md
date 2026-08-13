# Commandment Audit

**Living.** The fifth of the audit family (`kernel-audit.md`, `userspace-audit.md`,
`documentation-audit.md`, `security-audit.md`), and distinct from all four: they audit *code* against
the law. This one audits the **mechanisation of the law** - for each Commandment, what is encoded as a
build-failing check, what deliberately is not, and what the encoding found.

North star: **a green build must never claim more than it measured.**

## This document is not a second source of truth

`scripts/commandments.py` is the enforcement. `python scripts/commandments.py --report` prints the live
state: every check, its scanned scope, every exclusion with its reason, what it proves, what it
explicitly does not, and which Commandments have no check at all.

So this document deliberately does **not** list the checks or restate what they cover - that would be a
second copy of a fact the tool already owns, drifting the first time a check changes (Commandment III).
It holds only what the tool cannot: **why** a thing was mechanised the way it was, **what was rejected
and why**, and **findings awaiting a decision**. For "what is checked today", run `--report`.

## Method

One Commandment at a time. For each: read what it actually demands, measure the codebase against each
part of it *before* writing a check, encode what can be honestly encoded, record what cannot, then run
it and report findings without fixing them. Fixes are a separate, deliberate step - an audit that fixes
as it goes stops being able to tell you how bad things were.

Every check ships with probes in the self-test corpus. A guard never observed firing is not evidence.

---

## Commandment I - thou shalt not expand the responsibilities of the kernel

**Status: mechanised (6 checks). 3 findings, none fixed.**

### What is encoded, and why those things

The commandment names six kernel responsibilities and says "nothing else", then §4.4 lists the
anti-scope explicitly. Rather than try to police that prose, the checks pin the **surfaces through
which a new responsibility must arrive**: syscalls (new kernel verbs), well-known authorities (new
kernel nouns - several exist because a syscall alone could not express the authority), ring-0
dependencies (code the kernel did not write but trusts absolutely), top-level modules, per-file roles
under `arch/`, and the kernel's own spawn set.

The `arch/` role pin is the one that took thought. `arch/` is legitimate - a kernel must bring up its
own CPU, MMU, timer, interrupt controller and console - so pinning module *names* cannot see a USB
stack hiding inside a legitimate module. Every file under `arch/` therefore declares what it **is**,
from a fixed vocabulary. Smuggling a driver in now requires writing a lie into the baseline, in a diff,
rather than choosing an innocuous filename nobody re-reads.

### The one sanctioned exception

The kernel restarts exactly one service, the supervisor, and it must: Commandment V says no service is
special, so the supervisor must be restartable too, and only the kernel is beneath it to do the
restarting (§11.1, §6.2, `naming-design.md` §3.7 - a sliver of §26.10 traded for maximum fault
tolerance).

It is pinned as a **single string, not a list**. The shape carries the rule: a list has room for a
second entry and appending one reads like configuration, whereas a scalar cannot be appended to, so a
second kernel-spawned service means changing the schema. The checker refuses a list, and refuses the
old plural key, so the affordance cannot leak back in. An exception that *can* grow eventually does.

### Considered and REJECTED (recorded so they are not rebuilt)

**Anti-scope vocabulary scanning** (`inode`, `tcp_`, `work_steal`, ...). Measured before building:
almost every hit is a comment, and every `migrat` hit is code that *forbids* migration
(`assert_no_mid_execution_migration`). A check with that false-positive rate teaches people to ignore
it, which is worse than no check.

**A kernel line-count ceiling** for "the kernel remains tiny". It contradicts the commandment's own
text, which explicitly welcomes new hardware support and new CPU architectures - adding an arch would
blow a global ceiling while being permitted. The commandment counts **responsibilities**, not lines,
and a proxy that fires on sanctioned work only trains people to raise it.

### Findings

| ID | Finding | Severity | Status |
|----|---------|----------|--------|
| C1-1 | The kernel can spawn three services by name, not one | Medium | open |
| C1-2 | `arch/` is 65% of the kernel | Observation | open |
| C1-3 | The kernel links a font crate for a framebuffer console | Low | open |

**C1-1. The kernel carries the ability to spawn `logger` and `shell` directly, not only `supervisor`.**
`arm_spawn_logger_neutral()` and `arm_spawn_shell_neutral()` in `task/mod.rs` are ARM/AArch64 bring-up
scaffolding from before the supervisor path worked, called from `sched_spawn.rs` and `sched_shell.rs`
under the `arm-sched-spawn` and `arm-shell` features. Production builds use `arm-supervisor`, so
**neither runs in a shipping kernel** - this is not a live bug.

What raises it above dead code: both are gated on **architecture, not on the features that call them**
(`#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]`), so they compile into every ARM and
AArch64 kernel regardless of which scheduler feature is selected, including the Pi 4 mainline. The
kernel holds the ability whether or not any path currently reaches it.

No amendment covers either, so per the exemption rule neither can be recorded - they are removed, or
the constitution changes deliberately. Found only because the exception was written down and the count
checked, which is the argument for writing exceptions down.

**C1-2. `arch/` is 28,755 of ~44,000 kernel lines.** Legitimate by the commandment's text (hardware
support is welcome), and noted because it means the majority of the kernel lives in the layer these
checks scrutinise least - the role pin says what each file *claims* to be, not what it does. Two of
those files are the recorded arm32 USB exemptions.

**C1-3. `noto-sans-mono-bitmap` is linked into the kernel** for the framebuffer console (`fbcon/`,
1,172 lines). §4.4 forbids "logging infrastructure" and §11.4 sanctions only a 16 KiB ring buffer plus
the serial console. A framebuffer console for the kernel's own early output may well be justified, but
it is not currently justified **anywhere in writing**, which is the actual finding: either it earns a
sentence in the constitution or it is a service.

---

## Commandments II - X

Not yet audited. `--report` lists each one and why it is not mechanised, so the gap is visible on every
run rather than only here.

Working order, and the reasoning: **IV** next (fold in the existing `contract_check.py` - nearly free,
and it has already caught a real violation). Then **V's second half and IX together**, because they are
one machine - the runtime dependency matrix, kill each dependency and assert every caller still
answers. That is the highest-value item outstanding: it is the only thing that would have caught the
hangs, and no static check ever will. Then **VIII** (heuristic, noisy, allowlist-driven), then **II**
as a gate with a threshold rather than a lint, then **VII**. **III** and **X** are expected to stay
judgment, and to be printed as such.
