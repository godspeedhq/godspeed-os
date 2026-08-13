# Commandment Audit - TEMPORARY WORKING DOCUMENT

> **This file is scaffolding and is meant to be DELETED.** It is not a living audit, unlike
> `kernel-audit.md`, `userspace-audit.md`, `documentation-audit.md` and `security-audit.md`, which are
> permanent. It exists only while we walk the Ten Commandments one at a time, and its whole purpose is
> to hold the working state that a build cannot: the process being settled, and the findings not yet
> fixed.
>
> **Delete it when both are true:**
> 1. Every Commandment has been through the process below - encoded, or recorded as un-encodable with
>    the reason.
> 2. Every finding here is closed, either by a fix or by an amendment that accepts it.
>
> Nothing is lost when it goes. The **checklist becomes the build** (`scripts/commandments.py`, run by
> `osdev build` and CI), the **process** lives in that script's module docstring, and any finding that
> was accepted rather than fixed lives in `CLAUDE.md` as an amendment with a baseline entry citing it.
> If something in here has no home in one of those three, it is not finished.

## Why this is not a living audit

The other four audits are permanent because their subject is permanent: code drifts, so it must be
re-read. This document's subject is a **transition** - the one-time move from "the Commandments are
prose a human must remember" to "the Commandments fail the build". A transition document that outlives
its transition becomes a second, staler copy of the checks (Commandment III), so it is written to be
thrown away.

## This document is not a source of truth, even while it lives

`python scripts/commandments.py --report` prints the live state: every check, its scanned scope, every
exclusion with its reason, what it proves, what it explicitly does not, and which Commandments have no
check at all.

So this file deliberately does **not** list the checks. That would be a second copy of a fact the tool
owns, drifting the first time a check changes. It holds only what the tool cannot: **why** something
was mechanised the way it was, **what was rejected and why**, and **findings awaiting a decision**.

---

## The process (this is the part being locked in)

One Commandment at a time, in this order:

1. **Read what it actually demands.** The text and the `Grounded in` sections it cites, not the summary
   in your head. Commandment I's rationale is N-squared audit interactions, which is what rules out a
   line-count check; you cannot know that from the title.
2. **Measure the codebase against each part of it BEFORE writing a check.** Every check I wrote after
   measuring was sound; the ones I would have written from intuition (vocabulary scanning, size
   ceilings) were rejected by their own measurements.
3. **Encode what can be encoded honestly.** Prefer pinning the SURFACES a violation must arrive through
   (syscalls, authorities, dependencies, roles) over pattern-matching the violation itself. A pin is
   deterministic; a pattern is a guess with a false-positive rate.
4. **Record what cannot be encoded, and why.** Both the un-encodable Commandments and the specific
   approaches rejected. An unrecorded rejection gets rebuilt by the next person.
5. **Write the probes.** Known-bad that must be caught, known-good that must not. A check without
   probes is an assertion, and a guard never observed firing is not evidence.
6. **Run it and report. Do not fix.** An audit that fixes as it goes cannot tell you how bad things
   were, and the fix competes for attention with the finding.
7. **State what the check does NOT prove**, in the check itself, so it is printed beside every pass.

Two rules that came out of doing it, and cost something each time:

- **The shape of the pin is part of the policy.** `kernel_spawned_service = "supervisor"` as a scalar,
  not a list: a list has room for a second entry and appending one reads like configuration, while a
  scalar cannot be appended to. Where a rule permits exactly one of something, express one.
- **A badly written probe does not fail loudly - it agrees with you.** Two of three ratchet probes were
  wrong on first attempt and passed while asserting nothing. Writing them properly exposed a real hole.

---

## Patterns that keep recurring

These came out of doing the work, not from planning it, and every one of them cost something. They
apply to every Commandment, so they belong here rather than under any single one. When this document is
deleted, these are the part that must survive into the checker's docstring.

**1. Scope narrowing expressed as CODE instead of data.** Three times, in three unrelated places: my own
checker skipping `build.rs`; the `arch/` role vocabulary; and `is_transient()` in the chaos service
deciding who never faces Maximum Carnage. Each was a small, defensible, invisible edit that shrank what
was examined while everything still reported green. Exclusions belong in data, with a reason each,
printed beside the result. **A pass that does not say what it looked at is the most convincing kind of
lie.**

**2. A pin that vanishes passes vacuously.** Appending a plain key below a `[kernel.sub-table]` header
makes TOML swallow it, so the lookup returns nothing, which reads as an EMPTY pin - and an empty pin
permits everything. This landed four times while writing these checks. The failure is not a wrong
answer; it is a confident answer over a surface nobody is watching any more. Now caught by
`integrity-baseline`.

**3. A badly written probe does not fail loudly - it agrees with you.** Two of three ratchet probes were
wrong on first attempt: they asserted nothing and passed. Writing them properly exposed a real hole (a
role pinned for a deleted file was ignored). The corpus needs the same suspicion as the code.

**4. Measuring changes the design, every single time.** Anti-scope vocabulary scanning, a kernel
line-count ceiling, and "every service must be a chaos target" were all sound-sounding checks that
measurement killed - the third because chaos keeps no target list at all, so there was nothing to check
and the entire risk was at the other end. Every check written AFTER measuring has held; none written
from intuition survived contact.

**5. Pins catch addition at the surface they pin, and miss growth inside it.** `InspectKernel` was one
pinned syscall with 23 unpinned sub-queries behind it. A 24th would have been a new kernel
responsibility that changed no visible surface at all. Any surface that dispatches on an id needs its
id space pinned too.

**6. The shape of the pin is part of the policy.** `kernel_spawned_service = "supervisor"` is a scalar,
not a list, because a list has room for a second entry and appending one reads like configuration.
Where a rule permits exactly one of something, express one, so that a second requires changing the
schema rather than adding a line.

**7. Illegitimate things survive because something legitimate is parked next to them.** `control.rs` is
developer tooling that §4.4 forbids by name, and it cannot simply be stripped from production builds
because the supervisor respawn runs inside `control::process_pending`. It has survived not because
anyone defended it but because removing it would take a real responsibility with it. Worth actively
looking for elsewhere.

**8. Things that look like one finding often need different answers.** `control.rs`, `clock.rs`,
`wallclock.rs` and `fbcon/` all failed the same check for the same stated reason, and the right answers
are *separate then gate*, *move to a planned service*, and *amend the constitution*. Do not batch a
finding's remedy just because the check batched its detection.

## What a Commandment costs, roughly

Consistent enough after two to be worth stating. Each one produces: a handful of pins; one or two
sound-sounding checks that measurement kills; and **at least one finding nobody knew about**. Commandment
I yielded four responsibilities the kernel does not admit to having, a kernel spawn set of three where
the law says one, and a policy catalogue of 218 services. None of that was visible before counting.

That last part is the argument for the slow pass. The checks are the deliverable, but the findings are
what the checks are FOR, and they only appear when something is counted for the first time.


---

## Commandment I - thou shalt not expand the responsibilities of the kernel

**Encoded: 11 checks, 44 self-test probes. Findings: 6 - C1-4 closed, five open, two of them High.**

### Why these surfaces

The commandment names six kernel responsibilities and says "nothing else"; §4.4 lists the anti-scope
explicitly. Rather than police that prose, the checks pin the **surfaces a new responsibility must
arrive through**: syscalls (new kernel verbs), well-known authorities (new kernel nouns - several exist
precisely because a syscall alone could not express the authority), ring-0 dependencies (code the
kernel did not write but trusts absolutely), top-level modules, per-file roles under `arch/`, and the
kernel's own spawn set.

The `arch/` role pin took the most thought. `arch/` is legitimate - a kernel must bring up its own CPU,
MMU, timer, interrupt controller and console - so pinning module *names* cannot see a USB stack hiding
inside a legitimate module. Every file under `arch/` therefore declares what it **is**, from a fixed
vocabulary. Smuggling a driver in now requires writing a lie into the baseline, in a diff, rather than
choosing an innocuous filename nobody re-reads.

### The one sanctioned exception

The kernel restarts exactly one service, the supervisor, and it must: Commandment V says no service is
special, so the supervisor must be restartable too, and only the kernel is beneath it to do the
restarting (§11.1, §6.2, `naming-design.md` §3.7 - a sliver of §26.10 traded for maximum fault
tolerance). Pinned as a single string for the reason in the process notes above.

### The gap that pins do not close

All of these checks are **pins**, and a pin catches ADDITION at the surface it pins while missing growth
INSIDE it. `InspectKernel` proved the point: one pinned syscall carrying 23 sub-queries, each a distinct
thing the kernel will answer (allocator counts, scheduler ticks, RTC time, framebuffer dimensions, PCI
ids, a hardware random number). A 24th would have been a new kernel responsibility that changed no
visible surface at all - the syscall count stays 49.

Pinned now (C1-4). The general rule this yields: **any syscall that dispatches on an id needs its id
space pinned too**, or the pin above it is watching the door while the room extends out the back.

Two of the three gaps named here are now pinned: kernel **feature flags** (22 - a feature is a switch on
what the kernel IS, and the C1-1 scaffolding lives behind two of them) and the kernel's **per-service
config table** (218 - memory limit, placement, capabilities and embedded ELF for every service).

The third is NOT encoded, deliberately. Which **IRQs the kernel services itself** rather than routing to
a driver (§12) is scattered per-arch in different shapes, and "handled in-kernel" versus "routed" is not
expressed as one table anywhere. A partial version would look like coverage without being it, which is
worse than the honest gap.

### Considered and REJECTED

**Anti-scope vocabulary scanning** (`inode`, `tcp_`, `work_steal`, ...). Measured first: almost every
hit is a comment, and every `migrat` hit is code that *forbids* migration
(`assert_no_mid_execution_migration`). A check with that false-positive rate teaches people to ignore
it, which is worse than no check.

**A kernel line-count ceiling** for "the kernel remains tiny". It contradicts the commandment's own
text, which explicitly welcomes new hardware support and new CPU architectures - adding an arch would
blow a global ceiling while being permitted. The commandment counts **responsibilities**, not lines,
and a proxy that fires on sanctioned work only trains people to raise it.

### Findings

| ID | Finding | Severity | Closes by |
|----|---------|----------|-----------|
| C1-1 | The kernel can spawn three services by name, not one | Medium | fix (remove) or amendment |
| C1-2 | `arch/` is 65% of the kernel | Observation | no action expected |
| C1-3 | Font crate linked for a framebuffer console, unjustified in writing | Low | amendment or make it a service |
| C1-4 | `InspectKernel` carries 23 unpinned sub-queries | Medium | CLOSED by the I-introspect pin |
| C1-5 | The kernel holds a catalogue of userspace policy for 218 services | **High** | open |
| C1-6 | Four kernel modules serve none of the six responsibilities | **High** | open |

**C1-1.** `arm_spawn_logger_neutral()` and `arm_spawn_shell_neutral()` in `task/mod.rs` are ARM/AArch64
bring-up scaffolding from before the supervisor path worked, called from `sched_spawn.rs` and
`sched_shell.rs` under the `arm-sched-spawn` and `arm-shell` features. Production uses
`arm-supervisor`, so **neither runs in a shipping kernel** - not a live bug.

What raises it above dead code: both are gated on **architecture, not on the features that call them**
(`#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]`), so they compile into every ARM and
AArch64 kernel regardless of scheduler feature, including the Pi 4 mainline. No amendment covers
either, so neither is exemptible. Found only because the exception was written down and the count
checked.

**C1-4 (closed).** `InspectKernel` was one pinned syscall with an unpinned query space behind it. Now
pinned. Kept in the table rather than deleted because it is the finding that produced the general rule
above, and the rule is worth more than the fix.

**C1-5.** The kernel's `service_config` table holds memory limit, placement core, capabilities, send
peers and the embedded ELF for **218** services, roughly 190 of them test probes.

Raised to High after a correction that sharpened it: the finding is not "too many probes", it is that
**the catalogue should not be in the kernel at all**. Loading a task is mechanism and belongs to the
kernel. A catalogue of what every service is allowed is policy, and policy belongs to the supervisor
(§26.10). The probes are the most visible symptom, and §4.4's ban on "developer tooling" in the kernel
covers them, but removing only the probes would leave the architecture unchanged.

The target follows from the rule already established for spawning: the kernel knows about **one**
service, `supervisor`, because nothing is beneath it to bootstrap it. Every other service's policy
belongs to the supervisor, with its image handed to the `Spawn` syscall rather than compiled into ring
0. The pin is therefore recorded as DEBT that may only shrink - there is no "add it deliberately" path -
and 218 is the distance from where this should be.

**C1-6. The count is six; four modules serve something else.** §4.3 names exactly six kernel
responsibilities and §4.4 says "nothing else". Every top-level module now claims one of the six, or a
support role sanctioned elsewhere in the constitution and citing where (`arch-layer` §4.1, `syscall-entry`
§8.2, `boot-entry` §11, `kernel-log-floor` §11.4, `invariants` §3/§22). Four can claim neither:

| Module | What it is | Why it is not one of the six |
|--------|-----------|------------------------------|
| `control.rs` | COM2 developer command channel **and** the core-0 tick that runs the supervisor respawn | §4.4 forbids "developer tooling" by name |
| `clock.rs` | clock deglitch logic | Timekeeping is not among the six |
| `wallclock.rs` | wall-clock provenance and floor | Timekeeping, and stateful policy at that |
| `fbcon/` | framebuffer text console, 1,172 lines | §11.4 sanctions a ring buffer plus a SERIAL console |

They look alike and their answers are three different things.

**`control.rs` - separate it, then gate it.** It is `mod control;` in `main.rs`, unconditional, in every
kernel. But it is no longer only a dev channel: `control::process_pending` is where the **supervisor
respawn** runs (Path C / Phase 6) plus other core-0 periodic work. So it cannot simply be stripped from
production builds - that would remove the respawn. The respawn tick belongs to `task/` (scheduling, one
of the six); the COM2 command channel is tooling and should be feature-gated out. Welding them together
is *why* the tooling has survived in ring 0.

**`clock.rs` + `wallclock.rs` - a time SERVICE, planned.** Not open questions: they close when that
service exists. `wallclock.rs` is the one that most wants to leave, holding provenance and floor policy.

**`fbcon/` - probably an amendment, not a removal.** It is the kernel's console output path on every
arch (`put_byte`, `mirror`, `clear_and_home` from each `arch/*/mod.rs`). On a Pi with an HDMI TV and no
serial cable the framebuffer IS the console; without it the kernel is mute. And a console cannot be a
service for the same chicken-and-egg reason the ring buffer cannot: it must exist before any service
does, to report a boot that fails before services exist. §11.4 is simply **x86-shaped** - it says
"serial console" because on a PC serial is always there. The narrower questionable part is the slice
serving USERSPACE (console dimensions via `InspectKernel` query 9, and the rendering the shell drives);
the boot console is not the problem.

This check is also the one that answers "how many responsibilities does the kernel have" mechanically.
The number is pinned at six, and changing it fails the build - amending the constitution rather than
editing a config.

**C1-2.** `arch/` is 28,755 of ~44,000 kernel lines. Legitimate (hardware support is welcome), noted
because the majority of the kernel lives in the layer these checks scrutinise least - the role pin says
what a file *claims* to be, not what it does.

**C1-3.** `noto-sans-mono-bitmap` is linked into the kernel for the framebuffer console (`fbcon/`, 1,172
lines). §4.4 forbids "logging infrastructure" and §11.4 sanctions only a 16 KiB ring buffer plus serial.
It may well be justified - the finding is that it is not justified **anywhere in writing**. Either it
earns a sentence in the constitution or it is a service.

---

## Commandment II - thou shalt love Chaos and trust in it

**Encoded: 1 check (the static half). Runtime half deferred, NOT declared impossible. 1 finding.**

Chaos keeps no target list - it scans the live task table, so a new service is a candidate
automatically and there is nothing to drift. All the risk is at the other end, in `is_transient()`:
three lines inside the chaos service naming who never faces Maximum Carnage. That is pinned now, so a
service cannot be quietly removed from the storm's reach while every suite still reports green.

**The rule is encoded by there being nowhere to say otherwise.** There is no list of services allowed to
escape, and no scalar declaring the policy either - both were removed deliberately. A place to record an
escape is a place to add one, and a stated target sitting beside its own exceptions teaches a reader to
treat every target in this file as a wish. The only list is chaos's own APPARATUS (`chaos`,
`mem-pressure`), which is the instrument, not a set of exceptions.

A genuinely necessary exception is not impossible - it goes through the same door as every other
violation: a CLAUDE.md amendment plus an `[[exemption]]` citing it. The light path is gone; the heavy,
arguable one remains.

**This makes the build RED today**, on C2-1 below. That is the intended consequence rather than an
oversight: the alternative was a debt list, and a debt list is a place to add a second entry.

**C2-1 (Medium). `observe` is a real service and never faces the storm.** Of the three exclusions, two
are chaos's own apparatus rather than services escaping it: killing `chaos` mid-run stops the storm
measuring anything, and `mem-pressure` tasks are the ammunition it spawns. `observe*` is different - it
is a prefix rule, and `observe` is a genuine service. It is the one real distance from the target, and
the reason the target is written down rather than assumed.

The runtime half - was chaos RUN, did it PASS - cannot be a lint, but it can be a GATE: evidence of a
passing run, bound to a commit id. Deferred rather than impossible, for two reasons. It only has value
once findings are being fixed (a gate that always fails is noise), and it needs a stale-evidence guard
or it rubber-stamps a run from three commits ago. Recorded as **not-yet-built**, which is a different
category from **un-encodable**; III and X are expected to be the latter.

---

## Commandment III - thou shalt not duplicate truth

**In progress. One finding already, and it is a live bug.**

More encodable than expected. The commandment is careful - derived views are allowed if reconstructible,
reconciled and subordinate - and judging that is review. But a **second irreducible truth** has a
mechanical signature when it takes the commonest form: one fact, defined independently in several
places, under the same name.

**C3-1 (High, live). "The largest ethernet frame" is defined six times and the copies disagree.**

| Location | Value |
|----------|-------|
| `kernel/src/arch/arm/dwc2.rs` | 1600 |
| `kernel/src/syscall/dispatch.rs` | 1600 |
| `services/nic-driver/src/genet.rs` | 1600 |
| `services/nic-driver/src/main.rs` (module level) | 1600 |
| `services/nic-driver/src/main.rs:575` (shadowing local) | 1600 |
| `services/dwc2/src/net.rs` | **1514** |

Plus a seventh copy as a bare literal in nic-driver's arm backend (`[0u8; 1 + 1514]`).

A frame between 1515 and 1600 bytes is therefore accepted by `nic-driver` and silently TRUNCATED by the
dwc2 IPC path. Not a style complaint: this is precisely what the commandment predicts, since with no
single source the copies drift, and the drift stays invisible until a 1520-byte frame arrives. Both the
1514 and the literal were written during the arm32 USB work in this same session, by someone who had
just read this commandment.

Two of the copies even carry comments ADMITTING the duplication ("matches nic-driver's FRAME_MAX"),
which is the tell worth generalising: a comment asserting that two values agree is a second truth with a
note attached, not a solution.

### The check was measured and REJECTED

"Identically-named constants with different values" sounded nearly false-positive-free. Measuring it
says otherwise: **45 hits tree-wide, 41 across crate boundaries, and nearly all are correct.**

Three legitimate patterns swamp the signal:

* **Per-arch values, which MUST differ.** `ELF_MACHINE` has six distinct values because it names six
  ISAs. `PL011_BASE`, `USER_END`, `PM_RSTC`, `GPIO_BASE`, `DMA_ARENA_UNCACHED`, `ZERO` - all correct.
* **The same name meaning different things in unrelated modules.** `DATA_OFF` is a local buffer offset
  in both `ahci` and `dwc2`; `CLASS_MASS_STORAGE` is a PCI class in one file and a USB class in another;
  `STATUS` is an IOMMU register and an SDHCI register.
* **The same word for different concepts.** `NAME_MAX` is 32 in the kernel (SERVICE names in the name
  directory) and 38 in `fs` (FILE names). Checked by hand - not a disagreement at all.

A build-failing rule here would be roughly 90% false positives, and a check people learn to ignore is
worse than no check. **Rejected as a gate.** The scan is worth keeping as a REPORT a human reads
occasionally, which is how `FRAME_MAX` was found in the first place.

### Why III mostly resists mechanisation

The commandment is deliberately careful: a derived view is ALLOWED if it is reconstructible, reconciled
and subordinate. Deciding whether a stored value satisfies those three is design review, not pattern
matching - `fs`'s free bitmap is legitimate precisely because `fsck` rebuilds it from the file tree, and
nothing in the source distinguishes that from a second truth except intent.

So III is recorded as **largely un-encodable**, with one narrow mechanical residue (the scan-as-report)
and one live finding. That is a different category from II's runtime half, which is *not-yet-built*.

---

## Commandment IV - thou shalt honor service contracts

**Measured, not yet encoded. The largest gap of the first four.**

| | |
|---|---|
| Services shipping a contract | **6 of 14** |
| What `scripts/contract_check.py` reconciles | memory limit, placement core, `ipc_send` |
| Authority granted BY NAME in the kernel, outside any contract | **10 kinds** |

The ten: `acquire_any`, `console_push`, `has_console_read`, `introspect`, `net_device`, `reboot`,
`send_peers_grant`, `service_control`, `set_clock`, `usb_disk`.

So most authority in this system is granted through a path the commandment says should not exist - "do
not invent hidden communication paths ... if a service cannot express its needs through its declared
contract, redesign the contract - not the architecture". There IS a doctrine sanctioning it
(`docs/userspace-audit.md` U15, and `task/mod.rs` calls them "deliberately NOT contract capabilities"),
but that is a doc rather than a constitutional amendment, while §13.3 says the developer declares what
the service needs and the OS decides whether to grant it.

**C4-1 (High, provisional). Two thirds of the authority mechanism bypasses the contract mechanism.**
Provisional because the right answer may be an amendment rather than a fix - if by-name grants are the
correct design, §13 should say so. What is not defensible is the current state, where the constitution
describes one mechanism and the kernel uses another, with the reconciliation living in an audit doc.

**C4-2 (Medium). Eight of fourteen service crates ship no contract at all.**

The check shape follows from the measurement, and is mechanical: every service crate ships a contract;
every by-name grant is declared in that service's contract, or listed in a pinned exception set citing
an amendment. `contract_check.py` folds in as the third leg.

Not built. A check that cannot be red-teamed in the same sitting is exactly the thing that looks like
coverage without being it.

---

## Commandment V - thou shalt not assume thy service is special

**Half encoded (`V-no-panic`, red-teamed). Second half measured, not built. 1 finding, and it is live.**

The panic half is done: no service may halt the machine. The text carries two more obligations, and
measuring the first found a real hole.

**C5-1 (High, live). `dwc2` is spawned but never watched, so it dies permanently.**

The supervisor watches nine services - `block-driver`, `counter`, `ehci`, `fs`, `logger`, `net-stack`,
`nic-driver`, `shell`, `xhci`. `dwc2` is not among them. On the Pi 2 that service IS the USB stack:
storage, keyboard and networking all ride on it. If it dies it stays dead, and all three go with it
until reboot - which is precisely the "special service" the commandment forbids, arrived at by
omission rather than by decision. Introduced during the arm32 USB work in this same session.

The check shape is DERIVABLE rather than declared, which is the preferred form: **any service named as
a `send_peer` by another service must be watched for restart.** If A depends on B and B dies
permanently, A is broken forever. `block-driver` and `nic-driver` both declare `dwc2` as a peer, so the
rule catches it with no list to maintain and nothing to append.

**Still unmeasured: recovery whose own failure is discarded.** "A recovery whose own failure is
discarded becomes a silent success, and the service then proceeds on a state it never actually
reached." This is the failure that bit hardest this session - the slice-3c blocker was a discarded
`None`, and the receive path spent three boots on a discarded `HCINT`. A `let _ =` on a retry or
reacquire is the mechanical shape, and its false-positive rate is unmeasured.

**Runtime half, not built: a service must not HANG.** No static check reaches it. The dependency
matrix - kill each dependency, assert every caller still answers - is shared machinery with IX.

---

## Commandments VI - X

Not yet walked. VI is partly encoded (`VI-static-mut`, red-teamed). `--report` lists each and why it is not mechanised, so the gap shows on every build
rather than only here.

Order and reasoning: **IV** next (fold in the existing `contract_check.py` - nearly free, and it has
already caught a real violation). Then **V's second half with IX**, because they are one machine: the
runtime dependency matrix, kill each dependency and assert every caller still answers. Highest value
outstanding - the only thing that would have caught the hangs, and no static check ever will. Then
**VIII** (heuristic, noisy, allowlist-driven), then **II** as a gate with a threshold rather than a
lint, then **VII**. **III** and **X** are expected to stay judgment, and to be printed as such.
