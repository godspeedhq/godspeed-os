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

An item leaves by being **closed in place**: the file stays, its status line says CLOSED and the
date, and the post-mortem goes at the end. It does not leave by going quiet, and it is not deleted.

This used to read "delete the file, say so in the commit", and the practice had already gone the
other way - eleven entries were sitting closed and kept before anyone noticed the rule said
otherwise. The practice is the right one and the rule is corrected to match, because **the
post-mortem is usually worth more than the item was.** Item 37 took four measurements, each
retracting the last, before the answer turned out to be three lines nobody had read; deleting that
file would have destroyed the only record of how four rounds of instrumenting missed it. A closed
item is evidence (`audits/`), and evidence is kept.

The cost of keeping them is that "what is still open" stops being obvious once a dozen closed files
sit in the same folder. So a status line is **mandatory and goes at the top**, in the first lines of
the file, beginning `**Status:` and carrying one of CLOSED / OPEN. Two hand surveys of this folder
disagreed with each other before that was written down: one read only the first four lines and
missed status lines on line 6, and one read by eye and put three closed items in the open pile.

## One place, and what that means precisely

**Every entry carries a status line, and that is enforced.** `**Status:` within the first 12
lines, carrying CLOSED / OPEN / RESOLVED / FIXED in capitals, checked by
`scripts/backlog_check.py` - which also refuses an entry that no row above links to. Coverage
reached 38 of 38 on 2026-09-20 and the check became a gate the same day, so it can only be held.

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
| [1](01-placement-invalid-never-enforced.md) | `PlacementInvalid` IS constructed now - but only for an operator's `--core N`. A CONTRACT's `placement.core` is still rerouted, and 9.2/13.2 still say it must not be. The reroute is at least LOUD as of 2026-09-20 | **Constitutional** | single-core work, 9.2 |
| [2](02-single-core-support.md) | Single core: ANSWERED (Pi 4 and T630 both pass) - open remainder is USB on the T630 | Answered / open tail | - |
| [3](03-pi4-shell-stack-smash.md) | Pi 4 shell faults with a return address of ASCII spaces | Correctness | - |
| [4](04-serial-splice.md) | The kernel splices one log line into another under load | Observability | evidence quality |
| [5](05-pi2-clock-floor-never-persists.md) | Pi 2 never writes `/clock.last`, so every boot starts at 1970 | Correctness | - |
| [6](06-kernel-ring-not-drainable.md) | No syscall exposes the kernel's 16 KiB log ring to userspace | Feature | `events log` completeness |
| [7](07-events-remote-sink.md) | `events persist start <url>` - ship a capture off-box | Feature | - |
| [8](08-d3-assignment-vs-reenumeration.md) | D3: the assignment/re-enumeration split, and "cost 2" | Design decision | the D3 gate |
| [9](09-constrained-targets-and-sizing.md) | Constrained targets: boot-size the arenas (~22 MiB of .bss), and what really blocks a microcontroller | Design question | any small-memory port |
| [12](12-xhci-probe-blocks-input.md) | xHCI hub probes block the input loop - typing lags on one core | Latency | - |
| [11](11-ehci-bios-handoff.md) | `ehci` resets a BIOS-owned controller with no USBLEGSUP handoff - fatal on one core - **CLOSED 2026-09-21**: executed on the T630, firmware REFUSED to release and ownership was forced, keyboard still works | Closed | - |
| [10](10-ipc-efficiency.md) | IPC cost: fewer ROUND TRIPS, not a tighter protocol - batching, co-location, and the fixed 4 KiB message | Performance | the hot paths |
| [13](13-ehci-holds-core-when-unplugged.md) | EHCI holds a core while a device is unplugged; xHCI `Enable Slot` timeouts | Cosmetic-to-minor | - |
| [14](14-riscv64-port.md) | The RISC-V 64 port (StarFive VisionFive 2 Lite) - **shipped in v0.16.0**; open tails only | Feature / shipped | - |
| [15](15-nic-rx-coverage.md) | The NIC receive ring is only drained when somebody asks | Recorded (§26.7) | - |
| [16](16-riscv64-chaos-liveness-wedge.md) | riscv64 chaos: core 1 takes interrupts and never switches away - **CLOSED 2026-09-11** | Closed | - |
| [17](17-riscv64-port-shared-surface.md) | What the riscv64 port changed OUTSIDE riscv64, and what still needs testing elsewhere | Cross-port | - |
| [18](18-unsafe-audit-misses-the-sdk.md) | The unsafe audit does not cover the SDK - **CLOSED**; `sdk/` is scanned and its two floors frozen. Open tail: a safe `raw_syscall` wrapper would collapse ~86 of the 90 | Closed / open tail | - |
| [19](19-networking-does-not-recover-from-a-chaos-storm.md) | Networking does not recover from a chaos storm (Wyse / RTL8168) - fixed, kept open on evidence | Fixed / open on evidence | - |
| [20](20-audit-followups-code-and-config.md) | What the 2026-09-12 doc audit found in CODE and CONFIG - **all six FIXED**; item 6 awaits a VisionFive boot | Fixed / one on hardware | - |
| [21](21-nic-backend-chosen-by-isa.md) | `nic-driver` picks its MAC by instruction set on 3 of 4 boards (x86 asks the device); and NET_DEVICE syscalls 42-44 now have no userspace caller | Recorded (26.7) | - |
| [22](22-pi4-display-blanked-while-the-system-stayed-up.md) | Pi 4 display went blank during `selfcheck` while the shell kept answering typed commands - cause NOT established, discriminator recorded | Open / 1 occurrence | - |
| [23](23-recorder-crashed-and-selfcheck-did-not-notice.md) | `recorder` branched to address 0 mid-suite, and four assertions passed while it was dead - the ASSERTIONS are **fixed** (a liveness check now guards them); the crash is open on 1 occurrence | Half fixed / 1 occurrence | - |
| [24](24-adversarial-faults-run-on-one-port.md) | The 22 A14/C2 ring-3 fault tests run only on x86-64 under QEMU; the arm and aarch64 fault primitives exist but no build reaches them, and riscv64 has none | Recorded (26.7) | - |
| [25](25-pci-selector-layout-is-packed-above-the-kernel.md) | `hw-enumerator` packs a host-bridge config selector that each arch then unpacks - addressing is mechanism and belongs in `arch/` | Recorded (26.7) | - |
| [26](26-visionfive-uboot-cannot-load-large-files.md) | VisionFive would not boot: `extlinux.conf` was CRLF, so U-Boot read the trailing CR as part of every FILENAME - **CLOSED**, and now enforced by `line_ending_check.py` | Closed | - |
| [27](27-silent-clock-fallback.md) | `duration_cycles` floors to one quantum on an uncalibrated counter, so a bounded wait silently becomes a spin | Recorded (26.7) | - |
| [28](28-listener-release-is-the-client-s-job.md) | A listener's port is released by the CLIENT and sometimes is not; and the in-loop dance blocks `net-stack` - **MEASURED**: 1.5 to 4 s configured, 22 to 79 s with no link | Recorded (26.7) | - |
| [29](29-the-wyse-tcp-big-that-never-left-the-shell.md) | A `tcp` that took ~20 s to start: three defects, each hiding the one behind it - **CLOSED**, hardware-verified on five boards | Closed | - |
| [30](30-chaos-flood-storm-xhci-flakes-under-host-load.md) | `chaos: flood-storm xhci` fails intermittently in `osdev test shell` and passes on a re-run - dismissed as host load twice, so recorded | Open / not diagnosed | - |
| [31](31-net-stack-blocked-48s-on-a-live-nic-driver.md) | `net-stack` blocked 48 s on a `nic-driver` that was ALIVE - **cause CONFIRMED**: the reply stream runs ~28 requests behind. A correlation tag proved it and was reverted (refusing is not recovering) | Open / cause known | `backlog/28` |
| [32](32-the-fs-suites-run-in-no-gate.md) | The eleven `fs` suites run in NO pre-merge gate, and two were red on the v0.18.0 commit - both stale assertions, both now fixed; the gap that let them rot is open | Open / process | - |
| [33](33-a-directory-listing-stops-at-one-block-and-says-nothing.md) | A directory listing stopped at one block and reported the truncation as a complete answer - **CLOSED**, the listing RESUMES | Closed | - |
| [34](34-no-non-x86-port-can-reach-a-disk-in-qemu.md) | No non-x86 port can reach a disk in QEMU, so the whole storage stack is testable on one architecture | Recorded (26.7) | fs coverage off x86 |
| [35](35-six-copies-of-the-service-list-and-no-two-agree.md) | Six copies of "the services" and no two agree - the shell's three are **established** now (5 differences deliberate, `time` was drift and is fixed); `dwc2` and `control` are the open decisions | Open / narrowed | `backlog/21`, `backlog/25` |
| [36](36-selfcheck-second-run-races-events-persist-status.md) | `selfcheck`'s second run raced the capture's pre-fill - **CLOSED**, and its originally stated cause was WRONG; it waits on the state now, not a clock | Closed | - |
| [37](37-console-took-over-two-seconds-to-answer-a-scroll.md) | The console took over 2 s to answer a scroll - **CLOSED**: it repainted a 4K framebuffer inside the caller's deadline. Four measurements missed it; the mechanism is deleted | Closed | - |
| [39](39-a-reset-that-is-a-spin-loop-on-two-more-arch-paths.md) | `hardware_reset` is `loop { spin_loop() }` on a non-Pi4 aarch64 build and still prints that it reset - the riscv64 defect fixed 2026-09-21, one feature flag away on a port that DOES run userspace | Recorded (26.7) | a second aarch64 target |
| [40](40-selfcheck-is-out-of-room.md) | `selfcheck.gsh` is 376 bytes from a HARD 64 KiB ceiling (u16 prescan offsets; over it the interpreter dispatches the wrong function body, silently). Job control landed a 330-byte hardware check instead of the 960-byte one that reads a job's effect back | Recorded (26.7) | widening the offsets to u32 |
| [41](41-a-throwaway-shell-for-the-commands-that-cannot-detach.md) | a throwaway shell would let `selfcheck` and `run` detach - REJECTED on security: a service that runs any command on request is a confused deputy holding every cap the shell has, and its authority becomes exercisable over IPC rather than by whoever is at the console | **Rejected** (do not re-attempt) | - |
| [42](42-the-pi2-stick-accepted-the-flush-6-1-says-it-refuses.md) | the Pi 2's stick ACCEPTED `SYNCHRONIZE CACHE` on 2026-09-22 and recovered a deterministic cut in the strong form, while `CLAUDE.md` 6.1 records it as refusing. The only local evidence for the refusal is a warning that fired when the DRIVER was dead, not the device - now fixed to tell those apart | **ANSWERED - 6.1 amended 2026-09-23** | an unassisted cut hit the commit window and the journal REPLAYED |
| [43](43-fs-all-is-not-reproducible-on-a-loaded-host.md) | two full `fs-all` sweeps both returned 31 of 33 and failed DIFFERENT suites; every failure passes when re-run alone. Mechanism measured rather than assumed: one write took 200 ms with 99% inside block ops, driver ops at 6-17 ms, 1000+ flagged slow. A gate that only passes on an idle machine is not a gate | Recorded (26.7) | separate STARVED from FAILED, then audit which assertions are genuinely time-based |
| [44](44-commandment-viii-has-no-mechanised-check.md) | VIII (`wait for truth, not time`) is the ONLY commandment with no mechanised check - the checker covers the other nine. Noticed because `backlog/43`, opened the same day, is a VIII problem in its own words: suites asserting on elapsed time rather than on the work completing | Recorded (26.7) | nothing until wanted; the entry lists what might actually be catchable |
| [38](38-tab-completion-of-a-file-path-times-out-about-one-run-in-three.md) | Tab completion of a file path times out ~1 run in 3 and drags the next case down with it - bisected far enough to EXONERATE the console; not root-caused | Open / measured | `osdev test files` reliability |

Severity is about the MODEL, not about noise: "Constitutional" means the code and CLAUDE.md
disagree, which by 26.3 means one of them is wrong and it has to be settled.
