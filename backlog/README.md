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

Severity is about the MODEL, not about noise: "Constitutional" means the code and CLAUDE.md
disagree, which by 26.3 means one of them is wrong and it has to be settled.
