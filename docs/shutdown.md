# Shutdown: turning the machine off on purpose

**Status:** built on branch `feat/shutdown`, 2026-10-14; QEMU-verified and **hardware-verified on all five boards**
(S5, section 5). S1 to S4 are built: the kernel's `PowerOff` with its one deadline (S1), `fs`'s quiesce (S2),
`power`'s warnings and tidy-up (S3) and the shell's `shutdown` (S4). `osdev test shutdown` passes 37 of 37 on x86;
QEMU RISC-V powers off through SBI SRST; the refusal without the capability is `osdev test adv` A12. The Wyse powers
off; the T630, the Pis and the VisionFive halt and say so. The command is `utilities/64_shutdown.md`.

**`reboot` on the same deadline (S6), built 2026-10-15, QEMU-verified; hardware cards pending.** `reboot` takes
`shutdown`'s forms - a 3 s countdown, `reboot <delay>`, `reboot now`, `status`, `cancel` - and a countdown gets the
same warnings and tidy-up, so a plain `reboot` now flushes the disk first. `osdev test shutdown` passes 55 of 55,
including a reboot that falls due and comes back to a prompt. The command is `utilities/13_reboot.md`.

## 1. What is asked for

The operator, 2026-10-14: `shutdown 5` shuts down in five seconds, counting down on screen; `shutdown now` shuts
down at once; a plain `shutdown` waits a default 3 s; a delay may be `5s`, `5m` or `5h`, a long one runs in the
background, and the warnings come closer together as the moment nears. And: "GodspeedOS is built to fail and
recover in its core DNA ... I don't want those checks to take a long time. If I type `shutdown` it should
shut down in much less than 5 seconds." And, when a pending shutdown could have been lost with the services
holding it: that "has to be unacceptable."

## 2. The principle: a polite power cut

GodspeedOS does not need a shutdown to keep its disk consistent. `fs` commits every metadata change through a
redo journal and recovers on mount, and every board has recovered from an unassisted power cut (CLAUDE.md 6.1,
amendment 2026-09-23: five of five). So a shutdown is not a rescue. It does three things a power cut does not:

1. **It loses nothing you believe is saved.** File DATA written by `write_at` may still be in the device's cache
   (CLAUDE.md 6.1, 2026-07-25: "unflushed FILE DATA can be lost on a power cut"). One flush closes that.
2. **It leaves nothing half-made.** A copy in flight has its whole extent allocated up front, so cutting it leaves
   a full-size file with an undefined tail (`services/copier/CLAUDE.md`). Cancelling it first removes the file.
3. **It says what happened.** The machine is off, or the kernel says every core is halting and it is safe to
   switch off - never one claimed for the other.

Everything else is left to recovery, which is what recovery is for. **The tidy-up is bounded - up to a second for
`copier` and up to a second for `fs` - and the machine goes off when the deadline falls due whether or not
every step answered.** No service is drained, no driver
is waited on, and nothing is retried. A step that does not answer is said in one line and skipped.

## 3. The command

| Form | Meaning |
|---|---|
| `shutdown` | in 3 s |
| `shutdown 5`, `5s`, `5m`, `5h` | in that long; a bare number is seconds, as `wait 5` is. The ceiling is 24 h |
| `shutdown now` | no countdown and no tidy-up: the power-off at once, after warning that a running job will be cut |
| `shutdown status` | whether one is pending, and how long is left |
| `shutdown cancel` | stop a pending shutdown |

The duration words are the shell's `parse_duration`, which `wait` and `serve` already use. A delay over 24 h is
refused with a sentence: a shutdown a day away is a calendar's job.

## 4. Who does what

- **The shell** holds the authority, `POWER_OFF`, beside `REBOOT`: ending the session is the operator's, and the
  shell is how the operator speaks. It sets and cancels the deadline (or turns the machine off at once with `shutdown now`), shows a
  short countdown, and owns the keys.
- **The kernel** holds the deadline - the machine's ONE pending power-off - and fires it when it falls due,
  whether or not any service is alive. It knows nothing about why.
- **`power`** holds no shutdown authority (no `POWER_OFF`; it keeps `CPU_CLOCK` for the clock leases). It reads the
  deadline, gives the warnings and does the tidy-up three seconds before the deadline. The shell cues it when the deadline changes, and when it starts or ends `copier` while one is pending, so the tidy-up
  asks for `copier` only when a copy can be running; the cue carries no authority, because `power` only ever
  re-reads what the kernel holds, so a forged cue from anyone changes nothing but whether that one request is made.

### 4.1 The foreground countdown, and `[b]`

Under a minute, the shell shows the countdown on one line, rewritten each second, with the keys rule 10 asks for:

```text
gsh> shutdown 5
shutdown in 5s   [q] quit   [b] background
```

- **`q` cancels the shutdown** (rule 11: quitting stops the task, not just the view of it).
- **`b` stops watching** and gives the prompt back. The deadline was always the kernel's, so nothing moves.

A minute or more returns the prompt at once: `shutdown in 5m - shutdown cancel stops it`.

### 4.2 Warnings

`power` prints them as its own log lines, the way other services already print between prompts: when the deadline
is set, then at 1 h, 30 m, 15 m, 5 m and 1 m before, then every second for the last 10. The shell cannot print on a
timer while it waits for a key, and a warning from `power` is truthful about who is speaking. While a full-screen
program (`edit`) owns the screen the lines may land on top of it; that is recorded, not solved here.

`jobs` lists a pending shutdown above the jobs, `shutdown in 4h 12m (pending; shutdown cancel stops it)`, so one
command shows everything pending. A line, not a row: it is not a `copier` job.

### 4.3 At the deadline

The shell refuses `background` in a shutdown's last minute, naming the shutdown. Then `power`, three seconds out,
each step bounded at about a second - two steps in the two to three seconds left - and best-effort: a step
that is slower still is cut by the deadline:

1. **`copier`**, if it is running, is sent a cancel: a copy removes its partial destination. A check, a scrub and a
   tree delete are one `fs` request and cannot be stopped; they are cut, and `fs` recovers.
2. **`fs`** is asked to quiesce (up to 1 s): finish the request in hand, flush the device, and refuse every write
   for ten seconds after. Unanswered (a 34 s check in flight on a USB stick) is said, and skipped: the journal
   covers metadata. Ten seconds and not forever, so a shutdown that never comes leaves a filesystem that takes
   writes again by itself - and a shutdown CANCELLED after this step has `power` tell `fs` to take writes again at
   once.

Then the kernel's deadline falls due and the machine goes off. `shutdown now` skips both; that is a power cut
asked for on purpose.

### 4.4 Who remembers a pending shutdown: the kernel

Asked by the operator, 2026-10-14: if `power` dies the supervisor restarts it, but who remembers the shutdown - and
what if the machine booted with no disk? A first answer kept it in `power`'s memory and the shell's, each re-arming
the other after a restart; that lost the shutdown if both died at once, and the operator called that unacceptable.

**The kernel holds it**, because the kernel is the one thing that cannot die. Not as service state - CLAUDE.md 15
keeps state with services, and a per-service store in the kernel would bring back name-keyed kernel tables and
hand a crashed service the state that crashed it - but as the kernel's own scheduled action: one deadline, on the
same tick every timed wait already uses.

- **`power` restarts**: it reads the deadline (InspectKernel query 28) and carries on, saying what it found.
- **The shell restarts**: `shutdown status` reads the kernel; nothing to rebuild.
- **Everything else dies**: the kernel still turns the machine off at the deadline, without the tidy-up if `power`
  is not there to do it. A polite power cut becomes a plain one, which `fs` recovers from; never a lost one.
- **No disk**: nothing here needs `fs`. The tidy-up still asks `fs` to quiesce; with no volume it answers that
  there is none, or - as on QEMU RISC-V, where it was waiting on the storage driver - does not answer in time.
- **A reboot** clears it, as it clears all kernel memory. A shutdown can never persist across boots.
- **A reboot of its own** rides the same deadline (S6, section 8): the deadline records which of the two it is,
  and everything above holds for it unchanged.

## 5. What each board can do

Every row has run, 2026-10-14. The time is from a plain `shutdown` being set to the last line, on the board's
serial: its 3 s, plus the firmware's part.

| Board | Route | What happens |
|---|---|---|
| QEMU x86 | ACPI S5: `SLP_TYPa` from `\_S5`, written to `PM1a_CNT` | powers off (`osdev test shutdown`) |
| HP T630 | the same | **halts**, 3.1 s: S5 is written and the machine stays on, the power light lit - the firmware gates S5 on `\_PTS`, ACPI bytecode this kernel does not interpret, as in June 2026 (`utilities/14_poweroff.md`). The kernel says so, then every core halts quietly |
| Dell Wyse 5070 | the same (`SLP_TYPa` 7 on `0x0404`) | **powers off**, 3.0 s |
| Raspberry Pi 4 | the power-management watchdog `reboot` already uses, with the boot partition set to 63 (`0x555`), which the firmware reads as halt (Linux `bcm2835_power_off`) | **halts**, about 3 s; with `POWER_OFF_ON_HALT=1` in the EEPROM the firmware also turns the rails off (whether this board's EEPROM sets it was not checked). `reboot` checked on the same card |
| Raspberry Pi 2 | the same watchdog | **halts**, 3.0 s, "most of the chip powered off" (Linux's comment), with 5 V still on |
| VisionFive 2 Lite | SBI SRST, the call `reboot` already makes, type shutdown | **stops in the firmware**, 3.1 s: OpenSBI says `pmic_ops: cannot read pmic power register` and never returns, the step its cold reboot already dies at (`arch/riscv64/sbi.rs`). OpenSBI powers the AXP15060 off on the JH7110 since 2023; this board's flash evidently lacks the later device-tree fix. So the kernel says what to do BEFORE the call |
| QEMU RISC-V `virt` | SBI SRST shutdown | powers off; the tidy-up starts in time, and with no disk `fs` - blocked waiting on the storage driver - did not answer its quiesce before the kernel fired, which is the designed fallback |

So the arch answer is three-valued: **off** (the call does not return), **halt** (the firmware will hold the
board stopped), and **cannot** (the port has no route). The kernel says which before it acts.

## 6. The kernel's part

One syscall, `PowerOff` (56). The power-off ops are gated by `POWER_OFF` (resource 19), the reboot ops by
`REBOOT` (resource 8); the shell holds both:

- **op 0, now**: does not return. The machine is off, or every core is halted. (A reboot now is `Reboot`, 18.)
- **op 1 / op 3, at `secs`** (1 ..= 24 h): set the machine's one pending action, a power-off (1) or a reboot (3).
  **Earliest wins**: a request for LATER than a pending one of the same kind is refused (code 1) and names it, so
  no holder can postpone what another asked for; one for sooner moves it forward. **One action**: while one kind
  is pending, a request for the other is refused (code 2), so a reboot can never quietly swallow a shutdown or the
  other way round.
- **op 2 / op 4, cancel**: any holder of the right clears a pending power-off (2) or reboot (4); code 2 when the
  pending one is the other kind, which is kept.
- **Every set and cancel is logged** with the caller's name, from the task that called - nothing is looked up by
  name. The deadline counts in BSP ticks, never the wall clock, so a clock set from a spoofed network cannot move
  it. It is one word, the tick and the kind together, so a set, a cancel and the tick that fires it each see both
  at once.
- **Query 28** of InspectKernel returns `secs * 2 + kind` (1 for a reboot), or -1 when none is pending - one answer,
  so the time and the kind cannot disagree. UNGATED, deliberately: a deadline that outlives every service must be
  visible to everything that might report it, and one planted by a compromised holder must never be hidden. It
  discloses no task's state.
- **A pending reboot that falls due** resets through `hardware_reset`, the path `Reboot` takes, after putting its
  line straight on the wire: `reboot: the pending reboot fell due`.

Behind it, one member of the `arch::imp` seam every port answers (`scripts/arch_seam_check.py`):

- x86-64: find `PM1a_CNT` in the FADT (the ACPI tables `iommu.rs` already walks for IVRS) and `SLP_TYPa` in the
  DSDT's `\_S5` package, then write S5 (`arch/x86_64/acpi.rs`). If the machine is still running a moment later,
  it did not take.
- arm32 and AArch64: `hardware_reset`'s watchdog reset with the boot partition set to 63 instead of cleared.
- RISC-V 64: SBI SRST, reset type shutdown, probed first as `hardware_reset` does.
- The stubs (`riscv32`, `loongarch64`, `s390x`): cannot.

**When the hardware does not power off**, the kernel halts every core through the path a panic already takes, and
its last line is `power-off: every core is halting - it is safe to switch the machine off`. A halt asked for is
not a crash, and is not printed as one.

### 6.1 Does this add a kernel responsibility?

No, and MISCIS stays at six. Turning the machine off is reboot's sibling: the same registers on the Pis, the same
firmware call on RISC-V, the ACPI tables the kernel already reads on x86 - mechanism on channels the kernel already
owns, the argument the `DevicePower` and `CpuClock` amendments made (CLAUDE.md 12.3). The deadline is one more
timed action on the scheduler's existing tick. The kernel holds no policy: when, and whether, are the holder's, and
the bounds (one deadline, earliest wins, 24 h) are limits on a value, as a queue depth is. What grows is
**surface** - the syscall, its ops, the query, the resource and the seam member - which Commandment I pins and the
CLAUDE.md 12.3 amendment of 2026-10-14 records.

### 6.2 Security

Deferring adds no new reach: whoever can set a deadline could already turn the machine off at once. What it adds is
a shutdown that can be planted now and fire later, after the service that planted it has been restarted. That is
acceptable only because it can never be anonymous or silent: one holder today, every change logged with who, the
deadline readable by all and announced by `power` whenever it starts, monotonic time only, and gone at reboot.

## 7. Decisions and open questions

1. **Only the shell may shut the machine down, decided 2026-10-14.** No service has a reason to end the session -
   a failing service is restarted on its own - so the right is the operator's, beside `REBOOT`. The Commandments
   baseline lists the shell as its one holder; a second is a decision made there, in a commit that says why.
   Earlier options - any sender to `power`, or a capability `power` mints and hands out - were dropped: a request
   routed through `power` cannot say who sent it, which a deferred shutdown must.
2. **A second holder, when one is wanted** (the PCs' power button, a battery or thermal limit): it holds
   `POWER_OFF` itself, acts only on its own inputs, and never offers "shut down on request" to other services - a
   holder that did would reopen the anonymous door one level up. With more than one holder the single deadline is
   still exact (earliest wins, any holder cancels). A per-holder request list, where each cancels only its own, is
   the refinement; it needs an identity that survives a restart, which is a name, and the kernel keeps no
   name-keyed tables, so that is decided when the second holder arrives.
3. **`reboot`** (S6, built 2026-10-15): the same kernel deadline holds "turn off or reboot at T", with the shell
   holding both rights and `power` doing the tidy-up for either - which gives a plain `reboot` the disk flush the
   old immediate reset lacked. Asked by the operator: `reboot` counts down from 3, `reboot now` is immediate,
   `reboot 10m` and so on. The two kinds share ONE deadline rather than having one each, so the machine can never
   hold a shutdown and a reboot at once and have to choose; the second asked for is refused and names the first.
   `reboot now` and Ctrl+Alt+Del stay immediate, with no tidy-up, as `shutdown now` does.
4. **The name is `shutdown`, decided 2026-10-14.** It is the word other systems use, and it names the act - ending
   the session - rather than promising a mechanism: on the T630, the Pis and the VisionFive the machine halts and
   does not lose power (section 5), so `poweroff` would claim what does not happen. `poweroff` and `halt` are hints that answer `shutdown`.
5. **Clock time** (`shutdown at 22:00`) is left out: it depends on `time` having a clock, and a durations-only
   first version cannot be wrong about that.

## 8. Phasing

| Step | What | Verified by |
|---|---|---|
| S1 | The kernel: `PowerOff` now, at and cancel; the one deadline on the BSP tick; query 28; `POWER_OFF` to the shell; the seam member on every port; the halt fallback; the amendment | QEMU x86 and RISC-V power off; `osdev test adv` A12 refuses it without the capability |
| S2 | `fs`: `OP_QUIESCE` - finish, flush, refuse writes for 10 s | `osdev test shutdown`: fs says it flushed |
| S3 | `power`: read the deadline, warnings, the tidy-up three seconds out, carry on after a restart | `osdev test shutdown`: the countdown and tidy-up lines |
| S4 | The shell: `shutdown`, the countdown with `[q]` and `[b]`, the `jobs` line, the last-minute refusal; `shutdown` leaves `FOREIGN_HINTS` and CLAUDE.md Appendix B.4 says why | `osdev test shutdown` 37/0 |
| S5 | Cards on all five boards | **done 2026-10-14**: each says off or halted truthfully, and its last line lands within about 0.1 s of a plain `shutdown`'s 3 s (section 5). The cards found four things QEMU could not show, fixed in turn: log lines joined onto the shell's line on serial; three cores' fault reports printed over the T630's halt; the Pis' last line cut off by their own watchdog; and the VisionFive never saying it was safe to switch off |
| S6 | `reboot` on the same deadline, with the tidy-up | **built 2026-10-15**: `osdev test shutdown` 55/0, a reboot falling due and the machine back at a prompt; the jobs and quit suites; a card per board pending |

### Recorded, not fixed: a log line can still join the shell's line on serial

A log line starts a new line on serial when the shell has left one open (`log.rs`, `SERIAL_LINE_OPEN` and
`CONSOLE_WRITES`). It decides that before its own write waits for the port, so when the shell takes the port in
between, the two still run together: the Pi 4 card showed it once, `[b] backgroundpower: tidy-up`. The commonest
case is closed at its source: the shell cues `power` only after its own line is out, because cued first, `power`'s
announcement raced the shell's first countdown line every time (`osdev test shutdown` caught it in QEMU). What is
left is two clocks that tick in the same second - the shell's countdown and `power`'s warnings. Only each port's
serial writer knows the order bytes reach the wire - on the Pi 2 a queue drained later - so the complete fix is in
four writers, the most safety-critical output path there is, for tidiness. The operator chose to leave it recorded,
2026-10-14. The screen is unaffected: log lines go to serial only.

## 9. How other systems do it

- **Linux**: `reboot(2)` gated by `CAP_SYS_BOOT`; `shutdown +5` is held by systemd-logind in memory and a file in the
  RAM-backed `/run`, with warnings to logged-in users; polkit decides who may ask, and the power button, UPower and
  UPS daemons ask too. A critical thermal trip makes the kernel itself ask userspace to power off, and arm a timer
  that forces it if userspace does not.
- **Windows**: a privilege (`SeShutdownPrivilege`); `shutdown /s /t` held by the session manager, `shutdown /a`
  aborts; services get a shutdown notice and about 20 s.
- **Classic BSD and macOS**: `shutdown -h +5` forks a process that sleeps, warns and then calls `reboot()`; kill it
  and the shutdown is lost.
- **Fuchsia**: `zx_system_powerctl` needs a root-resource handle, held by one component; who may ask it is settled by
  capability routing.

## 10. Sources

- Linux `drivers/watchdog/bcm2835_wdt.c`, `bcm2835_power_off`: partition 63 (`PM_RSTS_RASPBERRYPI_HALT`, `0x555`)
  tells `bootcode.bin` not to reboot.
- OpenSBI, "Add StarFive JH7110 SoC shutdown and reboot ops" (2023), and the later commit matching the AXP15060's
  device-tree compatible to U-Boot's, without which OpenSBI found no PMIC and shutdown did nothing.
- Raspberry Pi 4 EEPROM `POWER_OFF_ON_HALT`: rails off on halt, about 0.15 W; PCB before 1.4 needs a modified
  EEPROM.
- `utilities/14_poweroff.md`: the June 2026 attempt, and the `\_PTS` wall on the T630.
