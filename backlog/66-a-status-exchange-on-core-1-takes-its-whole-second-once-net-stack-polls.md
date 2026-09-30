# 66. A STATUS exchange between net-stack and nic-driver takes its whole second once net-stack is polling, and two fixes that should have touched it did not

**Status: OPEN - parked 2026-09-30 by the operator ("park this ping lag a bit"), with everything measured so far below so the next person does not re-derive it.**
**Found:** 2026-09-30 on the Pi 4, as `ping` over the radio running at one echo every three seconds.

## The measurement, which is exact

After `time` sets the wall clock from the network, `net-stack` leaves its blocking `recv` for its
polling mode (`poll_step` plus `recv_timeout(POLL_MS)`). From that moment every STATUS query (op 3) it
makes to `nic-driver` takes 890-990 ms, which is its one-second deadline, and a ping makes three of them:

```
net-stack: the NIC exchange for op 3 took 979 ms (answered) - sent at 59257 ms, answered at 60236 ms
nic-driver: serving STATUS #50 at 54238 ms      (net-stack had sent that one at 53286 ms)
```

Both stamps are the same clock, cycles since boot in milliseconds. `nic-driver` begins serving the
request ~950 ms after it was sent and answers 11 ms later, and every serve lands on a one-second grid
(`...227` to `...246` ms). Before the clock is set the same exchanges take milliseconds and the ping runs at
one echo a second. The 16:32 boot shows the switch happening at the instant of `time: wall clock set`.

## What is ruled out, each by a boot

- **The radio.** `nic-driver`'s own query to `wifi-driver` logs `answered 0x10 in 0 ms` throughout, no
  mismatched reply was ever recorded, and the stall is between two services on core 1.
- **The IPC change (`backlog/65`).** A bisect image with only that change reverted (16:49 boot) showed
  the identical cadence.
- **The idle lost-wakeup window** (`183088a1`'s class; the aarch64 port answered "no" to that fix for a
  reason that has since gone). Closing it on the Pi 4 (`ce36b929`: mask, re-check, `wfi`, unmask) changed
  nothing measurable (17:36 boot): same 950 ms, same grid. The change is kept because the window it
  closes is real and documented, but it is not this.
- **A clock-unit error.** The shell's own timing of an echo (2066 ms) and net-stack's (979 ms per
  STATUS, three per echo) agree with each other and with the wall clock in the serial stamps.

## What is not yet known

Where core 1 is for the ~950 ms between `net-stack`'s send and `nic-driver`'s wake. The one-second grid
says a periodic event releases it, and the only one-second events in the machine are the idle tick of a
secondary core and the epoch-second boundary that `net-stack`'s deadline loop measures against
(`epoch_secs_monotonic() - t0 >= max_secs`). The stalls began exactly when `net-stack` started blocking
and waking every 20-100 ms, which is what changed the shape of core 1's scheduling.

## The next measurement, not the next theory

A kernel-side stamp, because both userspace ends are already stamped and agree: record, per task, the
tick at which `wake_by_slot` set it Ready and the tick at which the scheduler first ran it, and expose the
last such gap through `InspectKernel` (or print it once when it exceeds 100 ms). That says whether
`nic-driver` was Ready for a second and not picked (a `pick_next` / run-queue question) or was never
woken (a `blocked_receiver` / enqueue question). A second, cheaper check needing no kernel change: move
`nic-driver` and `net-stack` to core 0 by their spawn rows for one boot - core 0 never takes the slow
idle path - and see whether the grid disappears.
