# 88. `nic-driver` probes a dead `wifi-usb` capability once a second, and the kernel logs every probe

**Status: OPEN, 2026-10-13.** Seen on the Pi 4 card run of 2026-10-13 (`feat/ask`, the hub-cut image,
`build/serial_output.log`). Nothing failed because of it; the cost is a kernel log line a second for as
long as the dongle is gone, which on a Pi 4 with no dongle plugged in is normal operation.

## Evidence

`wifi-usb` was stopped when its dongle stopped answering:

```
08:32:30.056  supervisor: wifi-usb stopped - its USB device 0bda:8176 is not attached
08:32:30.072  supervisor: wifi-usb ended - not restarted, its USB device is not attached
08:32:30.134  cap::get: ResourceId(127) gen mismatch cap=31 rec=32 liveness=Dead
```

The mismatch line started 62 ms later and repeated 271 times, to the end of the log (08:37:21), at
about 54 a minute once settled. In the same window `nic-driver`'s radio bridge, which had moved to
`wifi-usb` at 08:31:43 ("the radio bridge now goes to wifi-usb (the radio in use, /wifi.radio)"), says
it is probing a radio that is not running:

```
08:32:30.384  nic-driver: the radio was silent and its name does not resolve - wifi-usb is not running (x1)
08:32:46.937  nic-driver: the radio was silent and its name does not resolve - wifi-usb is not running (x16)
...
08:35:44.312  nic-driver: the radio was silent and its name does not resolve - wifi-usb is not running (x176)
```

16 probes every ~17.7 s is 0.9 a second, the rate of the mismatch lines.

## The mechanism, from the code (`services/nic-driver/src/radio.rs`, `Radio::rpc`)

After `RADIO_REACQUIRE_AFTER` (3) silent requests the bridge holds the radio down for
`RADIO_BACKOFF_MS` (1 s) and then probes again, reacquiring the cap by name on every failed probe. When
the name does not resolve, `gs::cap::reacquire` fails and the bridge keeps the cap it had: the one to
the dead instance. The next probe is sent on it, the kernel's capability check finds the resource dead
one generation on and says so, and the bridge counts another unanswered probe. Once a second, for as
long as the dongle is absent.

## What is ruled out, and what is not established

- **Not a service failing to notice.** The bridge knows the radio is not running and says so, every
  sixteenth probe; the probe itself is the documented recovery (a respawned radio is noticed within a
  second).
- **Not established: that `nic-driver` is the sender.** The kernel's line names the resource and the
  two generations, not the task that used the cap (the same gap as `backlog/86`). The rate and the
  start time match `nic-driver`'s probes, and no other service sends to `wifi-usb` periodically, but it
  is an inference.
- **Not established: that resource 127 is `wifi-usb`'s endpoint.** Inferred from the line starting
  62 ms after `wifi-usb` ended.

## Next step

Service-local, no kernel change: when the reacquire fails because the name does not resolve, the
bridge removes the dead cap and, until the name resolves again, asks the kernel directory once a second
instead of sending. A radio that comes back is still noticed within a second, and nothing is sent on a
capability known to be dead, so the kernel has nothing to log. Verifying it needs a board with the
dongle (`wifi-usb` does not run in QEMU): unplug the dongle and the mismatch line must not appear,
replug it and the bridge must reach the new instance.

Naming the sender in the kernel's mismatch line would settle the inference here and in `backlog/86`
with one change; that is a kernel change and is recorded there rather than proposed twice.
