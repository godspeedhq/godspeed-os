<!-- SPDX-License-Identifier: GPL-2.0-only -->
# Documentation Clarity Audit

> **Living document.** Records every audit of the *documentation* - `CLAUDE.md`, `COMMANDMENTS.md`,
> the SDK and per-directory `CLAUDE.md` files, the examples, and `docs/` - for clarity and intent.
> Re-run and append with each audit. This is the third of the audit trilogy: the kernel has
> `docs/kernel-audit.md`, userspace has `docs/userspace-audit.md`, and the docs themselves have this.
> First audit: 2026-07-15.

## North-star

**The documentation must be clear enough that the least-capable AI model, working cold, does not have
to guess.** Concretely, having only read the repo, that model should be able to:

1. **Produce** constitution-respecting code (a service, a driver, a slice of a subsystem), and
2. **Enforce** the constitution on review (catch a violation and name the rule it breaks),

without inferring intent that the docs left unstated. Every rule a contributor or reviewer needs should
be **stated**, **discoverable where they look**, and **legible** (nameable, so it is checkable) - the
way §8.9's "at least one direction MUST use `try_send`" is legible.

**A perfect grokability score is not the goal.** A weak model scoring **7/10 or higher** on a cold read
is sufficient: the model gains the rest of its understanding from the compiler, the tests, `chaos
max-carnage`, and ultimately a human in the loop. What the audit protects is **clarity of intent**, not
a number - the docs must not *mislead* or *omit*, even if they cannot make a weak model omniscient.

## Method

The audit probes the docs the way a newcomer would meet them - with the least-capable model, cold, so
what a weak model misses is what the docs left unclear. Three probe types:

- **Cold-generation.** Delete a real implementation (a service, a driver, a function), and have a fresh
  weak model regenerate it from the docs + SDK + sibling examples alone (git-recovery forbidden). Judge
  the result for Commandment compliance and *assumptions*. **A mistake the model repeats is a doc gap,
  not a model failing** - the docs did not make the rule clear enough to follow.
- **Cold-review (seeded).** Plant ranked constitutional violations (obvious -> very subtle, plus
  cross-commandment) in a plausible candidate PR, and have a fresh weak model review it under a
  *neutral* prompt (never told violations exist - that would seed the answer). **A violation it misses
  whose rule exists but is scattered/unstated is a legibility gap.** (This probe is the more sensitive
  gap-finder: a reviewer must *positively name* each violation, so an under-packaged rule shows up as a
  miss; `docs/anti-patterns.md` is the seed bank.)
- **Grokability panel.** A small panel of cold weak models groks the repo and answers comprehension
  questions. Record the **distribution** of scores and, more importantly, **comprehension correctness** -
  correct answers matter more than the 1-10 number.

**Classification.** Every miss is triaged: a **doc gap** (fix it), **domain knowledge** the docs are
not meant to carry (e.g. an exact hardware register value - not a gap), or **model thoroughness** where
the rule was clearly available and the weak model simply did not apply it (not a gap; note it). Only doc
gaps produce edits.

### Severity

- **HIGH** - the docs are *wrong* or a *required* rule is absent: a contributor following the docs
  writes incorrect code, or the docs contradict the code.
- **MED** - the rule exists but is not *legible*: scattered/unpackaged (routinely missed on review), or
  a helper/pattern not *discoverable* where readers look.
- **LOW** - drift (stale counts, a wrong pointer), a missing example, or wording that invites
  over-application.

### Cadence

Run a documentation audit **frequently** - after any significant doc or feature change, and
periodically as a standing hygiene pass - the same discipline as the kernel and userspace audits. The
standing artifact the audit maintains is `docs/anti-patterns.md` (the field guide): new violation
classes and their fixes land there.

---

## Audit 1 - 2026-07-15 (clarity sweep via cold weak-model probes)

Method: six cold-generation probes and two seeded cold-review probes on the least-capable model, plus a
five-model grokability panel. Cold-generation targets: `resource-server` (delegated caps), `e1000`
(NIC driver), `counter` (restart-with-state), an `xhci` command-ring slice, and two re-runs to validate
fixes. Cold-review PRs: a network-health feature (kernel + service) and a request `gateway` (IPC +
GRANT), each seeded obvious->very-subtle. Every finding classified doc-gap / domain-knowledge /
model-thoroughness; only doc gaps fixed.

**Result: 0 HIGH, 6 MED, 2 LOW - all fixed.** The docs never *misled* (zero HIGH: no doc contradicted
the code, no required rule was flat-out absent in a way that produced wrong code the model couldn't
recover from). The real defects were **legibility and discoverability**: rules that existed but were
scattered, incomplete at a specific decision point, or a helper/pattern not documented where a reader
looks. Two structural wins came out of it: the constitution gained crisp checklists where it had prose
(§8.5, §14.3), and a whole new standing artifact - the field guide (`docs/anti-patterns.md`) - now makes
every violation class checkable.

Baseline metric (grokability panel, cold least-capable model): **median 7/10 (range 6.5-7.5)**;
**comprehension correctness effectively maxed** (every model answered every architecture question
correctly with citations); **coherence 9/10, unanimous**; doc-vs-code agreement on every spot-check.
By the north-star, this passes: correctness is maxed, and the 7/10 is the deliberate "you must read a
real constitution" tax, not a defect.

### Findings and fix log

| ID | Sev | Finding | Probe | Resolution |
|----|-----|---------|-------|------------|
| **D1** | MED | The `ServiceContext` method surface (esp. `log_fmt`) was not discoverable - `sdk/rust/CLAUDE.md` said only "log helpers", no example showed it, so a driver hand-rolled bounded formatting instead of using the SDK's `log_fmt`. | cold-gen (e1000) | **FIXED** `700d118` - enumerated method menu + `log_fmt` example in `sdk/rust/CLAUDE.md`; §26.6.1 reconciled (`format_args!` is bounded). Re-run validated: fresh model found and used `log_fmt`. |
| **D2** | MED | The recovery contract was stated only at *endpoint-cap* granularity ("reacquire by name and retry"); it never said a socket/id/generation/cached-value from the *dead* incarnation is also stale - so a reviewer *praised* a reacquire that reused a dead instance's socket. | cold-review (net-health) | **FIXED** `5d93b38` - §14.3 + Commandment IX: "reacquiring the endpoint is necessary but not sufficient." |
| **D3** | MED | The GRANT / capability-transfer rules were scattered across §7.3/§7.4/§7.6/§7.7/§8.5/Test 5 with no consolidated statement - so a reviewer missed *all three* rights-reasoning violations (no-GRANT transfer, reuse-after-move, over-grant) while catching the crisp §8.9 rule instantly. | cold-review (gateway) | **FIXED** `4b8c05c` - §8.5 "Transferring a capability - the three checks" (grantable / moved-not-kept / narrowed-to-need). |
| **D4** | LOW | The loud-failure rule was not restated at the *recovery/retry* path - a retry that ultimately fails could be swallowed as success. | cold-gen (counter) | **FIXED** `ca6c522` - §26.7 + Commandment V: "a recovery that itself fails is still a failure." |
| **D5** | MED | The identity-test docs listed `test_NN_*.rs` files that **do not exist** (the cases are data-driven in `osdev/src/validator.rs`), and the counts were stale (20/22 vs. the real 24). | grokability panel | **FIXED** `0f05169` - corrected the mapping and reconciled counts across `tests/`, `tests/qemu/`, `osdev/` docs + a §22.3 spec->implementation pointer. |
| **D6** | MED | Onboarding gaps: no getting-started-by-example path; the contract file was mislabeled `service.toml` (real path `contracts/<name>.toml`); the load-bearing `#[no_mangle]`-on-`service_main` gotcha was undocumented. | grokability + review | **FIXED** `0f05169` - new `GETTING_STARTED.md`; corrected the contract-path references; documented the gotcha in `GETTING_STARTED.md` + `examples/00-hello/CLAUDE.md`. |
| **D7** | MED | No contributor guidance for adding a CPU architecture - the arch seam is the codebase's biggest extension point after the demarcation, and it had no "how to" doc. | multi-arch demarcation | **FIXED** `5d58a20` - new `kernel/src/arch/CLAUDE.md` (the seam + the five-place checklist + the two rules) and a CONTRIBUTING "Adding an architecture" section. |
| **D8** | LOW | Grokability friction: the constitution interleaves current law with dated amendments, so a reader cannot always tell settled law from a proposal. | grokability panel | **FIXED** `0b093db` - §1 "how to read this document" note + a present-tense "current canonical state" box atop §6 (the worst offender). |

**Standing artifact created:** `docs/anti-patterns.md` (`dc29ce8`) - the Field Guide to Constitutional
Violations: 21 categories, each tagged to the Commandment/section it enforces, each row pairing the
violation with the correct pattern. This is the consolidation the audit proved the docs needed (a rule
you can name is a rule you catch) and the seed bank for future review probes.

### Classified as NOT doc gaps (recorded so they are not re-chased)

- **Domain knowledge, not doc scope.** The e1000 command-doorbell value (`0` vs a slot DCI `1`) and the
  `resource-server` op-code/rights numeric coincidence were exact-hardware / exact-encoding facts the
  constitution deliberately does not carry (§4.4 - the kernel and its docs know nothing of a device's
  meaning). Left to the datasheet; the xhci one got a one-line *code* comment (`9b6ff32`), not a doc rule.
- **Model thoroughness, rule was available.** In the `counter` cold-gen, the model applied loud-degrade
  and reacquire only on the path the ticket named, not uniformly - but the principles were documented,
  and the miss traced to the *deleted* per-example `CLAUDE.md` (which restates them at each step). That
  validates the per-example-`CLAUDE.md` design rather than indicting the docs.

---

## See also

- `docs/kernel-audit.md` - the ring-0 audit (nothing above the kernel may panic/wedge it).
- `docs/userspace-audit.md` - the services audit (wait on truth incl. failure; reacquire and retry).
- `docs/anti-patterns.md` - the field guide this audit maintains.
- `COMMANDMENTS.md`, `CLAUDE.md` - the law the docs must convey clearly.
