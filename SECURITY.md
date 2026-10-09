# Security policy

GodspeedOS is a capability microkernel, and its central promise is narrow and checkable: **a service can
do only what its capabilities allow** (CLAUDE.md 3.1, 26.9). A vulnerability is anything that lets
someone do more than they were granted, or take down more than themselves.

It is also a small project, not a production kernel (CLAUDE.md 2.2). Reports are welcome and taken
seriously; there is no promised response time and no embargo process.

## Reporting

**Report privately, through GitHub:** the repository's **Security** tab, then **Report a
vulnerability** - <https://github.com/godspeedhq/godspeed-os/security/advisories/new>. Do not open a
public issue or pull request for a security problem until it is fixed.

Please include:

- the **commit**, from the first line of the boot banner (`GodspeedOS 0.22.0 x86_64 (f54aafef) - kernel`);
- the **machine** - the board, or the QEMU command line;
- **what you did and what happened**, and the **capability** the attacker held - which service, or which
  of the attackers below;
- the **serial log**, or the smallest program or sequence that shows it.

Fixed findings are recorded in [`audits/security-audit.md`](audits/security-audit.md) as `SEC-N`, and
the fixing commit carries a `Reported-by:` trailer with your name - the same credit, in git history,
that every contribution gets ([`CONTRIBUTING.md`](CONTRIBUTING.md)).

**Supported:** the `main` branch and the most recent release. Older releases are not patched.

## Who the attacker is

1. **A malicious or compromised service**, holding exactly the capabilities its spawn request gave it.
   This is the main model: it is what the capability system exists to contain.
2. **A remote network peer** - frames to `nic-driver` and `net-stack`, DNS answers, TCP segments, and
   WiFi handshake frames to the radio drivers' supplicant.
3. **A malicious USB or PCI device**, feeding crafted descriptors and data to `xhci`, `ehci` or `dwc2`.

**Physical access is out of scope.** Someone holding the board can replace the card, the firmware or
the image.

## What is a vulnerability

- **Capability forgery or escalation** - rights widened on transfer, a generation check bypassed, a
  capability moved without `GRANT`, a fabricated handle accepted. (SEC-35: a read-only file capability
  that performed a write.)
- **Stale authority** - a capability that survives the revocation or death of its resource and reaches
  what replaced it. (SEC-5: after `delete_tree`, a surviving capability re-resolved to a recreated file.)
- **A missing check** - a privileged action reachable with no capability, or with the wrong one.
- **An isolation break** - reading or writing another service's memory, or the kernel's.
- **A kernel panic or hang from unprivileged input** - any value a service or a remote peer controls
  that stops the kernel. One service crashing is a recovery bug; the kernel stopping takes every service
  with it, so it is a security bug.
- **Unbounded exhaustion** - one service using up a kernel resource others need (the capability table,
  the task pool, memory beyond its limit), so that others fail.
- **A confused deputy** - a less privileged service getting a trusted one (the supervisor, the shell,
  `fs`) to act with authority it does not hold itself.
- **An IOMMU confinement escape** - a confined device reaching outside its arena, on a machine whose
  IOMMU confines it.
- **Remote compromise** - a network peer reaching beyond the service that parses its packets.

**Not a vulnerability, but still wanted:** a service that crashes on bad input and recovers is a
RECOVERY bug - report it in a public issue. A failure `chaos` causes is also a recovery bug, even a
panic: `chaos` holds the authority to kill services, so it is not unprivileged input
(`CONTRIBUTING.md`, "Break it").

## Known limitations

These are recorded in the constitution and the audits, accepted for now, and do not need reporting
again. A way to go beyond them - for example, from one of them to something it does not already give -
does.

- **DMA without an IOMMU is unconfined.** On the Raspberry Pis, the VisionFive 2, and x86 machines
  without AMD-Vi, a service granted a DMA arena can point its device anywhere in memory, so a compromised
  driver is kernel-equivalent (CLAUDE.md 6.4). With AMD-Vi, only `xhci` and `audio-driver` are confined;
  `ehci`, `block-driver` and `nic-driver` run in passthrough.
- **A keyboard driver can type commands** (SEC-2). Keystrokes are commands, and the kernel cannot tell a
  synthesized key from a real one, so a `CONSOLE_PUSH` holder is inside the shell's trust.
- **A compromised supervisor can run any code under any name.** It supplies service images itself, and
  nothing checks them until images are signed (CLAUDE.md 14.1, the step C amendment).
- **Open low-severity findings** in `audits/security-audit.md`, such as SEC-36 (a reply-capability edge
  case) and SEC-38 (aarch64 cannot stop a dead driver's controller from doing DMA into its own arena).

## Severity

The scale `audits/security-audit.md` uses, with one rule added:

- **HIGH** - a reachable path to authority beyond grant, or kernel memory corruption.
- **MED** - a real weakness with a bounded precondition (a compromised driver, a specific sequence, a
  non-default holder), or a defect one edit away from HIGH.
- **LOW** - a bounded denial of service that recovery absorbs, or an information-only hazard.

**Added:** a kernel panic or hang reachable from any unprivileged input is at least **MED**, because it
stops the whole machine.
