# backlog/

Open items that are **recorded rather than closed** (CLAUDE.md 26.7), one file each.

A limitation that cannot be closed today is written down, not left implied. Until this folder
existed those records were scattered across `docs/`, service `CLAUDE.md` files and commit
messages, which meets the letter of 26.7 and fails its point: a record nobody can find is not a
record. This is the index.

## What belongs here

An item earns a file when it is **real, reproducible or evidenced, and not being fixed right now**.
Three things every entry must carry, because their absence is what makes a stale backlog:

- **Evidence** - a log line, a compiler warning, a measurement. Not a suspicion.
- **What is RULED OUT** - so the next attempt does not re-derive the dead ends.
- **The next concrete step** - what would actually move it, and what it costs.

An item leaves by being fixed (delete the file, say so in the commit) or by being decided against
(keep the file, record the decision and why). It does not leave by going quiet.

## One place, and what that means precisely

**Status lives here and nowhere else.** Whether a thing is open, what rules it out, and what the
next step is - that is this folder's, exclusively. Before this existed the same item could be
"recorded" in a service doc, a design note and a commit message, and those three would drift; the
per-core log design sat in `services/events/CLAUDE.md` where nobody looking for open work would
find it, and this folder's item 4 was written without it.

**Design narrative stays where it is written**, and is LINKED, not copied. `docs/service-ownership.md`
owns the D3 reasoning; item 8 owns the fact that D3 is blocked on a decision. A link is a reference,
not a second copy - what must never exist twice is the claim about STATE.

The split is the same one the project already uses: CLAUDE.md is law, `docs/` is reasoning,
`audits/` is evidence. This is open work.

**A constitutional limitation is NOT a backlog item.** The ARM DMA posture and the
backend-conditional crash-recovery guarantee are recorded in CLAUDE.md because they are the current
law of the system, not work waiting to be done. Moving those here would break the rule in the other
direction.

## Current items

| # | Item | Severity | Blocks |
|---|------|----------|--------|
| [1](01-placement-invalid-never-enforced.md) | `PlacementInvalid` is never constructed - contracted core silently ignored | **Constitutional** | single-core work, 9.2 |
| [2](02-single-core-support.md) | GodspeedOS on ONE core: what actually breaks | Feature + audit | - |
| [3](03-pi4-shell-stack-smash.md) | Pi 4 shell faults with a return address of ASCII spaces | Correctness | - |
| [4](04-serial-splice.md) | The kernel splices one log line into another under load | Observability | evidence quality |
| [5](05-pi2-clock-floor-never-persists.md) | Pi 2 never writes `/clock.last`, so every boot starts at 1970 | Correctness | - |
| [6](06-kernel-ring-not-drainable.md) | No syscall exposes the kernel's 16 KiB log ring to userspace | Feature | `events log` completeness |
| [7](07-events-remote-sink.md) | `events persist start <url>` - ship a capture off-box | Feature | - |
| [8](08-d3-assignment-vs-reenumeration.md) | D3: the assignment/re-enumeration split, and "cost 2" | Design decision | the D3 gate |
| [9](09-constrained-targets-and-sizing.md) | Constrained targets: boot-size the arenas (~22 MiB of .bss), and what really blocks a microcontroller | Design question | any small-memory port |

Severity is about the MODEL, not about noise: "Constitutional" means the code and CLAUDE.md
disagree, which by 26.3 means one of them is wrong and it has to be settled.
