# 74. On every board image the last services spawned get no reply mailbox, and `net-stack` is always one of them

**Status: OPEN, low priority - found 2026-10-04 from a VisionFive boot, confirmed in a Pi 4 log. Measured on both radio boards the same day: the mailboxes make no measurable difference (below). Options 3 and 4 are DONE
(refusals named; a watched task's or the supervisor's respawn takes back a released mailbox,
hardware-verified on the Pi 2 2026-10-06); the reserve stays at 72.**

## What was seen

Every boot prints `routing: reply endpoint refused` two or three times, and it had been read as noise. The
kernel registers a task's reply mailbox BEFORE it prints `task: '...' spawned OK`, so each refusal belongs
to the spawn that follows it. Read that way:

```
VisionFive 2 Lite (V1 boot, 2026-10-04 15:35)
task: 'wifi-driver' spawned OK on core 3 (slot 11)
routing: reply endpoint refused - 71 of 96 slots free, reserve 72 (1 refused so far)
task: 'nic-driver' spawned OK on core 1 (slot 12)
routing: reply endpoint refused - 70 of 96 slots free, reserve 72 (2 refused so far)
task: 'net-stack' spawned OK on core 1 (slot 13)

Pi 4 (build/pi4_cache_after_storm.log)
task: 'wifi-driver' spawned OK on core 3 (slot 11)
routing: reply endpoint refused - 71 of 96 slots free, reserve 72 (1 refused so far)
task: 'pwm-audio' spawned OK on core 2 (slot 12)
routing: reply endpoint refused - 70 of 96 slots free, reserve 72 (2 refused so far)
task: 'nic-driver' spawned OK on core 1 (slot 13)
routing: reply endpoint refused - 69 of 96 slots free, reserve 72 (3 refused so far)
task: 'net-stack' spawned OK on core 1 (slot 14)
```

So on both radio boards `nic-driver` and `net-stack` have never had a reply mailbox, and on the Pi 4
neither has `pwm-audio`. The same line is in at least ten other logs (QEMU, the Pi 2, the audio runs).

## Why it happens

`kernel/src/ipc/routing.rs`: `OPTIONAL_RESERVE = MAX_ENDPOINTS * 3 / 4` = 72 of 96. A reply mailbox is
optional and is refused once free slots fall to 72, so optional and mandatory endpoints together may use 24
slots before every later mailbox is refused. A board image of 12-14 services, two endpoints each, crosses
that at about the eleventh spawn. The reserve was sized for the PROBE builds - "70 services hold recv
endpoints at peak", about 178 services spawned - where it fixed a real priority inversion (P5, P7). A board
image never comes near that peak, and pays for it with the mailboxes of whatever spawns last.

## What it costs

- **The hazard the mailbox exists to remove is live in exactly the services it was written for.**
  `net-stack` serves clients on its endpoint AND awaits `nic-driver`'s replies there; `nic-driver` serves
  `net-stack` there and awaits the radio's replies there. The SDK says so at `reply_mailbox`: without one
  a service "cannot drain client traffic while waiting on the endpoint it also serves".
- **The stale-reply repair is off for them.** `drain_stale_replies` and `drain_owed_replies` refuse to
  drain a shared endpoint, correctly, so the desync repair the SDK documents does not run in these
  services.
- **Refusals after the third are almost silent.** The line prints for the first three and then every
  64th. A respawn after a chaos storm takes whatever is free at that moment, so a respawned `fs` or
  `shell` can lose its mailbox and nothing in the log says which. That is invariant 12 bent: the refusal
  is counted, but the operator cannot see who paid it.

## What it may explain - NOT shown

`backlog/66`'s steady one-second tax is between exactly these two services: the first STATUS of each
exchange is received by `nic-driver` about a second late, served back to back with the retry. 66 reads that
as a lost wake-up in the kernel's blocked-receiver path. Neither service had a mailbox in any of those
boots, so every reply and every request in that exchange shared a queue with the other kind. That is a
candidate, not a cause: nothing here shows the mechanism, and 66 stays as written until a boot with
mailboxes says otherwise. **Corrected 2026-10-08:** 66's later form, `net dns` failing over the radio, was
an empty reply the kernel refused on three ports (`e3fcf7ed`), not the mailboxes; the one-second STATUS tax
this paragraph describes had stopped by 2026-10-04 and was never explained.

## Ruled out

Nothing yet - this entry is the measurement of who is refused, not a diagnosis of any symptom.

## The next step, and the options

**The measurement first, and it is cheap:** one boot of each radio board with the mailboxes granted, then
`backlog/66`'s exchange timings and the `op 3 ... timed out on the first attempt` lines compared against
the boots above. That needs one of the changes below (all kernel, all the operator's call):

1. **A smaller reserve.** The probe builds are what it protects; a quarter-table reserve was measured to
   fail P7. It would need re-measuring P5 and P7 under the probe build, which is the honest cost.
2. **Make an optional entry evictable.** A mandatory registration that finds the table full takes a
   mailbox's slot, and that mailbox's owner falls back to its shared endpoint - which is the fallback it
   already has. This fixes the inversion at its source and needs no reserve, but evicting a mailbox a task
   is blocked on needs the same wake-with-error the death path has.
3. **Name the refused task.** Independent of 1 or 2 and the smallest change: print the task's name with
   every refusal (not only the first three), so a log says who runs without one.

Recommendation: 3 now (it only adds information), then 1 or 2 with the measurement above.

## Option 3, done (2026-10-04, operator's go-ahead)

`try_register_optional` returns the refusal (`OptionalRefusal`: free, total, reserve, count since boot)
instead of printing it, and the spawn path prints EVERY refusal with the task's name:

```
spawn[ipc]: 'net-stack' gets no reply mailbox - 69 of 96 routing slots free, reserve 72 (3 refused since boot); it awaits replies on its own endpoint
```

The decision and the threshold are unchanged; only who is named, and how often, changed. The old line,
`routing: reply endpoint refused`, is gone. Verified in QEMU (riscv64, Pi 4) and on the VisionFive 2 Lite
the same day: `nic-driver` (1) and `net-stack` (2) at boot, and `wifi-driver` (3) when `wifi radio on`
restarted it, the numbers as predicted. Next: the measurement above.

## The measurement on the VisionFive (2026-10-04): inconclusive, because this board shows no tax

Two boots with the cable in, the same commands (`net`, `ping count 20 8.8.8.8`, `wifi radio off hard`,
`wifi radio on`): a test image with the reserve at 48 (never committed; `build/vf_measure_reserve48.log`),
then the committed build at 72. The test image refused nothing; the committed one refused `nic-driver`,
`net-stack` and `wifi-driver`, as predicted. Both pinged 8.8.8.8 at 19-31 ms, one echo a second, 0% loss,
and NEITHER logged a NIC exchange of 300 ms or more during the pings. The only slow exchange in each was
the boot-time link query (748 ms with mailboxes, 840 ms without), one sample each.

So the one-second tax of `backlog/66` does not appear on this board's cable path at all, mailbox or none,
and this board cannot say whether the mailboxes cause it. The Pi 4, where 66 was found and where
`pwm-audio` is refused as well, is the board that can.

## The measurement on the Pi 4 (2026-10-04): the mailboxes make no measurable difference

The same pair on the Pi 4, over the RADIO with the cable pulled after boot, which is how 66 was found:
`net`, one `ping 8.8.8.8`, then `ping count 20 8.8.8.8`. Test image at reserve 48
(`build/pi4_measure_reserve48.log`), then the committed build at 72 (`build/pi4_baseline_reserve72.log`).

| | reserve 48 | reserve 72 (committed) |
|---|---|---|
| refused | none | `pwm-audio`, `nic-driver`, `net-stack` (1, 2, 3), as predicted |
| `ping count 20 8.8.8.8` over the radio | 20/20, 29-66 ms | 20/20, 22-39 ms |
| NIC exchanges of 300 ms or more | none | none |
| the first echo after the switch to the radio | answered after 1185 ms, in a 1277 ms serve pass | answered after 1645 ms, in a 1737 ms serve pass |

**Verdict.** With or without the mailboxes, today's code shows no slow STATUS exchange on either radio
board, so the mailboxes are not shown to be the cause of 66 and the one-second tax itself did not
appear. The reserve stays at 72: options 1 and 2 would change the kernel to fix a cost nobody can now
measure. This item stays OPEN for the structural hazard alone (two services that serve and await replies
on one endpoint, with the drain repair off), at low priority, and option 3 means any respawn that loses
its mailbox is named in the log.

**Not this item, recorded so it is not lost.** In BOTH boots the first `ping 8.8.8.8` after the radio
took over waited in a net-stack serve pass of 1.3-1.7 s before its echo went out (the echo itself took
about 30 ms), and every later echo was prompt. Same place, both reserves, so it is not the mailboxes.
Two samples; not diagnosed.

**Diagnosed and fixed the same day: it was the first ping of the boot, not the radio.** The VisionFive
showed it over the cable (answered after 1383 ms, a 21 ms round trip), which ruled the switch out. Before
its first echo, `net-stack` calibrated its cycle counter (`calibrate_tsc_hz`), which waits for a
wall-clock second to turn and then a whole second more - and it had already done exactly that at startup
for TCP's clock and kept the result there. Ping now starts from the startup measurement and calibrates
lazily only if that one failed. The Pi 4 boot after the fix: first echo over the cable 24 ms, first over
the radio 37 ms, no slow pass logged.

## A cost found: the respawned `wifi-usb` on the Pi 2 (2026-10-06, `docs/wifi-usb.md` 19)

**A driver that RECEIVES NOTICES from the peer it calls is not unaffected.** The measurements above found
no tax on the radio boards, and for a service whose only messages from a peer are replies, that holds.
`wifi-usb` is different: `dwc2` sends it a notice per received USB transfer (`NOTE_BULK_IN`) and calls
from it carry requests, both on one endpoint when there is no mailbox. The kernel matches a call's reply
by sender, so a notice can be taken as the answer. Boot gave `wifi-usb` a mailbox while it was spawned after `fs`; EVERY respawn was
refused one (`71 of 96 routing slots free, reserve 72`). Since it is started on demand on the Pi 2
(`docs/wifi-usb.md` 26) it spawns after `net-stack` and is refused one - every instance, five of five on the card
(`docs/wifi-usb.md` 26): an instance that never held one banks nothing when it is stopped. Each still
joined in 5 to 6 s on the `OP_SYNC` fallback.

The first respawn after R9's power cycle stopped dead: every answer one behind. `usbfn::OP_SYNC`, a
request `dwc2` never answers, now recovers it without a kernel change, and the operator's run that day
(19 restarts of `wifi-usb`, `chaos max-carnage` 50 rounds, an unplug and replug) passed throughout.

**What it costs, measured in that run.** On a respawned instance nearly every received transfer goes
through the recovery: `dwc2`'s heartbeat read `1044 transfers ... 987 taken as answers (OP_SYNC)`. Each one
is an extra call to `dwc2`, and the notice is re-sent only after `wifi-usb` has been quiet for 5 ms. Beacons
and a ping do not notice; sustained receive would. The instance with a mailbox showed 0.

**What would remove it:** the mailbox for a respawn. A watched service's respawn could take back the slot
its dead instance just released, instead of competing with the reserve as a new service would. That is
option 1 or 2 above applied to respawns only, and a kernel change, so not made without the operator's
go-ahead and a QEMU boot first.

## Option 4, done (2026-10-06, operator's go-ahead): a respawn takes its mailbox back

**The rule.** When a WATCHED task (`SPAWN_FLAG_WATCHED`, what the supervisor restarts) dies holding a
reply mailbox, the death path banks one credit (`routing::MAILBOX_CREDITS`). A watched spawn the reserve
would refuse may spend one, and is granted the mailbox past the reserve. The credit is spent only on a
registration that happens; a table that filled meanwhile refunds it.

**Why this keeps what the reserve protects.** A mailbox is now granted either above the reserve, as
before, or in place of one a watched task released. The reserve exists so convenience endpoints cannot
starve the mandatory ones in the probe builds (P5, P7). Probes are not watched, so they neither bank nor
spend, and the footprint a credit restores is one boot already granted. **Not keyed on a name:** the
kernel learns only that one watched mailbox was given back and one is being asked for, not which service
is which.

**And a floor under it, because credits are pooled.** A credit is not tied to the task that banked it, so
an unusual order (watched deaths freeing slots, unwatched spawns taking mailboxes above the reserve in that
window, the respawns then spending their credits) could have mailboxes hold more slots past the reserve
than boot granted. "It balances in practice" is weaker than what the reserve promises, so a credit is never
spent when 12 or fewer slots are free (`CREDIT_FLOOR`, an eighth of the table). Those are kept for
mandatory receive endpoints, whatever the credits say, and the grant falls back to the refusal.

**What it does not change.** A service refused at boot (`nic-driver`, `net-stack` on these images)
never held one and banks nothing. On a respawn it may still take a pooled credit another watched task
released, so whether it gets one depends on the order of deaths and spawns. That is the rest of this item. The
reserve itself is untouched.

**Said, like the refusal:** `spawn[ipc]: '<name>' takes back a reply mailbox released by a service that
died - N of 96 routing slots free, reserve 72`. (It first read "a dead watched task released", which was
wrong once the supervisor could take one too; the runs quoted below predate the rewording.)

**Verified in QEMU (Pi 2, 2026-10-06):** at boot `nic-driver` and `net-stack` were refused as before. Then
`kill time`, and `time`'s respawn at 71 free took the mailbox back, where before it would have been
refused. The x86 identity suite, then the Pi 2 card for `wifi-usb`, follow (`docs/wifi-usb.md` 20).

## Option 4 on hardware (Pi 2, 2026-10-06): it holds

The operator's run: `wifi radio off` and `on`, `off hard` and `on`, a `powercycle`, then `chaos max-carnage`
50 rounds (351 kills, kernel alive). **167 grants were taken back** past the reserve, and **no service was
refused its mandatory endpoint**: no `spawn REFUSED - IPC routing table full` anywhere. Every `wifi-usb`
restart took a mailbox back, and `dwc2` read `0 taken as answers (OP_SYNC)` throughout, where the run
before it read 987 of 1044.

**Two things it showed:**
- **Credits are pooled, as designed, so they also reached `nic-driver` and `net-stack`.** Both were refused
  at boot, banked nothing, and during chaos took mailboxes other watched tasks had released. That is the
  footprint conserved, not grown: a grant spends exactly one release. But it means which service holds a
  mailbox after a storm is the order of respawns, not who held one at boot. A service that had one at boot
  can find the credits gone. `wifi-usb` did not, in 7 restarts.
- **The supervisor was refused 13 times** (`gets no reply mailbox`), once per kernel respawn. Credits
  went only to WATCHED tasks, and the supervisor is never watched. "Watched" is a flag the supervisor
  sets on the services it restarts, and it means "tell the supervisor when this one dies". The
  supervisor is restarted by the kernel instead, and telling it of its own death would be telling the
  dead. So it neither banked nor spent. Nothing failed for it, and that was equally true before this
  change: the boot supervisor had a mailbox and every respawn did not.

## The supervisor too (2026-10-06, operator's go-ahead)

**The rule now reads "watched, or the supervisor".** The death of either, holding a mailbox, banks a
credit, and the spawn of either may spend one (`may_take_back` in `task::spawn`, and the death path in
`scheduler.rs`). The supervisor is NOT marked watched to get this, because that flag would also send its
death notice to its own dead endpoint and log a false "death UNHEARD" at every respawn. It is named
instead, as the one service the kernel knows by name. The restart counter in the same death path already
names it the same way, for the same reason.

**What the kernel restarts, stated plainly because an earlier note blurred it:** the supervisor, and
nothing else. Every other service is restarted by the supervisor; the kernel's part in that is only to
report the death of a task the supervisor asked about (`SPAWN_FLAG_WATCHED`). "Watched" is that request,
made by the supervisor in its own spawn requests. The supervisor is not watched because nothing above it
is asked to restart it: the kernel does that itself (CLAUDE.md 6.2), and it is the one service the
kernel knows by name.

**Verified in QEMU (Pi 2):** `kill supervisor` - the kernel's respawn logged `'supervisor' takes back a
reply mailbox ... 71 of 96 routing slots free`, where the same boot without this change logged `gets no
reply mailbox` at the same count. The respawned supervisor adopted every running service as before.

**On hardware (Pi 2, 2026-10-06, b2a54655): the supervisor is never refused now.** The operator's run:
`kill supervisor`, cable plugged and unplugged repeatedly under `ping`, then `chaos max-carnage` 50 rounds
(344 kills, kernel alive). The kernel respawned the supervisor 21 times. 16 of them logged `'supervisor'
takes back a reply mailbox released by a service that died`, and **none** logged `gets no reply mailbox`.
The other 5 were granted above the reserve, where the table had room, which is not logged. Across the
run 199 mailboxes were taken back. The only refusals were at boot (`nic-driver`, `net-stack`) and
`chaos` itself, which is not watched and is not restarted. No `spawn REFUSED - IPC routing table full`
anywhere. The cable and the radio handed the link back and forth on every plug (`the cable carries the
link; the radio stands by` / `the cable is out - the radio carries the link`), and the dongle rejoined
after chaos.
