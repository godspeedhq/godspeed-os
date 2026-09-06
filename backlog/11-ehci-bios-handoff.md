# 11. `ehci` resets a controller the BIOS still owns - no USBLEGSUP handoff

**Severity:** latent defect on EVERY machine; FATAL on a single core.
**Root-caused:** 2026-09-06, T630, by instrumenting the fatal window after four theories failed.

## The evidence, from inside the window

Step logging around the halt/reset sequence gives the exact last instruction:

```
ehci: [1] about to READ USBCMD
ehci: [2] USBCMD=0x00080b11; about to READ USBSTS
ehci: [3] USBSTS=0x00004008; about to WRITE USBCMD (clear RS)
ehci: [4] RS cleared; waiting for HCHalted
ehci: [5] halted; about to WRITE HCRESET      <- LAST LINE. [6] never prints.
```

The `HCRESET` write is what kills the machine. And the registers say why:

| bit | value | meaning |
|---|---|---|
| `USBCMD.RS` | **1** | the BIOS left the controller RUNNING |
| `USBCMD.PSE` | **1** | periodic schedule ENABLED |
| `USBSTS.HCHalted` | **0** | not halted on arrival |
| `USBSTS.PSS` | **1** | periodic schedule actively RUNNING |
| `HCCPARAMS.EECP` | **0xa0** | a USBLEGSUP capability EXISTS - BIOS ownership is declared |

So on arrival the firmware is actively driving this controller: that is legacy USB keyboard
emulation, polling the keyboard through the periodic schedule. And this driver resets it without
ever asking for ownership - its own comment calls the handoff "E2b", future work.

## Why one core dies and four do not

An **SMI is handled on the core that triggered it.** Resetting a controller the firmware is using
raises one. With four cores the OS keeps running on the other three while the firmware deals with
having its controller pulled out from under it; with one core there is nowhere else to run, and the
platform goes down. Same defect on both - only the single-core case has no slack to absorb it.

This also explains the earlier confusion cleanly: removing `ehci` made single-core pass 459/0 through
100 chaos rounds, because nothing then touched the BIOS-owned controller.

## The fix, and why it is not a small change

EHCI 2.1.7 / the EHCI Extended Capability at `EECP`: set the **HC OS Owned** semaphore, wait for
**HC BIOS Owned** to clear, then optionally disable SMI generation in `USBLEGCTLSTS`. Both live in
**PCI CONFIGURATION SPACE**, not MMIO.

**There is no PCI config WRITE in this system, deliberately.** `pci_cfg_read` exists, gated by
`PCI_CFG`; the design states the rule plainly in `hw-enumerator`: *"read ONE config register... No
write. Ever."* And `ehci` holds only `CONSOLE_PUSH`, so it cannot even READ the capability today.

So the correct fix needs a new, gated PCI-config-write capability - new kernel surface, against a
rule the project set on purpose. That is a decision, not an implementation detail.

## Options, honestly

1. **Implement the handoff** with a narrowly-gated config-write capability (ideally: write ONLY the
   USBLEGSUP OS-Owned bit, for a BDF the caller was already granted). Correct per spec, fixes a
   latent defect present on every machine, and is the only route to owning this controller properly.
   Costs kernel surface and a Commandment I argument.
2. **Refuse to reset a controller that arrives RUNNING**, log loudly, idle. No new surface, stops the
   reboot. But it likely disables the back-panel USB ports on machines where the firmware holds the
   controller - including multi-core, where they work today. Trading working hardware for a
   configuration nobody ships is a bad trade.
3. **Leave it**, with single-core-plus-`ehci` recorded as unsupported on this machine.

Option 2 is the only one that is free, and it is the one that costs functionality. There is no cheap
correct answer here, which is why this is written down rather than guessed at.
