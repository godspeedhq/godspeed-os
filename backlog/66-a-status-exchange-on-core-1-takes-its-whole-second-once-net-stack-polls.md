# 66. A STATUS exchange between net-stack and nic-driver takes its whole second once net-stack is polling, and two fixes that should have touched it did not

**Status: OPEN - PARKED 2026-09-30 by the operator, for the second time, after the boot that ran well. The mechanism is measured to the point of naming the kernel path (below, "What the slot log said"); the fix is not attempted. The card carries the build that ran well (`kernel8.img` sha `e319dae6`), which is this tree.**
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
- **Core 1, and the secondary-core idle path with it.** The cheap check named below was run (18:20
  boot, an uncommitted image with `nic-driver` and `net-stack` placed on core 0 by their spawn rows,
  the source restored afterwards): identical. `took 990 ms (answered)` on the second boundary, one echo
  every three seconds, and 130 exchanges running back to back with no ping in flight. Core 0 never
  takes the secondary idle path, so whatever holds the exchange is not a property of which core runs
  it. The title of this item names core 1 because that is where it was found; the fault is not there.

## What is not yet known

Where core 1 is for the ~950 ms between `net-stack`'s send and `nic-driver`'s wake. The one-second grid
says a periodic event releases it, and the only one-second events in the machine are the idle tick of a
secondary core and the epoch-second boundary that `net-stack`'s deadline loop measures against
(`epoch_secs_monotonic() - t0 >= max_secs`). The stalls began exactly when `net-stack` started blocking
and waking every 20-100 ms, which is what changed the shape of core 1's scheduling.

## The next measurement, not the next theory

The exchanges run back to back even with nothing to ping, which says the wait itself is what takes a
second, not the arrival of work. `net-stack` waits in the SDK's sifted deadline loop: a 20 ms
`await_slice`, a check of `epoch_secs_monotonic() - t0 >= max_secs`, a `yield_cpu`, again. The stamps at
either end cannot say whether each of those 20 ms slices BLOCKS for 20 ms or returns at once and the
loop spins until the second boundary trips the check - and those two give different answers to where
the time goes. So the loop is instrumented (`sdk/rust/src/service_context.rs`,
`request_with_reply_deadline_sifted_inner`): a wait over 500 ms logs its slice count and the shortest
and longest slice. Twenty-odd slices near 20 ms each say the slices block and the reply arrives late;
hundreds of slices near 0 ms say `recv_timeout` is returning immediately and the second is being spun
away. Either result names the next file.

### What the first slow boot with the instrument said (20:20, cable out from power-on)

**The instrument never fired, and that is the finding.** Every `op 3 took 960 ms (answered)` line
came with no `sifted wait took` line, so no answered wait reached 500 ms. The 960 ms is not spent
waiting for a reply that arrives late; it is spent somewhere the answered path does not measure. The
remaining places are the FIRST attempt timing out (the loop's timeout branch was not instrumented, and
`nic_req_inner` reacquires and retries in silence after it) and the send itself. A timeout at the
epoch-second boundary followed by an immediate retry fits the numbers exactly: a cycle that ends a few
ms after one boundary sends again at once and times out at the next, 960-990 ms later, and the retry
is answered in 10 ms. It also fits nic-driver's only complaint, `a reply send FAILED ... queue full or
peer dead`, logged at the moment the slow phase began - a reply to a request whose cap the requester
had already reclaimed.

**The slow phase is one serve pass.** `a serve pass took 127989 ms` closed at 20:23:51, which puts its
start at 20:21:43 - the second the clock was set by `time`'s nudge. For those 128 s net-stack sat
inside its stash-drain cycle: a client request (op 0, patience 1500 ms) is held because an exchange is
in progress, served after ~980 ms, serving it issues another op 3 to nic-driver, and the next op 0
arrives 16 ms later to be held behind that one. Nobody was pinging for part of it. Neither `date sync`
that set the clock again broke the cycle; the third one did, and the pass ended.

**Sequence, for the record.** Slow with no clock (first ping, 35 echoes in 90 s); still slow after a
`date sync` that set the clock (11 in 33 s); fast after a second `date sync` (53 in 52 s). A synced
clock is therefore not the switch, though every fast boot today had one and the slow ones did not at
the start. Instruments added for the next boot: the timeout branch reports its slices, net-stack names
a first attempt that failed and a retry that answered, and nic-driver says WHY a reply send failed
(full queue, or a reply cap the requester already reclaimed) with a running count.

### What the slot log said (22:09 boot) - the finding this item parks on

An instrument that logged every STATUS nic-driver served, with the cap slot it answered through and the
result, alongside net-stack's send stamp per timed-out attempt. **Every timed-out first attempt was
RECEIVED by nic-driver, about a second late, and served back to back with the retry**: STATUS #17 and
#18 twelve ms apart, #19 and #20, #21 and #22, #23 and #24, every pair. Not one of those answers failed
to send. nic-driver blocks in a plain `recv()`, so the first send should have run it. It did not; the
SECOND send - net-stack's retry after reacquiring - did, and nic-driver then served both.

So the second is spent in the kernel between `send` and the receiver running, on the same core, with
the receiver recorded as blocked. That is a LOST WAKE-UP in the blocked-receiver path (`enqueue_locked`
takes the receiver, `wake_by_slot` follows), the family the x86 work of August found twice
(`fd5578dc`), on a port whose idle window was closed separately (`ce36b929`) and measured not to be
this. It is not why the lag STARTS - the onset each time was a reply into a dead cap (`backlog/67`) -
but it is why every cycle after the onset costs a second. Not fixed here, because it is a kernel path
on a battle-tested port and the operator parked the item.

**Two userspace fixes went in on the way and stay, each correct on its own reading:**
- `nic-driver` (GENET): the radio wait sifts. A message with a reply cap is a client's request, never
  the radio's answer; it is kept (two slots) and served before the next `recv`. It was the one place a
  request to nic-driver could be silently discarded, measured at one rescue per run.
- the observe column (unrelated, same session).

**Tried and reverted, recorded for the next attempt:** sifting the three unsifted waits in net-stack's
address dance (`dns_resolve`'s ARP answer and RX poll, `ping`'s ARP ack). Correct by reading - each
can take a client's request as the driver's answer and leave its cap in the kernel's pending-cap FIFO -
but the boot that carried it was no better, and the operator asked for the build that ran well. The
patch is `build/netstack_sift_dance.py` and applies cleanly to this tree.

Still available, and needing a kernel change: record, per task, the tick at
which `wake_by_slot` set it Ready and the tick at which the scheduler first ran it, and expose the last
such gap through `InspectKernel` (or print it once when it exceeds 100 ms). That says whether
`nic-driver` was Ready for a second and not picked (a `pick_next` / run-queue question) or was never
woken (a `blocked_receiver` / enqueue question). The cheaper check this paragraph used to end with -
place both services on core 0 for one boot - has been run and is recorded above.
