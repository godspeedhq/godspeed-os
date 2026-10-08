# 66. A STATUS exchange between net-stack and nic-driver takes its whole second once net-stack is polling, and two fixes that should have touched it did not

**Status: FIXED 2026-10-08 (`e3fcf7ed`) - an empty reply was refused by the kernel on every port but ARM32 (the last entry below). Verified on the VisionFive over WiFi and over the cable; the Pi 4 has not yet run the fixed build. Was: PARKED 2026-09-30 by the operator, for the second time, after the boot that ran well.**
**Found:** 2026-09-30 on the Pi 4, as `ping` over the radio running at one echo every three seconds.

**2026-10-02, the symptom is mostly gone, and nobody fixed it on purpose.** The boot of commit `5dd1f1b8`
(the clock out of `net-stack`, which `time` now asks through op 12) pinged 8.8.8.8 over the radio at one
echo a SECOND, 5 of 5 and 19 of 19, 0% loss. Five STATUS exchanges were still slow - 303, 335, 496, 658
and 711 ms, against 890-990 before - and none landed on the one-second grid. The same boot logged
`nic-driver: a reply send FAILED - the reply cap is dead` twice, which is `backlog/67`'s signature, and
67 is fixed in the SDK since, and the Pi 4 boot after the fix logged no such line through a 50-round chaos run. What changed the timing is not shown; the clock leaving
`net-stack`'s serve loop is the obvious candidate and is a guess. Left parked, with the new numbers.

**2026-10-04, gone on the current tree, and the mailboxes are not why.** `backlog/74` found that
`nic-driver` and `net-stack` never had reply mailboxes on the radio boards and tested it as a cause: two
Pi 4 boots over the radio (cable pulled), 20 echoes to 8.8.8.8 each, one with the mailboxes granted by a
test image and one on the committed build (`5f501775`) without them. Both 20/20, and NEITHER logged a
STATUS exchange of 300 ms or more; the VisionFive's cable path gave the same pair of answers. So on this
tree the tax does not appear at all, with or without the mailboxes. Still parked rather than closed: no
change has been shown to have removed it, and the mechanism above was never explained.

## The measurement, which is exact

After `time` sets the wall clock from the network, `net-stack` leaves its blocking `recv` for its
polling mode (`poll_step` plus `recv_timeout(POLL_MS)`). From that moment every STATUS query (op 3) it
makes to `nic-driver` takes 890-990 ms, which is its one-second deadline, and a ping makes three of them
(Corrected below, 2026-10-01: polling starts when the network is configured; the clock being set was a
correlate.):

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

### Two causes, and a correction to this item's own reading (2026-10-01)

**The lag had two causes, and this item measured one of them.** A causal trace of boot 2026-10-01
11:17-11:18 (five readers over the services involved, three adversarial adjudicators) separated them:

- **The multi-second spikes** - `ping echo 2 was answered after 7092 ms` with a wire round trip of 39 ms -
  were `net-stack` running SNTP inside its single serve loop while the clock was unset: at the end of the
  link-up dance, on `time`'s nudge every 20 s, and on ordinary client requests. Each exchange held every
  client for up to about fifteen seconds when the server or resolver was silent. `date sync` ended them
  only because its success latched the clock and quietened the nudges. FIXED IN CODE by taking SNTP out
  of the loop (`docs/networking.md` 16): x86 shell suite 215/0, a Pi 4 QEMU boot, and on the Pi 4 the boot
  of `5dd1f1b8` (2026-10-02, above: one echo a second, 19 of 19). That boot does not record whether its
  clock was unset, so the closing condition is still unproven: closed when a Pi 4 boot with an UNSET
  clock shows no multi-second echo.
- **The steady one-second tax** is what the slot log above measured, and it stands: the first send of
  each exchange does not wake `nic-driver`. Still open, still parked, still the kernel path named above.

**A correction to "after `time` sets the wall clock, net-stack leaves its blocking `recv` for its polling
mode".** The poll gate is `gw_known && tcpst.have_clock()`, and `have_clock` is `net-stack`'s cycle-counter
calibration, taken once at startup against the kernel's monotonic seconds - on the Pi 4 those are
`secs_since_boot()`, which advance from power-on whatever `time` has set. So polling begins when the
network is CONFIGURED. It coincided with the clock being set because the dance that configured the
network used to end in an SNTP exchange: the clock was a correlate of the onset, not its cause. The
index row is corrected to match.

**2026-10-07, seen again, once.** A Pi 4 `selfcheck` over the onboard radio (cable out) failed only its
DNS check: `net-stack`'s ops 4 and 0 to `nic-driver` went unanswered for about 2 s each, and
`nic-driver`'s replies arrived after `net-stack` had given up (`a reply send FAILED - the reply cap is
dead`), with no slow or unanswered radio exchange logged in that window. Pings in the same session ran
at 43-52 ms. Recorded as a sighting; the item stays parked (`docs/wifi-usb.md` 49).

**2026-10-07, later the same day, and reproducible.** On the current image the Pi 4's DNS check failed in
two `selfcheck`s running, both over the radio, with `nic-driver` answering `net-stack` about 2 s late
while every radio exchange it made was answered within 2 ms. With the cable in, every lookup resolved. So
the late answer is on the radio-bridged path on core 1 (which `nic-driver` shares with `net-stack`,
`block-driver` and `fs`), and DNS is where it shows because a lookup is one request and one reply with
nothing to retry it but `net-stack`'s own bound. Still parked; `docs/wifi-usb.md` 49 has the split.

**2026-10-07, the instrument this item named, built.** `kernel/src/task/scheduler.rs`: `wake_by_slot` stamps
the BSP tick when it moves a task out of a BLOCKED state, and every switch to a task
(`core_release_current`) checks the stamp - `sched: '<task>' ran N ms after a wake made it Ready (core C,
which halted in idle H time(s) meanwhile)` for 100 ms or more. Beside it, the direct test: when
`pick_next` finds nothing and a task on that core has been Ready since a wake 100 ms or more ago, `sched:
core C going idle with '<task>' Ready on it ... - pick_next did not return it`. The first QEMU boots taught
two corrections before it could be trusted: a wake that reaches a task still Running leaves no switch to
clear the stamp (now stamped only out of a blocked state), and a wake landing between `pick_next`'s
answer and the idle check is benign (now filtered by age). In QEMU one boot in three showed `events`
Ready 2 s after a wake on an idle core, with no idle-with-Ready line - not yet explained, and not yet
seen on the hardware. The Pi 4 card: cable out, `net dns google.com` until it fails, and read which
line, if either, comes with the late answer.

**2026-10-07, the Pi 2 rules out everything but the Pi 4.** On the Pi 2, the dongle carrying the link
through the same shared bridge (`radio.rs`) on the same guest network and DNS server, every lookup
resolved and `selfcheck`'s DNS check passed. What differs is the board: the Pi 4's AArch64 kernel, and
`nic-driver` pinned to core 1 with `net-stack`, `block-driver` and `fs` (the Pi 2's is unpinned).

**2026-10-08, FOUND AND FIXED: an empty message was refused by the kernel on three ports of four.** The
one-question instruments above each ruled out one explanation, so they were replaced by a flight recorder
in the kernel: every send, receive, block, wake, switch and cap-table change for `net-stack`,
`nic-driver` and `wifi-driver`, in a ring printed when `net-stack` had asked `nic-driver` and heard nothing
for 300 ms. On the VisionFive, cable out, it showed the whole exchange: `net-stack` asks for a frame (op 4)
with a correct reply cap, `nic-driver` asks `wifi-driver`, which answers "none waiting" in 0.2 ms, and
`nic-driver` answers `net-stack` with an EMPTY message - which never reached the queue.

Each arch's user-range check (`validate_user_ptr`) refused a zero-length range on x86, AArch64 and RISC-V
and accepted it on ARM32, so a zero-length send failed on three ports and worked on the Pi 2 - which is
why the Pi 2 resolved over the same dongle, network and DNS server. `nic-driver` counted it (`reply send
FAILED`); `net-stack` waited out its second and retried into the next empty drain. Over the radio most
drains are empty while an answer is in flight, so DNS ran out of time; over the cable fewer are, which
was the ~900 ms first-try miss seen there. The receive side had the same fault: an empty payload already
taken off the queue failed its copy-out.

`e3fcf7ed` makes an empty message a message on every port, in neutral code: `build_message` reads
nothing for nothing, and the receive copy-outs succeed on an empty payload. VisionFive, the same day:
twenty `net dns` lookups over WiFi, each answered first time, no failed reply, no flight dump; and with
the cable in. The instruments came out in `b77c7dd5`, and `osdev test shell` no longer passes a `net
dns` that `net-stack` did not answer - accepting that is how x86 hid it. Two readings made on the way
were wrong and are recorded as such: that `net-stack`'s self-grant was being replaced, and that replies
were addressed to another endpoint. The recorder showed neither.
