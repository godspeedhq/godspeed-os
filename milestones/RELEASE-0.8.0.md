# v0.8.0 - the machine notices what you unplug

*Prepared release note. Use as the annotated tag message at merge:*
`git tag -a v0.8.0 -F milestones/RELEASE-0.8.0.md`

---

v0.8.0 - the machine notices what you unplug

A device pulled out of a Raspberry Pi 2 is now seen, said, and recovered from.
Keyboard, USB storage and an unrecognised dongle each report their arrival and
departure by name; the ethernet cable reports itself; and a device replugged into
the same socket is detected, which the previous level-sampling design could not
see at all. The Pi also keeps a wall clock over SNTP and can reboot itself.

Minor rather than patch, and NOT arm32-only:

- The fs request/reply protocol gains a correlation tag in both directions, so a
  reply is matched to its request by identity instead of arrival order. This is a
  wire-format change: every fs client must be rebuilt in lockstep.
- x86 gains a real kernel fix - the BSP could halt onto a consumed one-shot TSC
  deadline and never wake, panicking the machine seconds after it first went
  idle. Latent for the life of the port.
- The liveness watchdog is newly armed on ARM (it was gated on a stubbed quantum
  figure, so the port had run with no wedge detection at all). A previously
  silent freeze now panics loudly, naming the core and its last task.
- The kill path no longer frees a driver's device MMIO as if it were RAM.
- `chaos max-carnage` is paced by the clock rather than a yield count, so a round
  is ~1 s on every machine; x86 runs are correspondingly longer than before.

Hardware-validated on the Raspberry Pi 2 (1M-round chaos soak in progress past
55K, 1000 rounds and 3x100 rounds clean, selfcheck 349/0) and on the HP T630
(identity 24/24, file-cap 10/10, fs-restart 11/11, reply-dead 5/5, chaos 7/7,
networking and idle confirmed on real silicon).
