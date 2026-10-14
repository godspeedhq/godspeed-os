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

## Code defects (recorded by the audit; fixed the same day, in the commit after it, at the operator's word)

1. **`power` re-announced a shutdown as it counted down.** `Shutdown::refresh` read any fall of two seconds or more
   as "new or brought earlier". **Fixed:** a deadline is new only on a cue from the shell or at `power`'s start
   (`Shutdown::changed`); a countdown step only follows it. `osdev test shutdown`: a 70 s shutdown warns at 1m and is
   announced once.
2. **A wrong first warning after a deadline was set** ("15m" for a `shutdown 10m`). **Fixed:** a new deadline marks
   every threshold at or above it as passed (`passed_threshold`). Checked in the same 70 s case.
3. **The tidy-up could be cut** (two steps of up to a second each, started at two seconds left, rounded up).
   **Fixed:** it starts three seconds out (`TIDY_AT = 3`), and while a shutdown is pending `power` re-reads the
   deadline at least once a minute, so one brought earlier without a cue is still seen in time.
4. **A cancel in the last seconds was silent, and `fs` went on refusing writes.** **Fixed:** `fs` takes `[op, 0, 1]`
   to end a quiesce (`gs::fs::Fs::resume`), and `power` sends it when a shutdown is cancelled after its tidy-up. Found
   while fixing it: `power` ran its countdown step BEFORE reading the cancel's cue, and took the vanished deadline for
   one falling due; the cue is now read first. `osdev test shutdown`: a write goes straight through after such a cancel.
5. **`shutdown now`'s lines overclaimed.** **Fixed:** it says a copy in flight leaves a partial file, and it reports
   "does not hold POWER_OFF" only for that refusal.
6. **`shutdown 0` was refused as "at most 24h".** **Fixed:** "a delay is at least 1s - or `shutdown now`".
7. **`acpi.rs` blamed `\_PTS` whenever S5 did not take, and read unknown `\_S5` encodings as values.** **Fixed:** an
   SCI_EN that never set is said as that, `\_PTS` is "most likely" rather than certain, and any encoding but
   `BytePrefix`, `Zero` and `One` is refused rather than guessed.
8. **The Pi 4's `hardware_reset` re-armed the watchdog on every loop pass.** **Fixed** as the Pi 2's: poke once, then
   re-arm only at a cadence far longer than the timeout. Not this branch's defect, and the Pi 4's `reboot` has not
   been re-checked on hardware since: it needs a card.

After the fixes: `osdev test shutdown` 35/0, and every regression suite again (in the commit).
