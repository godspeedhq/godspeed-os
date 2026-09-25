# 50 - `nic-driver` read 100% CPU on the T630

**Opened:** 2026-09-24
**Status:** OPEN - one observation, on one machine, not root-caused and not reproduced.
**Observed by:** the operator, on the HP T630, during the `feat/stdlib` hardware pass.

## What was seen

`nic-driver` reading 100% CPU on the T630, noticed after a run of network tests - `net`, `sock`,
`tcp` to two addresses, and `serve 8080` accepting three connections from a machine on the LAN.

That is the whole of the observation. It was not timed, not correlated with a particular command,
and not watched to see whether it settled.

## What is ruled out

**Not this branch.** `services/nic-driver/` is byte-identical to `main` - `git diff main...HEAD --
services/nic-driver/` is empty. `feat/stdlib` did change `services/net-stack` (48 lines, the badged
correlation tag and the client-patience byte), and net-stack is the only thing that talks to
nic-driver, so "we did not edit it" is not on its own an alibi. But that diff touches no yield, no
sleep, no poll interval and no loop structure: grepping the diff for any of those returns nothing.
The poll RATE is exactly what it was.

**Not visible at idle in QEMU on this branch.** Booted, left to settle for 25 s after the boot burst,
then sampled `status` twice six seconds apart:

```
10    nic-driver     1     BlockRecv    299008
11    net-stack      1     BlockRecv    360448
```

`BlockRecv` both times - blocked in `ctx.recv()`, which is what the driver's main loop does when
nobody is asking it anything. So whatever was seen on the T630 is not the steady state of an idle
machine on this code.

## Three candidates, none eliminated

1. **It was transient.** The driver spins with `ctx.yield_cpu()` while WAITING ON THE DEVICE - the
   RTL tally-counter dump (`RTL_DTCCR`), the descriptor-ownership waits, the RX frame poll. Those
   burn a core for as long as a request takes, so a machine mid-test looks busy by design. The
   observation followed a run of network commands.
2. **It is RTL-specific.** The T630 has an RTL8168; the QEMU measurement above used an e1000. They
   are different code paths in the same driver, and only one of them was measured.
3. **It is the instrument.** The per-tick CPU% is a SAMPLER and is known in this project to resonate
   with the 10 ms poll - a service that wakes on the tick can read high while doing very little. This
   has produced wrong readings here before.

## What was NOT checked, and matters

**The other four machines.** Dell Wyse, Raspberry Pi 2, Raspberry Pi 4 and VisionFive 2 were not
looked at. Three of them have different NICs entirely (smsc95xx, GENET, dwmac), so if the reading is
RTL-specific they would not show it, and if it is the sampler they all would. One machine is not a
pattern.

## The cheapest next step

Not a bisect. Three questions, in order, each of which can be answered in a minute at a prompt:

1. With the network quiet for a few minutes, is it STILL high - or did it settle after the tests?
2. What does `status` say for it - `BlockRecv` or `Running`? That is the scheduler's own answer and
   is not sampled. `status` saying `BlockRecv` while `observe` says 100% settles candidate 3 on the
   spot.
3. Is `net-stack` high too? Both means the pair is polling each other; only nic-driver means
   something else is asking it.

If it survives those, it belongs with the power-efficiency work, which already records the USB
drivers busy-spinning at 100% and lists tickless idle as the unfinished half.
