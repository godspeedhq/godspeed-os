# Shutdown: turning the machine off on purpose

**Status:** design, 2026-10-14, branch `feat/shutdown`. **S1 (the kernel's `PowerOff`) is built and QEMU-verified**
(`osdev test shutdown` 6/0 on x86, SBI SRST on QEMU RISC-V, the refusal in `osdev test adv` A12); S2 to S6 are not
built. The command is `utilities/64_shutdown.md`.

## 1. What is asked for

The operator, 2026-10-14: `shutdown 5` shuts down in five seconds, counting down on screen; `shutdown now` shuts
down at once; a plain `shutdown` waits a default 3 s; a delay may be `5s`, `5m` or `5h`, a long one runs in the
background, and the warnings come closer together as the moment nears. And: "GodspeedOS is built to fail and
recover in its core DNA ... I don't want those checks to take a long time. If I type `shutdown` it should
shut down in much less than 5 seconds."

## 2. The principle: a polite power cut

GodspeedOS does not need a shutdown to keep its disk consistent. `fs` commits every metadata change through a
redo journal and recovers on mount, and every board has recovered from an unassisted power cut (CLAUDE.md 6.1,
amendment 2026-09-23: five of five). So a shutdown is not a rescue. It does three things a power cut does not:

1. **It loses nothing you believe is saved.** File DATA written by `write_at` may still be in the device's cache
   (CLAUDE.md 6.1, 2026-07-25: "unflushed FILE DATA can be lost on a power cut"). One flush closes that.
2. **It leaves nothing half-made.** A copy in flight has its whole extent allocated up front, so cutting it leaves
   a full-size file with an undefined tail (`services/copier/CLAUDE.md`). Cancelling it first removes the file.
3. **It says what happened.** "Powered off", or "halted, safe to switch off", never one claimed for the other.

Everything else is left to recovery, which is what recovery is for. **The tidy-up has a fixed budget, about 1 s,
and the machine goes off when it runs out whether or not every step answered.** No service is drained, no driver
is waited on, and nothing is retried. A step that does not answer is said in one line and skipped.

## 3. The command

| Form | Meaning |
|---|---|
| `shutdown` | in 3 s |
| `shutdown 5`, `5s`, `5m`, `5h` | in that long; a bare number is seconds, as `wait 5` is. The ceiling is 24 h |
| `shutdown now` | no countdown and no tidy-up: the power-off call at once, after naming a copy it will cut |
| `shutdown status` | when, and how long is left; one record when piped |
| `shutdown cancel` | stop a pending shutdown |

One shutdown is pending at a time: a second is refused and names the first. The duration words are the shell's
`parse_duration`, which `wait` and `serve` already use. A delay over 24 h is refused with a sentence: a
shutdown a day away is a calendar's job.

## 4. Who does what

- **The shell** parses the command, shows a short countdown, and owns the keys. Nothing else.
- **`power`** owns every shutdown that has a delay, from the moment it is asked: the deadline, the warnings, the
  tidy-up and the final call. A five-hour wait cannot live in the shell, which would hold the prompt for five
  hours and lose the shutdown silently if it restarted.
- **The kernel** turns the machine off, or halts it, when `power` asks. It knows nothing about why or when.

`power` is already the home `docs/power.md` 15.6 names for "device power-down, shutdown, thermal".

### 4.1 The foreground countdown, and `[b]`

Under a minute, the shell shows the countdown on one line, rewritten each second, with the keys rule 10 asks for:

<!-- foreign-ok: `shutdown` is designed here and not built; it leaves the shell's FOREIGN_HINTS in step S4 -->
```text
gsh> shutdown 5
shutdown in 5s   [q] quit   [b] background
```

- **`q` cancels the shutdown** (rule 11: quitting stops the task, not just the view of it).
- **`b` stops watching** and gives the prompt back. The timer was always `power`'s, so nothing moves.

A minute or more returns the prompt at once: `shutdown at 14:05 (in 5h) - shutdown cancel stops it`. The clock
time is said only when `time` has one; otherwise the delay alone.

### 4.2 Warnings

`power` prints them as its own log lines, the way other services already print between prompts: when asked, then
at 1 h, 30 m, 15 m, 5 m and 1 m before, then every second for the last 10. The shell cannot print on a timer
while it waits for a key, and a warning from `power` is truthful about who is speaking. While a full-screen
program (`edit`) owns the screen the lines may land on top of it; that is recorded, not solved here.

`jobs` lists a pending shutdown as a row, `shutdown in 4h 12m (power)`, so one command shows everything pending.
It is a row, not a `copier` job.

### 4.3 At the deadline

In order, the whole of it bounded at about 1 s:

1. **No new job** may start in the last minute: `background` and a typed copy refuse, naming the shutdown.
2. **`copier`**, if it is running, is sent a cancel (300 ms): a copy removes its partial destination. A check, a
   scrub and a tree delete are one `fs` request and cannot be stopped; they are cut, and `fs` recovers.
3. **`fs`** is asked to quiesce (up to 1 s): finish the request in hand, flush the device, and refuse every write
   after. Unanswered (a 34 s check in flight on a USB stick) is said, and skipped: the journal covers metadata.
4. **The kernel's power-off call.**

`shutdown now` skips 1 to 3 and names a copy it will cut; that is a power cut asked for on purpose.

### 4.4 Who remembers a pending shutdown

Asked by the operator, 2026-10-14: if `power` dies, the supervisor restarts it, but who remembers the shutdown -
and what if the machine booted with no disk at all? The answer has to hold state somewhere without the kernel.

**Not the kernel**, which keeps no service state (26.10), and **not the supervisor**, which would become a store
for other services' state - a new job for the trusted root - and is itself restartable, so it loses what it holds.
**Not `fs` either** (an earlier draft wrote the deadline to `/system/services/power/shutdown`): a machine booted with
no disk has no `fs` to write to, and a pending shutdown must work there too.

**The two that already know hold it, each in memory: `power`, which owns the timer, and the shell, which asked.**
Under question 1's answer the shell is the only one that may ask, so it is also the one that knows what was asked.

- **`power` restarts**: it knows of no pending shutdown. While the shell's copy says one is pending, the shell asks
  `power` at each prompt whether it still has it - one fast request, made only while a shutdown is pending - and if
  `power` says no, it sets it again with the time that is left and says so: `power restarted - the shutdown is set
  again for 14:05`.
- **The shell restarts**: it asks `power` once whether a shutdown is pending (`shutdown status`) and rebuilds its row.
- **Both die in the same window**: the shutdown is lost. The supervisor logs both deaths, and the machine stays on,
  which is the safe direction for this failure. Recorded rather than closed (26.7).
- **No disk**: nothing here needs `fs`. A shutdown works the same, and the tidy-up's `fs` step is skipped because no
  `fs` is running.

The same rule GodspeedOS applies everywhere after a restart (CLAUDE.md 14.3): reacquire, and re-derive what hung off
the dead instance - here, from the peer that asked.

If `power` is not running when `shutdown` is typed, the shell says so and nothing happens: it fails safe.

## 5. What each board can do

Read 2026-10-14; the two "not checked" rows need a card.

| Board | Route | What happens |
|---|---|---|
| QEMU x86 | ACPI S5: `SLP_TYPa` from `\_S5`, written to `PM1a_CNT` | powers off (worked in June 2026, `utilities/14_poweroff.md`) |
| HP T630 | the same | **does not power off**: the firmware gates S5 on `\_PTS` / `\_GTS`, ACPI bytecode needing an interpreter. Halts |
| Dell Wyse 5070 | the same | not tried |
| Raspberry Pi 4 | the power-management watchdog `reboot` already uses, with the boot partition set to 63 (`0x555`), which the firmware reads as halt (Linux `bcm2835_power_off`) | halts; with `POWER_OFF_ON_HALT=1` in the EEPROM the firmware also turns the rails off. Not checked on this board |
| Raspberry Pi 2 | the same watchdog | halts, "most of the chip powered off" (Linux's comment); 5 V stays on |
| VisionFive 2 Lite | SBI SRST, the call `reboot` already makes, type shutdown | OpenSBI powers the AXP15060 PMIC off on the JH7110 since 2023; whether the firmware in this board's flash has the later device-tree fix it needs is not checked, and its cold reboot already dies reading the PMIC (`pmic_ops: cannot read pmic power register`, `arch/riscv64/sbi.rs`), so a refusal or a dark board is likely |
| QEMU RISC-V `virt` | SBI SRST shutdown | powers off |

So the arch answer is three-valued: **off** (the call does not return), **halt** (the firmware will hold the
board stopped), and **cannot** (the port has no route). The kernel says which before it acts.

## 6. The kernel's part

One new syscall, `PowerOff`, gated by a new capability, `POWER_OFF`, held by `power` alone, as `CPU_CLOCK` is: the
machine has one power state and a second holder would race the first. Behind it, one new member of the
`arch::imp` seam that every port must answer (`scripts/arch_seam_check.py`):

- x86-64: find `PM1a_CNT` in the FADT (the ACPI tables `iommu.rs` already walks for IVRS) and `SLP_TYPa` in the
  DSDT's `\_S5` package, then write S5. If the machine is still running a moment later, it did not take.
- arm32 and AArch64: `hardware_reset` with the boot partition set to 63 instead of cleared.
- RISC-V 64: SBI SRST, reset type shutdown, probed first as `hardware_reset` does.
- The stubs (`riscv32`, `loongarch64`, `s390x`): cannot.

**When the hardware does not power off**, the kernel halts every core through the path a panic already takes, and
prints its last line through the 11.4 floor: `halted - it is safe to switch off`. A halt is not a crash, so it is
not printed as one.

### 6.1 Does this add a kernel responsibility?

No. Turning the machine off is reboot's sibling: the same registers on the Pis, the same firmware call on RISC-V,
the ACPI tables the kernel already reads on x86. It is mechanism on channels the kernel already owns, the argument
the `DevicePower` and `CpuClock` amendments made (CLAUDE.md 12.3). The kernel learns nothing about why, when, or
what to save; the countdown, the tidy-up and the decision are services'. What it does add is **surface**: a
syscall, a capability and a seam member, which Commandment I pins. So it needs the amendment below before the
gate will accept it, and that is the gate working.

### 6.2 The amendment, as it would be written into CLAUDE.md 12.3

> **Amendment 2026-10-xx (`PowerOff`, syscall 56): the kernel turns the machine off, or halts it, and a SERVICE
> decides when.** `reboot` resets the machine through a channel the kernel owns on every port; turning it off uses
> the same channels - the Pis' power-management watchdog, the RISC-V firmware's SRST call, the ACPI tables x86
> already parses - asked for a different outcome. `PowerOff` is gated by `POWER_OFF`, granted to ONE service,
> `power`, because the machine has one power state. Each port answers the new seam member with what it can do:
> off, halt, or cannot, and the kernel says which before it acts; where the hardware does not power off, every
> core halts through the panic path's stop and the 11.4 floor prints that it is safe to switch off. The kernel
> holds no policy (26.10): the countdown, the warnings, the tidy-up and whether to shut down at all are
> `power`'s (`docs/shutdown.md`). Not a seventh responsibility - mechanism on channels the kernel already owns -
> but a new syscall, resource and seam member, which Commandment I pins and this records.

## 7. Open questions, for the operator

1. **Who may ask `power` to shut down.** A send right to `power` today carries its whole protocol, and `wifi-driver`
   holds one (for leases). Without more, it could ask for a shutdown too. Options:
   - (a) the send right IS the authority: record the holders (the shell, `wifi-driver`) and pin them with a gate.
     Simplest; honest about a coarse grant. On the Pis `wifi-driver` is unconfined DMA and already
     kernel-equivalent (CLAUDE.md 6.4, SEC-33), so it gains nothing there it lacked.
   - (b) a narrower right: `power` mints a delegated resource capability for "shut down" (CLAUDE.md 7.10, as `fs`
     mints a file) and the supervisor hands it to the shell alone. No service has two endpoints today, so this is
     the capability-correct route; it costs `power` the `RESOURCE_MINT` authority and the supervisor a hand-over.
   **Decided 2026-10-14: (b).** The operator chose the narrower right.
2. **`reboot` moves into `power`, decided 2026-10-14, as its own step (S6)** after `shutdown` has passed its cards:
   both ways of ending a session then go through one service with one budget, the shell holds neither authority,
   and `reboot` gains the tidy-up it lacks today (it resets without flushing the disk).
4. **The name is `shutdown`, decided 2026-10-14.** It is the word other systems use, and it names the act - ending
   the session - rather than promising a mechanism: on the T630 and the Pi 2 the machine halts and does not lose
   power, so `poweroff` would claim what did not happen. `poweroff` and `halt` stay hints that answer
   "try `shutdown`" (step S4).
3. **Clock time** (`shutdown at 22:00`) is left out: it depends on `time` having a clock, and a durations-only
   first version cannot be wrong about that.

## 8. Phasing

Each step is QEMU-verified before the next, and the cards come at the end.

| Step | What | Verified by |
|---|---|---|
| S1 | The kernel: `PowerOff`, `POWER_OFF`, the seam member on every port, the halt fallback, the amendment | QEMU x86 and RISC-V power off; the Pis' QEMU halt; a probe without the capability is refused |
| S2 | `fs`: `OP_QUIESCE` - finish, flush, refuse writes after | `osdev test files`: a write after quiesce is refused loudly |
| S3 | `power`: the schedule, warnings, the final sequence, and question 1's answer; the shell re-arms a shutdown `power` lost in a restart (4.4) | a QEMU suite: `shutdown 5s`, `cancel`, `status`, a `power` kill mid-wait |
| S4 | The shell: `shutdown`, the countdown with `[q]` and `[b]`, the `jobs` row, the last-minute refusal; `shutdown` leaves `FOREIGN_HINTS` (where it answers "try `reboot`" today), `halt` and `poweroff` hint at `shutdown` instead, and CLAUDE.md Appendix B.4 records why `shutdown` is a command | `osdev test quit` and `jobs` cases; `foreign_word_check` with the `foreign-ok` markers in this note removed |
| S5 | Cards on all five boards | each says "powered off" or "halted" truthfully, under 5 s from Enter for a plain `shutdown` beyond its 3 s |
| S6 | `reboot` moves into `power`: the same bounded tidy-up before the reset, `reboot now` as `shutdown now`, Ctrl+Alt+Del asking `power`, and `REBOOT` leaving the shell | the jobs and quit suites, and a card per board |

## 9. Sources

- Linux `drivers/watchdog/bcm2835_wdt.c`, `bcm2835_power_off`: partition 63 (`PM_RSTS_RASPBERRYPI_HALT`, `0x555`)
  tells `bootcode.bin` not to reboot.
- OpenSBI, "Add StarFive JH7110 SoC shutdown and reboot ops" (2023), and the later commit matching the AXP15060's
  device-tree compatible to U-Boot's, without which OpenSBI found no PMIC and shutdown did nothing.
- Raspberry Pi 4 EEPROM `POWER_OFF_ON_HALT`: rails off on halt, about 0.15 W; PCB before 1.4 needs a modified
  EEPROM.
- `utilities/14_poweroff.md`: the June 2026 attempt, and the `\_PTS` wall on the T630.
