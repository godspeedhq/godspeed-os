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

## FIXED - the handoff already existed, and was never called

`kernel/src/arch/x86_64/pci::ehci_bios_handoff()` implements the whole EHCI 2.1.7 procedure: claim
the OS-Owned semaphore in USBLEGSUP, wait (time-bounded) for the firmware to release BIOS-Owned,
report whether it did, and **disable firmware SMIs on the controller** (`USBLEGCTLSTS = 0`). That
last step is exactly what makes a later HCRESET safe.

It carried `#[allow(dead_code)]` and had NO CALLERS, with a written reason: EHCI was deliberately
left co-owned in IOMMU passthrough, "the configuration the back-port keyboard works in". That
reasoning held only because several cores can absorb the SMI. **Co-ownership was never safe - it was
survivable**, and single core is where the difference shows.

It is called now, at the EHCI MMIO grant in `task::spawn_service_with_image` - before the driver
runs, and per-grant rather than once at boot so a RESTARTED driver (chaos does this constantly) also
gets a controller nobody else is running.

Nothing depended on the firmware keeping it: the `ehci` service drives that keyboard itself - HID
decode, key repeat, Ctrl+Alt+Del, `CONSOLE_PUSH`.

## The question this raised, answered: it CANNOT be a service

Worth recording, because it is the natural first instinct in this project. USBLEGSUP lives in PCI
CONFIG space, and this kernel reaches config space through legacy mechanism #1 - ports `0xCF8`
(address) and `0xCFC` (data). The kernel's own comment calls that pair "a single global register".
A port pair that addresses EVERY device cannot be granted narrowly: handing it to a service hands
over every device's configuration, which is kernel-equivalent power.

ECAM would allow a narrow grant - each function gets its own 4 KiB page, which the existing
`hw_mmio` capability could cover exactly - but this kernel does not implement ECAM, and adding MCFG
parsing would be MORE kernel surface than the handoff it was meant to avoid.

So this genuinely belongs in the kernel, and no new capability was needed: the kernel already owns
PCI configuration access and already had the code.

## Historical: why this looked like it needed new kernel surface

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
