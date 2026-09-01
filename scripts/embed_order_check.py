"""Refuse to ship an image whose supervisor embeds a STALE copy of a service.

The supervisor `include_bytes!`s every service image, so it must be built AFTER all of them. If it is
built first, cargo happily reuses the previous run's binaries and the image ships services that are
one build behind - with a kernel that is current. Nothing fails, nothing warns, and the machine runs
yesterday's code.

This is not hypothetical. `scripts/arm_build.py` built the supervisor at index 4 of 24 and
`scripts/pi4_build.py` at index 6 of 24, so on BOTH Pi ports every image carried stale copies of the
services built after it - `shell`, `dwc2`, `fs`, `block-driver`, `nic-driver`, `net-stack` among
them. It was found when a shell change was verified on x86, flashed to a Pi 2, and demonstrably was
not in the image: `target/.../shell` had the new code, `target/.../supervisor` did not, and neither
did `kernel7.img`. Every ARM hardware result before that had been read as evidence about code that
may not have been running.

The x86 path (`osdev`) always had this right - it builds `non_supervisor` first and the supervisor
last - which is why x86 results were trustworthy while ARM ones silently were not.

Ordering alone is not enough to rely on: it is a convention, and a convention that is only visible as
a list's index order is one edit from being wrong again. This is the mechanical check that makes the
ordering enforceable rather than remembered.
"""
import os


def enforce(root, target, profile, services):
    """Fail loudly if any embedded service binary is NEWER than the supervisor that embeds it."""
    outdir = os.path.join(root, "target", target, profile)
    sup = os.path.join(outdir, "supervisor")
    if not os.path.exists(sup):
        raise SystemExit(
            "embed-order: no supervisor binary at %s - cannot check what it embedded." % sup)
    sup_mtime = os.path.getmtime(sup)

    stale = []
    for svc in services:
        if svc == "supervisor":
            continue
        p = os.path.join(outdir, svc)
        if not os.path.exists(p):
            continue
        if os.path.getmtime(p) > sup_mtime:
            stale.append(svc)

    if stale:
        raise SystemExit(
            "\nembed-order: the SUPERVISOR IS OLDER than %d service(s) it embeds:\n"
            "    %s\n\n"
            "The supervisor `include_bytes!`s each of these, so an image built now would carry the\n"
            "PREVIOUS build of them next to a current kernel - silently, because a stale file is not\n"
            "a missing one and nothing else checks.\n\n"
            "Build the supervisor LAST (after every other service), then the kernel.\n"
            % (len(stale), ", ".join(sorted(stale))))

    print("OK  embed-order (%s): the supervisor is newer than all %d services it embeds"
          % (target, len([s for s in services if s != "supervisor"])))
