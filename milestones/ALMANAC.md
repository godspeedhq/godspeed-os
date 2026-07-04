# The Almanac of GodspeedOS - A History of Understanding

> **What this is.** Not a changelog. Git already records *what* changed and *when*; this
> records *why the thinking changed*. Every entry below pins a date to a realization - a
> thing the project came to understand that it had not understood the day before - and
> then names the principle, commandment, or design decision that the realization produced.
> It is the intellectual history of the system, told backward from the rules to the days
> that earned them.
>
> It is a **record, rarely touched** - a reflective document to look back on, not a living
> one. For the law, read [`CLAUDE.md`](../CLAUDE.md); for its distillation, read
> [`COMMANDMENTS.md`](../COMMANDMENTS.md). This file is the answer to the question a
> contributor eventually asks at a whiteboard: *"Why is it written that way?"* The dates
> are real, anchored to commits and to the dated amendments in the constitution. The
> reasons are the part worth keeping.

---

## Contents

- [2026-05-08 - The day the screen first said the machine was alive](#2026-05-08---the-day-the-screen-first-said-the-machine-was-alive)
- [2026-05-09 - The day the nouns became verbs](#2026-05-09---the-day-the-nouns-became-verbs)
- [2026-05-10 - The day two services talked across a boundary](#2026-05-10---the-day-two-services-talked-across-a-boundary)
- [2026-05-12 - The day a service died and came back](#2026-05-12---the-day-a-service-died-and-came-back)
- [2026-05-21 - The day the model met a real machine](#2026-05-21---the-day-the-model-met-a-real-machine)
- [~2026-05-30 - The day the thin client taught me about real concurrency](#2026-05-30---the-day-the-thin-client-taught-me-about-real-concurrency)
- [2026-06-09 - The day a trusted service stopped being trusted (H11)](#2026-06-09---the-day-a-trusted-service-stopped-being-trusted-h11)
- [2026-06-12 - The day I found kernel-equivalent power hiding in a driver (H1)](#2026-06-12---the-day-i-found-kernel-equivalent-power-hiding-in-a-driver-h1)
- [2026-06-17 - The day a crash stopped meaning a reboot (Phase D)](#2026-06-17---the-day-a-crash-stopped-meaning-a-reboot-phase-d)
- [2026-06-18 - The day a file became a real capability (P2)](#2026-06-18---the-day-a-file-became-a-real-capability-p2)
- [2026-06-20 to 2026-06-21 - The day I deleted the registry and learned what is truly irreducible](#2026-06-20-to-2026-06-21---the-day-i-deleted-the-registry-and-learned-what-is-truly-irreducible)
- [2026-06-21 - The day nothing was allowed to be unkillable but the kernel](#2026-06-21---the-day-nothing-was-allowed-to-be-unkillable-but-the-kernel)
- [2026-06-21 - The day Maximum Carnage was born](#2026-06-21---the-day-maximum-carnage-was-born)
- [2026-06-22 to 2026-06-28 - The days Carnage found the wedges no one would write by hand](#2026-06-22-to-2026-06-28---the-days-carnage-found-the-wedges-no-one-would-write-by-hand)
- [2026-06-25 - The day I wrote the constitution down as ten lines](#2026-06-25---the-day-i-wrote-the-constitution-down-as-ten-lines)
- [2026-06-29 - The day the examples had to teach the truth](#2026-06-29---the-day-the-examples-had-to-teach-the-truth)
- [2026-06-30 to 2026-07-01 - The day "wait on truth" turned out to be half a truth](#2026-06-30-to-2026-07-01---the-day-wait-on-truth-turned-out-to-be-half-a-truth)
- [2026-07-03 - The day the shell became a language, and a 10 MB script was a non-event](#2026-07-03---the-day-the-shell-became-a-language-and-a-10-mb-script-was-a-non-event)
- [2026-07-04 - The day the system survived a million acts of violence](#2026-07-04---the-day-the-system-survived-a-million-acts-of-violence)
- [2026-07-04 - The day gsh was finished, and never once reached for a heap](#2026-07-04---the-day-gsh-was-finished-and-never-once-reached-for-a-heap)
- [TBA - The day a private discipline becomes a public one](#tba---the-day-a-private-discipline-becomes-a-public-one)
- [The Days I Was Wrong](#the-days-i-was-wrong)
  - [~2026-06-21 - The day the constitution rejected its author](#2026-06-21---the-day-the-constitution-rejected-its-author)
  - [~2026-06-27 - The day I reached for a heap](#2026-06-27---the-day-i-reached-for-a-heap)
  - [2026-06-28 - The day my own test lied to me](#2026-06-28---the-day-my-own-test-lied-to-me)
- [The Named Bugs - the teachers](#the-named-bugs---the-teachers)

---

## 2026-05-08 - The day the screen first said the machine was alive

The repository opened, and by the end of the day the kernel booted to a steady state under
QEMU - Limine handoff, serial, GDT, IDT - after two x86 correctness bugs were found and
fixed in the same sitting.

**What I came to understand:** a boot is not a feature you add; it is a fact you either
observe on the wire or you do not have. The very first thing built was not the kernel - it
was the *ability to see the kernel*: serial output before anything else.

**What it produced:** the instinct that became *failures are loud, never silent* (Invariant
12) and Commandment VIII's deeper sibling - wait for truth, not time. From day one, progress
was something the serial console *showed*, never something assumed.

## 2026-05-09 - The day the nouns became verbs

In a single push the kernel gained a frame allocator and page tables, a preemptive scheduler,
capability enforcement with generation checks, and synchronous IPC.

**What I came to understand:** the constitution's vocabulary - capability, generation,
endpoint, quantum - was not description; each word had to *be* a mechanism or it was a lie.
A "capability" that is not checked on every syscall is just a number.

**What it produced:** the discipline behind Commandment VII (no ambient authority) and the
§7 capability model - authority is the cap you hold and the generation it carries, validated
every single time, never inferred from who you are.

## 2026-05-10 - The day two services talked across a boundary

Cross-core IPC came up after a stack overflow and a silent-exception halt were chased down,
and the first user-space service ran in ring 3: an ELF loaded, spawned, and logging through
a syscall.

**What I came to understand:** the moment code crossed the kernel boundary, the kernel's job
stopped being "do the work" and became "route, isolate, and enforce." A service is not a
privileged part of the kernel that happens to live elsewhere; it is an untrusted identity
the kernel mediates.

**What it produced:** Commandment I (the kernel is complete; use a service) and §26.10 (the
kernel is mechanism, not policy). The line drawn that day - work lives in services, the
kernel only mediates - never moved.

## 2026-05-12 - The day a service died and came back

The v1 milestone met its bar: ping on core 0, pong on core 1, a message across the boundary,
pong killed, the supervisor restarting it, the system continuing.

**What I came to understand:** the headline was never "two services talk." It was "one can
*die* and the other does not care, because it talks to a *name*, not a place." A client that
saw `EndpointDead`, looked the name up again, and resumed had just demonstrated the whole
philosophy in miniature.

**What it produced:** Invariant 11 (identity is stable; location is not) made concrete, and
the seed of Commandment IX - recovery is the client's job, and a restart is normal, not
exceptional.

## 2026-05-21 - The day the model met a real machine

GodspeedOS booted on real x86_64 hardware via UEFI USB (`CLAUDE.md` §23.3). The bring-up
machine was a **Dell Wyse 5070 thin client** - the **Intel J5005 (Goldmont+)** of the first
performance tables. Four cores up, cross-core ping/pong running on metal.

**What I came to understand:** QEMU is a model of a machine, and a model is a place where bugs
hide politely. Real silicon does not round off the corners. A thing is not "done" because it
is green in emulation.

**What it produced:** the standard that would later harden into Commandment II - chaos and
correctness are judged on hardware - and the habit of treating a clean QEMU run as a
hypothesis, not a verdict.

## ~2026-05-30 - The day the thin client taught me about real concurrency

On the Wyse, bare-metal boot froze shortly after the supervisor came up, and a cross-core
reply-to-the-BSP path stalled hard under load. The diagnosis (`bugs/1_FINDINGS_AP_TO_BSP_IPI.md`,
2026-05-30): core 0 wedged with interrupts disabled, so a sender blocked on `recv` on the BSP
was never woken. Development moved to the **HP T630 (AMD GX-420GI, Jaguar/Puma+, 4 cores)**,
the permanent SMP test machine thereafter.

**What I came to understand:** concurrency bugs are not abstract; they are *specific to the
silicon that exposes them*. The J5005's stall was not a flaky machine - it was the machine
telling the truth about an IF=0 wedge that QEMU and a friendlier CPU had both forgiven.

**What it produced:** the conviction behind Commandment VIII (rely on observable truth, not
timing) and a permanent test rig on hardware that does not forgive. Cross-core correctness is
only honest on a machine willing to deadlock you.

## 2026-06-09 - The day a trusted service stopped being trusted (H11)

`registry` became a real user-space name service holding only delegated caps, so its death
degraded name resolution temporarily instead of corrupting the system - and it left the
Trusted Computing Base (`CLAUDE.md` §6.1, Amendment 2026-06-09).

**What I came to understand:** "trusted" had been doing two jobs at once - "holds authority"
and "cannot be allowed to die." Those are different claims, and conflating them is how a TCB
quietly grows.

**What it produced:** the first real motion of the TCB-shrink program, and the early form of
Commandment V - no service is special. A name service that resolves a stable name is itself
just an identity, replaceable like any other.

## 2026-06-12 - The day I found kernel-equivalent power hiding in a driver (H1)

IOMMU DMA-confinement landed (`CLAUDE.md` §6.4, Amendment 2026-06-12). A DMA-capable driver,
confined, reaches only its granted arena; the trust posture - confined or trust-critical - is
now printed at boot.

**What I came to understand:** the very first invariant, *no ambient authority*, had an
unstated exception I had never written down. A driver could aim its controller's DMA engine at
any physical address - reach the capability model never granted, granted instead by physics.
The model said "no ambient authority" while a USB driver quietly held the most ambient
authority in the machine.

**What it produced:** the amendment that closed the gap in Invariant 1, and Commandment VII
made literal at the hardware edge - authority must be *visible*, so the posture is a boot fact,
not a hidden assumption. A silent exception to an invariant is worse than no invariant, because
it lies.

## 2026-06-17 - The day a crash stopped meaning a reboot (Phase D)

With a crash-consistent redo-journal and recovery-on-mount, `fs` and `block-driver` became
restartable and left the TCB (`CLAUDE.md` §6.1, Amendment 2026-06-17).

**What I came to understand:** the only reason the filesystem had been unkillable was that it
could not *recover* - and "cannot recover" is a property you can engineer away, not a law of
nature. Once recovery existed, the special status evaporated.

**What it produced:** Commandment IX made concrete (plan for recovery, and a service that can
recover need not be trusted) and another rung down the TCB ladder toward `{kernel}`.

## 2026-06-18 - The day a file became a real capability (P2)

Delegated resource capabilities arrived (`CLAUDE.md` §7.10, Amendment 2026-06-18): `fs` mints
a genuine, kernel-validated, revocable cap on open. The same day, the full persistence stack
and the file-as-capability self-check ran green on the **HP T630** against a real SSD - a file
opened as a cap, read and written *through* it, non-escalation enforced, a forged handle
rejected, the cap revoked on close.

**What I came to understand:** the meaning of a file belongs to a *service*, never to the
kernel. The kernel does not need to know what a file *is* to mint, route, and revoke its
capability - it only needs to carry an opaque resource id and let `fs` supply the meaning. "A
file is *like* a capability" was a compromise; "a file *is* a capability" was achievable, and
the difference was the whole point.

**What it produced:** the §7 north star - a file *is* a capability, literally, not by analogy -
and a reinforcement of Commandment X (complexity in the layer that owns it): file semantics
live in `fs`, capability mechanism lives in the kernel, and neither reaches into the other.

## 2026-06-20 to 2026-06-21 - The day I deleted the registry and learned what is truly irreducible

Over a phased migration, name-to-endpoint resolution moved out of the kernel and the **registry
service was retired outright** (`CLAUDE.md` §6.1, Amendment 2026-06-21, "Path C"). A minimal
`name -> endpoint` directory inside the kernel became the namer; the supervisor wires services
from a `name -> cap` map; clients reacquire by name.

**What I came to understand:** an entire service had existed to *store* a mapping that turned
out to be almost entirely derivable - the supervisor already knew the caps, the kernel needed
only a tiny recovery anchor. Deleting it revealed how little state is genuinely *irreducible*,
and how much "truth" is really a derived view dressed up as a source.

**What it produced:** this is the day behind **Commandment III** - *do not duplicate truth;
store the irreducible fact, derive the rest.* When someone asks why III is written the way it
is, the honest answer is: because deleting the registry showed that only a tiny amount of state
was ever truly irreducible, and everything else should be reconstructed, not stored twice.

## 2026-06-21 - The day nothing was allowed to be unkillable but the kernel

Two long-standing exceptions fell at once. The **shell was made restartable** - `kill shell`
respawns a fresh prompt instead of leaving a dead session. And in the Path C finale, the
**supervisor itself became restartable**: the kernel respawns it on death, unconditionally and
forever, and the respawned supervisor reconciles by adopting the still-running services. The
non-restartable set reached its floor: `{kernel}` alone.

**What I came to understand:** every "this one cannot die" was a comfort, not a requirement. If
the *supervisor* - the holder of restart authority - can itself be respawned by the kernel, then
no service has any standing to claim exemption. "Trusted" finally, fully stopped meaning
"unkillable."

**What it produced:** **Commandment V** in its strongest form - *no service is special; only the
kernel is special* - grounded in Invariant 6 (services must be restartable) and §6.2-6.3 (the
unkillable set is `{kernel}`). Identity survives restart; only the kernel is location.

## 2026-06-21 - The day Maximum Carnage was born

`chaos max-carnage` arrived - a storm that kills a random live service every round and asserts
the kernel survives. It grew into a per-round sweep over every live service, mixing kills,
queue floods, memory pressure, and spawn storms.

**What I came to understand:** passing tests proves a system handles the failures you *imagined*.
It says nothing about the ones you did not. The only way to trust a system is to spend real
effort trying to destroy it and watch it refuse.

**What it produced:** **Commandment II** - *love Chaos and trust in it; thy service shall pass
through Maximum Carnage* - and the standard that a green unit suite is necessary but never
sufficient. Chaos is the teacher; if it finds a bug, the bug already existed.

## 2026-06-22 to 2026-06-28 - The days Carnage found the wedges no one would write by hand

The storm did exactly what it was built to do. A **71K-round SMP deadlock** from concurrent TLB
shootdowns (fixed 2026-06-24 with per-core shootdown). A **supervisor-respawn wedge** where
respawn ran inside an IF=0 timer ISR and could not acknowledge shootdown IPIs (fixed 2026-06-23
by moving respawn to an interrupt-enabled point). And at high round counts on the T630, a
**page-table reclaim page fault** where a driver kill followed a corrupt PTE into an out-of-RAM
frame - which led to the root-cause fix: **quiesce DMA on driver kill** (2026-06-28).

**What I came to understand:** these were not new bugs Carnage *created* - they were old bugs
Carnage *revealed*, latent since the day the code was written, waiting for the one interleaving
that QEMU and light load never produced. A system you have not tried to break is a system whose
real failure modes you have simply never met.

**What it produced:** the conviction under Commandment II made unarguable, and the practice of
**long soaks on real hardware as the acceptance bar** - the verdict tightened every time a soak
exposed a wedge, because a test that passes a known-broken target is a weak test, not a clean one.

## 2026-06-25 - The day I wrote the constitution down as ten lines

`COMMANDMENTS.md` was authored - the Ten Commandments of Godspeed, each grounded in the invariants
it enforces.

**What I came to understand:** the greatest long-term threat to the system was never a race or a
fault - it was *erosion*. Hidden complexity, a convenience here, a silent fallback there, each
reasonable on its own, would rot the model one "just this once" at a time. And a 2,700-line
constitution is hard to defend a pull request against in the moment; ten memorable lines are not.

**What it produced:** the realization that **the architecture, not the code, is the product**
(§26.1) - which is *why a constitution exists at all*, and why it has a human-readable
distillation. The Commandments were written in response to real bugs, real wedges, and real
lessons; they are the compressed memory of everything above.

## 2026-06-29 - The day the examples had to teach the truth

The example library was rebuilt as a Commandment-grounded reference for contributors, modelling
the real patterns - placement defaults, explicit capability passing - rather than carrying stale
registry references from a retired design.

**What I came to understand:** demonstration code that no one runs drifts into a lie, and a lie in
the examples teaches the *wrong model* to exactly the people most willing to learn it. Examples are
documentation that executes; if they are not tested, they are not true.

**What it produced:** the examples folded under Commandment IX's rule - *if it cannot be tested, it
does not exist* - applied to teaching material, and a reaffirmation that the model is only as honest
as the smallest program that claims to follow it.

## 2026-07-03 - The day the shell became a language, and a 10 MB script was a non-event

gsh grew from a flat command-runner into a real programming language - variables, arithmetic,
conditionals, `switch`, loops, functions with bounded recursion, libraries (`import`), `defer`,
typed-pipe aggregators, `$( )` capture, and console input with a secret-taint guard rail - built the
way everything here is built: fixed stacks, bounded arenas, streaming, no heap, loud on every ceiling.
Then a **10 MB script was run, and nothing happened.** That is the whole point.

**What I came to understand:** the ~7 KiB cap on a run-from-file script had felt like a limitation to
apologize for. The 10 MB test inverted it. A heap-backed loader meeting a 10 MB file has a bad day - it
allocates, it fragments, maybe it runs out of memory mid-parse. The streaming minifier meets the same
file, reads about 7 KiB of it, prints one loud line - *a huge script is a program* - and carries on. The
bound is not the wall the language hits; it is the thing that lets the language meet a pathological input
and shrug. "Handled gracefully" and "bounded" turned out to be the same sentence.

The design of the language taught that lesson twice more, in miniature, by refusing to let me claim more
than the system could honestly deliver. A secret from `input secret` is a *guard rail against the
accidental echo, not a vault* - because once you may write it to a file, you can always read it back, so
airtight secrecy is impossible by construction, and pretending otherwise would have been a lie the
console told the user. And a function is *not a pipe source* - a boundary I found only by writing a real
program (the selfcheck tour) that tripped over it, `fn greet` quietly shadowing the greet service.

**What it produced:** the proof that §26.6.1 is not a kernel rule - it is *the* rule. The same "change
the representation, do not reach for a heap" that governs the piece table and the record arena governs a
whole userspace language, and it holds from a 7 KiB script to a 10 MB one with no special case. A bounded
system does not fear scale; it renders scale boring. (§26.6.1; Commandment III and the loud-failure
discipline.)

---

## TBA - The day a private discipline becomes a public one

The licenses are chosen - **GPL-2.0-only** for the OS, **Apache-2.0** for the SDK - and the groundwork
for going public is laid. The launch date itself is still **to be announced**.

**What I came to understand:** every lesson above was learned in private, where the only person the
discipline had to convince was me. Public will mean the constitution has to hold against contributors
who did not live through the wedges - which is precisely what the Commandments were written for.

**What it will produce:** the moment the invariants stop being a personal practice and become a public
contract - held by the same identity tests, the same chaos bar, and the same ten commandments that
earned them. The work of keeping the architecture uncorrupted does not end there; it opens. This entry
is dated forward on purpose: it is the almanac's one anticipated day, waiting on the launch to make it real.

---

## 2026-06-30 to 2026-07-01 - The day "wait on truth" turned out to be half a truth

A million-round `chaos max-carnage` soak finished with the system alive but the filesystem gone -
`ls` said "no filesystem," `drives` read raw, and a cold reboot brought it all back. No data was lost;
the disk was intact the whole time. So the bug was not corruption. It was a recovery that never
happened.

The cause was a single wall-clock timer. `fs` waited eight seconds for `block-driver` to report a
readable disk, then gave up. Under a soak that had wedged the AHCI controller, eight seconds was never
going to be enough - the controller needed a hard reset, not more waiting - so `fs` latched "no
filesystem" and sat there. The timer *felt* like the safe, obvious way to wait for a dependency. It
was the trap. Commandment VIII, exactly: wait on truth, not time.

**What I came to understand, and it had a turn in it.** Removing the timer and waiting on
`block-driver`'s actual reply fixed the soak - but following that discipline *rigorously* exposed
something deeper. The request/reply primitive itself could hang: a blocking wait on a reply hangs
forever if the reply never comes, and it never comes if the peer dies mid-request. "Wait on truth" had
quietly become "wait forever" for the one truth it could not see: the peer's *death*. The easy version
of a principle ("do not use timers") and the complete version of it are not the same thing.

So the fix was not only in `fs`; it was in what "truth" means. We taught the kernel that a peer's
death is a truth too - a synchronous Call whose caller wakes with `ReplyDead` the instant its replier
dies, the reply-side twin of the sender-side wake §8.6 already had. seL4's Call/Reply, borrowed for
exactly this. And the principled part, which was the owner's call: putting it in the kernel grows its
*code* but not its *responsibility* - IPC death-semantics were always the kernel's job (Commandment I
is about scope, not line count). Mechanism, not policy: the kernel learned about a *reply cap*, never
about "RPC."

**And then the fix had a hidden failure of its own.** The kernel change passed every green test -
identity, fs-restart, a new ReplyDead pin - and still drained the whole system to zero on the first
hardware soak: recovery of the dead supervisor hung on a single trigger, the timer ISR, which the
storm starves, and the new code's latency was just enough to lose a race main had narrowly won. The
lesson turned on itself. A fix I believed was complete was again only half-satisfied, and the half
missing was the failure case - caught by exactly the fire this whole entry is about. The repair drove
recovery from the storm's own hot path, the yield, making it *un-starvable* - stronger than it had
ever been.

**What it produced.** Commandment VIII, deepened: *wait for truth, not time - but ensure the truth
includes failure.* A wait that cannot observe failure has quietly become an infinite wait on time. A
standard for every contributor (`CONTRIBUTING.md`): interdependent services wait on each other's truth,
the reply or the loud fact of death, never on a clock. And the lesson I keep relearning, now with
teeth: a principle you believe you have satisfied is usually only half-satisfied, and the half you are
missing is what happens when things fail.

---

## 2026-07-04 - The day the system survived a million acts of violence

`chaos max-carnage all-services 1000000` ran to completion, and the system works as expected.

A million rounds. Each round is not a gentle probe - it kills every restartable service, floods every
endpoint it can reach, storms memory and the spawner, and rotates a kill through whatever is left. This
is the same soak that, in the entry just above, drained the system to "no filesystem" on its first
hardware run and taught the hardest version of Commandment VIII. This time it finished. Nothing had to
be rebooted. The prompt still answered.

**A million is not a number you reach by luck.** Fault tolerance that is 99.99% correct dies long
before round ten thousand: leak one frame per kill, fail to reclaim one capability, leave one lock
wedged, and the footprint grows without bound until something falls over. To reach 1,000,000 every
recovery path has to be not merely correct but *conserving* - each respawn reclaims the dead instance's
frames, kstack, and caps *before* it allocates fresh, so the footprint is flat and only a count grows
(§6.2). The kernel respawns the supervisor unconditionally, forever; the supervisor reconciles - adopts
the survivors, respawns the dead; clients see `EndpointDead`, reacquire by name, and retry. `unkillable
= {kernel}`, and the kernel held a million times over.

**This is the entry above, vindicated.** The wait-on-truth fix, the un-starvable respawn driven from
the yield path, the `ReplyDead` reply-side twin - each was built to survive exactly this fire, and now
the fire has burned a million times and the house still stands. Fault tolerance stopped being a claim
in a constitution and became a measured fact. The model was right; the proof is a number with six
zeroes.

**The receipt.** When the storm was called off, `observe now` on the T630 put the whole truth on one
screen:

```text
--------------------------- system state (8 live) ---------------------------
UPTIME: 3d 03:07:34     CPU: C0 99% C1 99% C2 0% C3 0% (49%)     RAM: 5 MiB used / 7 GiB total (0%)
TASK  NAME          CORE  STATE      RESTARTS   QUEUE   UPTIME
 3    supervisor    C0    BlockRecv    142829    0/16      9h
 4    block-driver  C1    BlockRecv    142826    0/16      9h
 5    fs            C1    BlockRecv    997721    0/16      9h
 6    shell         C0    Running      997721    0/16      9h
 7    logger        C0    BlockRecv    143706    0/16      9h
 8    xhci          C1    Running      143704    0/16      9h
 9    ehci          C1    Ready        143706    0/16      9h
drives:  0  data  GSFS  122104 MiB (122074 MiB free)
```

`RESTARTS` is what `observe` calls *deaths recovered, not clean re-runs* - the soak's odometer. `fs` and
`shell` had each come back from death **997,721** times; `supervisor`, `block-driver`, `logger`, and the
two USB drivers each sat near **142,000-143,000** - together roughly **2.7 million** individual service
deaths, every one recovered, not one escalated. And the number that settles it is two lines up: **`RAM:
5 MiB used`**. Flat. After 2.7 million deaths and resurrections and three days of uptime, memory had not
grown by a single page - one leaked frame per death would have bled ~11 GiB; the machine used 5 MiB of
its 7 GiB, and the 122 GiB SSD sat untouched at `122074 MiB free`. That is what "conserving, not merely
correct" (§6.2) looks like the moment you finally read the meter.

---

## 2026-07-04 - The day gsh was finished, and never once reached for a heap

gsh started as a batch runner: `run_lines` split a file on `;` and `\n` and handed each line to
`execute`. It is now a language. Variables and arithmetic, `if` / `switch`, `for` / `loop` with
`break` / `continue`, functions with parameters and bounded recursion, libraries via `import`,
`defer`, record aggregators, typed pipes - and, last, the output-capture cluster: `for line in
(producer)` to walk a stream, `if myfn` to branch on a function's result, and `$(fn)` to capture a
function's output into a value. That last one closed the set. gsh is complete.

**And it never once reached for a heap.** Every part of it lives on the stack or in a fixed, named
arena: the ~5 KiB variable table, the ~7 KiB script buffer, the 512 B capture buffer, the 16-frame
block stack. Not one general allocator in the whole interpreter. When a working set felt too big for
the stack, the answer was never "add a heap" - it was "change the representation": stream a file in
fixed chunks, refer to data by `(offset, len)` spans, give a subsystem its own bounded arena, iterate
with an explicit stack instead of native recursion (§26.6.1). The constraint did not cramp the design;
it improved it. Every limit in gsh is a number you can read off the source.

**The proof is a 10 MB script.** A shell that malloc'd its way through source would choke on it or take
the machine down with it. gsh reads ~7 KiB, truncates the rest *loudly*, runs the tour it can hold -
functions, captures, error paths and all - and leaves the prompt answering. Overflow is not a crash
here; it is a feature. Every bound fails the same honest way: a value too big for the 512 B capture, a
script too big for the 7 KiB buffer, a statement too long to hold across a read - each is a loud,
specific refusal, never a silent truncation and never a corrupted result. The system tells you the
truth about its limits.

**The last feature nearly went the other way, and the honesty of the ask is the whole point.** Capturing
a function's output looked like it wanted a big buffer, and the question came out loud: *"is this a cool
place to use a heap - throw the function on a heap, get the result, throw the heap away?"* That is the
reflex, caught in mid-air - because "scratch space, filled, then discarded" is not a heap, it is a
bounded stack buffer, which is exactly what we used (512 bytes - it lives in the executor's own frame,
so it must stay small enough to coexist with everything else on a 256 KiB stack; a captured value is a
name, a number, a short line). The heap is a *shape of thought*, and the shape can almost always be had
without the allocator. gsh is that argument, made in code, half a kilobyte at a time.

---

## The Days I Was Wrong

The entries above are mostly victories - the days understanding clicked into place. But the days
worth keeping most are the ones where the architecture had to beat the author. A constitution that
only ever agreed with my instinct would be a mirror, not a law. These are the days it told me no,
and was right to. They are the proof that the rules bind the founder first.

### ~2026-06-21 - The day the constitution rejected its author

While hardening Maximum Carnage, I noticed an apparent contradiction: Chaos exempted itself while
running. Earlier, the shell had been exempt from Chaos so that I would always have a way back into the
system, and that exemption had later proved to be hiding bugs. My first instinct was simple: if no
service is special, then perhaps Chaos itself belonged in the kernel, so it could destroy every
service, including itself.

**What I came to understand:** the apparent contradiction came from confusing *operation* with
*verification*. Chaos is fundamental to verifying the system, but it is not fundamental to operating
it. A test harness cannot terminate itself while coordinating the test, any more than a scheduler can
deschedule itself out of existence. Chaos remaining alive during a run is not privilege; it is the
logical consequence of the role it performs. The kernel already provides the escape path through the
serial console, so Chaos needs no special authority beyond the capabilities it already holds.

Moving Chaos into the kernel would have violated Commandment I: the kernel's responsibility is to
*operate* the system; Chaos exists to *challenge* it.

**What it produced:** perhaps the strongest proof yet that the Commandments are not written only for
contributors - they constrained the project's own author. The idea was rejected, Chaos remained a
userspace service (§4.4, §26.10), and the constitution proved that it governs principles, not people.

### ~2026-06-27 - The day I reached for a heap

A working set felt too big for the stack - an editor meant to open files of any size - and the
reflex was instant: add an allocator.

**What I came to understand:** "too big for the stack" is almost never a reason to add a heap. It is
a reason to change the *representation* until the working set is small - a piece table, a streaming
window, spans instead of copies. A heap would have hidden the bound; the constraint forced me to
find the right data shape instead, and the right shape was simpler than the allocator would have been.

**What it produced:** §26.6.1 (bounded memory means stack and arenas, not heap) and the habit of
reading a hard ceiling reached *loudly* as a feature - a prompt to rethink the working set - not as a
missing allocator.

### 2026-06-28 - The day my own test lied to me

A chaos flood-storm passed, and I believed it. Then I looked closer: it had been counting a full
queue as "drained," so it had been passing a target that was actually broken. I had overstated a fix
on the strength of a test that could not fail.

**What I came to understand:** a test that passes a known-broken target is not a clean test, it is a
weak one - and a green check from a weak test is worse than no test, because it sells false
confidence. Fix the verdict before you trust the result.

**What it produced:** the verification was rebuilt to demand that a re-sent message actually *land*,
which immediately exposed three real clogs the old test had been hiding. Trust a system only as far
as you trust the test that watched it.

---

## The Named Bugs - the teachers

Some bugs are worth naming, because a name turns a failure into shorthand. Years from now someone
will say *"do not repeat the Registry Illusion,"* and everyone in the room will know exactly what
that means. These are not listed because they were bugs. They are here because they were teachers.

- **The Registry Illusion** (2026-06-21) - an entire service existed to *store* a name-to-cap mapping
  that turned out to be almost entirely derivable: the supervisor already held the caps, the kernel
  needed only a tiny recovery anchor. *Taught:* most of what looks like irreducible truth is a derived
  view dressed up as a source. (Commandment III.)
- **The Sleeper on Core Zero** (~2026-05-30) - on the Dell Wyse, a service blocked on `recv` on the
  bootstrap core was never woken: core 0 sat with interrupts disabled, and the cross-core wake IPI had
  nowhere to land. *Taught:* a cross-core wake is only as good as the target core's willingness to be
  interrupted. The first appearance of the IF=0 family.
- **The IF=0 Respawn Wedge** (2026-06-23) - the kernel respawned the supervisor from inside an
  interrupts-disabled timer ISR, so it could not acknowledge the TLB-shootdown IPIs it depended on, and
  the whole machine froze. *Taught:* a recovery path that runs where it cannot be interrupted cannot
  recover; privileged work belongs where interrupts are enabled.
- **The Global TLB ACK Wedge** (2026-06-24) - at 71,000 carnage rounds, two cores each waited on the
  other to acknowledge a global TLB shootdown, and both stopped. *Taught:* a global barrier under true
  concurrency is a deadlock waiting for the one interleaving that triggers it; make it per-core.
- **The DMA After Death** (2026-06-28) - a driver was killed and its memory reclaimed, but its device's
  DMA engine kept writing into the freed-and-reused frame, and eventually scribbled over a kernel page
  table. *Taught:* a device's DMA outlives the driver that aimed it; quiesce the hardware before
  reclaiming its memory, and reserve the arena so a stray write can never reach a page table.
- **The Mortal Shell** (2026-06-21) - the shell, the user's own interface, was the last thing that
  stayed dead if you killed it. *Taught:* nothing escapes restartability, not even the face of the
  system. (Invariant 6; identity over location applies to the shell too.)
- **The Test That Lied** (2026-06-28) - a flood-storm counted a full queue as "drained" and passed a
  target that was broken. *Taught:* a green check from a test that cannot fail is false confidence; fix
  the verdict first. (The bug behind "the day my own test lied to me," above.)
- **The Four-Thousand-Year Uptime** (2026-06-28) - uptime briefly reported ~4,987 days because the
  clock was derived from a momentarily-glitched source. *Taught:* a derived view is only as honest as
  its source; deglitch at the source, never paper over the symptom.

*Add to this list as the project earns new names. A bug that taught something deserves to be
remembered by name.*

---

*Compiled once, looking back - a record of what was understood, and the rules that understanding
produced.*

*Godspeed.*
