# 15. The NIC receive ring is only drained when somebody asks

**Status:** open, recorded rather than closed (§26.7). Not a regression - it was always true, and it
became visible only once the packet loss ahead of it was fixed.

## What happens

`nic-driver` harvests a receive descriptor only inside the drain request that net-stack sends it. Its
serve loop otherwise blocks in `ctx.recv()`. So between operations - which is most of the time, and
all of the time during anything that is not networking - nothing empties the ring. After `RX_DESCS`
frames the engine has no descriptor left, raises `RBU`, and the MAC drops everything that follows
until somebody asks again.

Measured on the VisionFive 2 during a `selfcheck` run, with no network activity of our own:

```
MAC rx 272 ... handed 266     healthy
MAC rx 434 ... handed 285     162 frames arrived, 19 handed on
```

Nothing wanted those frames, so nothing broke. The same gap would drop a frame we did want - a DHCP
renewal, an ARP for us, a reply arriving a moment before the client starts listening.

## Why the obvious fixes are wrong

- **A bigger ring** buys proportionally more slack and does not change the shape. It is worth taking
  where it is free (24 descriptors is what the granted 64 KiB arena affords), and it is not a fix.
- **Discarding while idle** - having the driver harvest and drop on a timer - would keep `RBU` at zero
  and make things WORSE: the ring is currently also the buffer that lets a slow client collect frames
  that arrived before it asked. Removing that to flatter a counter is the wrong trade.

## What the fix actually needs

A driver that harvests without being asked needs somewhere to put frames nobody has requested yet: a
bounded software queue in the service, filled from a timed receive (`RecvTimeout`, no new kernel
surface) or from a device interrupt. `nic-driver` only ever serves - it makes no requests of its own -
so unlike net-stack it has no reply-stealing hazard and a timed receive is safe here.

The Pi 2 reached the same conclusion by a different route: coverage, not ring size, and the answer
there was interrupt-driven receive (`project_arm_networking_hw`, 85% loss to 4%).
