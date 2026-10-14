# Docs and comments audit, feat/shutdown (2026-10-14)

**Scope.** Everything `feat/shutdown` wrote or made stale, at `f73fdc16`: the design note and the command spec, the
CLAUDE.md amendments (12.3, 18.5, Appendix B.4), the indexes and service descriptions, the Commandments baseline's
comments, and the code comments in the kernel (the `PowerOff` path on every port, the deadline, query 28), the SDK,
the stdlib, `power`, the shell, `fs`, the supervisor, the probe and the test harness. Three read-only passes, every
finding re-read against the code before it was changed. Docs and comments only: code defects are listed at the end,
not fixed here.

**Result.** 34 defects in documents and comments, corrected in the same change; 8 code defects recorded below.

## Documents (corrected)

| Where | Was | Now |
|---|---|---|
| `docs/shutdown.md` 2, `docs/CLAUDE.md` | the tidy-up "has a fixed budget, about 1 s" | up to a second for `copier` and up to a second for `fs`, best-effort |
| `docs/shutdown.md` 4.3 | "it starts two out so it finishes before the kernel fires" | best-effort: a slow step can be cut by the deadline |
| `docs/shutdown.md` 4.3 | "No new job" listed as `power`'s step, two seconds out | the shell's refusal, for the whole last minute, said apart |
| `docs/shutdown.md` 3, `utilities/64_shutdown.md` | `shutdown now` "names" the job it will cut | it warns that a running job will be cut |
| `docs/shutdown.md` 4 | the shell "sets, cancels and fires" the deadline | the kernel fires it; the shell sets, cancels, or powers off at once |
| `docs/shutdown.md` 4, `CLAUDE.md` 12.3, the baseline | `power` "holds no authority" / "holds nothing" | no shutdown authority; it keeps `CPU_CLOCK` |
| `docs/shutdown.md` 4.4 | with no disk the `fs` step "is skipped because no `fs` is running" | `fs` runs; it answers that there is no volume, or not in time |
| `docs/shutdown.md` 5, 7, `CLAUDE.md` 12.3 and B.4 | the T630 and the Pi 2 "halt" stated as fact | expected, from the June 2026 attempt and Linux's driver; not yet checked |
| `docs/shutdown.md` 6, `CLAUDE.md` 12.3, the baseline | query 28 is "the one deliberate addition to the ungated list" | ungated, deliberately (15, 20, 21 and 23 were added before it) |
| the baseline, `dispatch.rs` | the deadline "must be seen by ... `ask`" | `ask` does not read it; `power` and `shutdown status` do |
| `CLAUDE.md` 18.5 | "Three amendments to the grandfathered floors stand" | four |
| `CLAUDE.md` 22 A12 | `Reboot` without the capability | `Reboot` or `PowerOff` |
| `kernel/src/syscall/CLAUDE.md` | syscalls "1-55"; the ungated reads omit 28 | 1-56; 28 listed |
| `docs/CLAUDE.md` observability row | 26 live queries, 15 ungated | 27, 16 |
| `docs/shutdown.md`, `CLAUDE.md` 12.3, `docs/power.md`, `website/src/services.md` | "last-second tidy-up" | two seconds before the deadline (`TIDY_AT = 2`) |
| `website/src/services.md` | `power`'s peers "none" | `fs` and `copier`, for the tidy-up |
| `utilities/14_poweroff.md`, `website/src/utilities.md` | `shutdown` "designed, not built" | built, QEMU-verified, hardware pending |

## Code comments (corrected)

| Where | Defect |
|---|---|
| `dispatch.rs`, the ungated-set comment | listed every ungated query but 28, and said every other query is gated |
| `dispatch.rs` `handle_power_off`, `scheduler.rs` `power_off_arm` | "one already due sooner" - a deadline on the same tick is kept too |
| `scheduler.rs` `scan_timed_wakes` | did not say it fires the power-off, from the timer ISR, and does not return |
| `capability/table.rs` | the by-holdings gates omitted `PowerOff` |
| `acpi.rs` | the one `unsafe` named as `table` (it is `region`); `\_S5` parsing claimed only Byte/Zero/One; "the result says" about an SCI_EN timeout it does not report |
| `riscv64/mod.rs` `power_off` | "a refusal is likely and is said": the recorded failure is a board that goes dark, which cannot be said |
| `probe/src/main.rs` A12 | `POWER_OFF` "which only `power` holds" - the shell does |
| `fs` `QUIESCE_SECS` | "the power-off follows within a second" - about two |
| `power` module header, `power.toml` header | described only the clock leases |
| `power` | the SDK note named only `cpu_clock`; "replies lead with a status byte" (`OP_SHUTDOWN_CHANGED` has none); the wait comment named only lease expiry; `TIDY_AT`'s "budget is about a second" |
| supervisor, shell, `power`, test text | "last second" where the tidy-up starts two out |
| `osdev/src/main.rs` | `run_quit_test`'s doc comment was left on `run_shutdown_test`; `power` in the plain build list had no reason |

## Code defects (recorded, not fixed here)

1. **`power` re-announces a shutdown as it counts down.** `Shutdown::refresh` treats any read where the seconds left
   fell by two or more as "new or brought earlier" (`secs + 1 < w`), so each scheduled wake on a long countdown logs
   "the machine will shut down in ..." again and resets the warnings. `osdev test shutdown` passes because it checks
   only that lines are present.
2. **A wrong warning right after a deadline is set.** With `warned` reset, a `tick` before the first scheduled wake
   (a lease request arriving, say) announces the smallest threshold at or above the time left - "15m" for a
   `shutdown 10m`.
3. **The tidy-up can be cut.** It starts when the kernel says two seconds or less remain (rounded up, so between one
   and two), and takes up to a second for `copier` and a second for `fs`. A slow `copier` leaves `fs`'s quiesce to
   be cut. Start it at 3, or share one budget.
4. **A cancel in the last two seconds is silent in `power`**, and `fs` may already be refusing writes for its ten
   seconds; the shell does say "cancelled".
5. **`shutdown now`'s line "fs recovers on the next boot"** overclaims for a cut copy, which leaves a full-size file
   with an undefined tail; and any non-success return is reported as "does not hold POWER_OFF".
6. **`shutdown 0`** is refused as "at most 24h".
7. **`acpi.rs` blames `\_PTS`** whenever S5 does not take, which the kernel does not know: an SCI_EN that never set,
   or a misread `SLP_TYPa`, ends the same way. `Ones` (0xFF) read as 0, and an unknown prefix taken as a value rather
   than refused.
8. **The Pi 4's `hardware_reset` re-arms the watchdog on every loop pass** - the pattern the Pi 2's `hardware_reset`
   records as having stopped `reboot` from ever resetting ("kept petting the dog it was waiting on to bite").
   Pre-existing, not this branch's; whether the Pi 4's `reboot` misbehaves on hardware is not checked here. The new
   Pi 4 `power_off` pokes once.

1 to 7 are this branch's own and stand between it and the hardware cards; 8 is older. Where they are recorded and
fixed is the operator's call (no new backlog entry without asking).
