# 74. On every board image the last services spawned get no reply mailbox, and `net-stack` is always one of them

**Status: OPEN - found 2026-10-04 from a VisionFive boot, confirmed in a Pi 4 log. Option 3 (every refusal
named) is DONE, below; the reserve itself is unchanged until the measurement this item names has been made.**

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
mailboxes says otherwise.

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
