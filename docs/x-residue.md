# Commandment X: what is enforced, and what is left to judgement

**Status:** the honest boundary of `X-user-vocabulary`, written so that "10 of 10 commandments
mechanised" is not read as "Commandment X is enforced".

Commandment X is two sentences, and they are different problems.

> Do not move complexity into the kernel because it is convenient.
> Do not push complexity onto users because it is easier.
> Place it in the layer that naturally owns it.

## The kernel half: enforced, under Commandment I

Nothing new was needed here, and nothing new was added. Four checks already stop complexity moving
into ring 0, and they are filed under I because that is the commandment they were written for:

| check | what it stops |
|-------|---------------|
| `I-responsibilities` | a kernel module that claims none of the six responsibilities (§4.3) |
| `I-arch-drivers` | a peripheral device driver living in the kernel |
| `I-syscalls` | a syscall admitted to the surface without deliberation |
| `I-kernel-deps` | a crate linked into ring 0 without deliberation |

Plus `I-kernel-spawns`, `I-service-table`, `I-features` and `I-introspect` on the same axis. If X's
first sentence is violated, one of those fires.

## The user half: enforced as of 2026-09-14, narrowly

`X-user-vocabulary` reconciles the verbs the shell answers against the specs in `utilities/`, whose
own `0_conventions.md` declares itself canonical for *every* utility. Two directions, plus an
inversion:

- a **spec with no command** teaches a verb that does not exist. The user types it, gets `unknown:`,
  and learns the documentation cannot be trusted - which costs more than the missing feature.
- a **command with no spec** can only be found by reading the source. That is §26.11 failing at the
  one surface a user actually touches.
- a spec may **document an absence** (`utilities/14_poweroff.md` opens "not provided (considered and
  rejected)" so nobody re-attempts it). For those the rule flips: a command appearing makes the doc
  a lie, and that fires instead.

It found seven verbs with no spec, including `selfcheck` - the most-used verb in the project, the one
every hardware run reports through. They are recorded in `contract_authority_debt`'s neighbour
`utility_vocab_debt` and may only shrink.

## The residue: not mechanised, and not mechanisable by this route

**"Place it in the layer that naturally owns it."** This is the sentence the commandment turns on and
no checker reads it. Deciding that terminal emulation belongs in a `console` service rather than the
kernel, that bus enumeration belongs in `hw-enumerator` rather than the PCI code that discovers it, or
that a device's *class* is the kernel's fact while its *meaning* is the driver's - each of those was
an argument, made once, recorded in an amendment. A pattern cannot make it.

What a checker CAN do is notice afterwards that the argument was never made: a new kernel module, a
new syscall, a new ring-0 dependency, a verb with no documentation. That is what the eight checks
above do. They are a tripwire on the consequences, not a judge of the decision.

**Three things are therefore still unguarded, and are listed so they are not mistaken for covered:**

1. **A module that grows.** Every I-check pins a SET - modules, syscalls, dependencies, services. None
   counts lines. Two thousand lines can be added to an existing kernel module and the whole layer
   stays green. A size ratchet would catch it; it would also be crude, since a line count says
   nothing about placement.
2. **Complexity pushed to a service that does not own it.** The checks know which services exist and
   what they may reach (`VII-service-grants`), not whether the work inside one belongs there.
3. **A user-facing surface that is documented and still bad.** `X-user-vocabulary` proves a verb has a
   spec. It cannot read the spec and tell you the verb is confusing, inconsistent with its siblings,
   or asks the user to know something the system already knows. `utilities/0_conventions.md` rules 4
   and 7 (words not flags; report facts, do not editorialize) are exactly that judgement, and they
   stay a review matter.

## Why this file exists rather than a tenth check

Inventing a check so the scoreboard reads 10 of 10 would be a number optimised instead of a property
secured, which is the failure §26.3 and `commandments_redteam.py` exist to prevent. The count is now
10 of 10 because a real gap was closed - the user-facing vocabulary was reconciled against nothing
before - and this file records precisely how much of X that does and does not buy.
