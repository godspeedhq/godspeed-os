"""Refuse to build an image whose supervisor spawns a service the kernel did not embed.

The failure this prevents has now happened twice, both times on a fresh port, both times with a
diagnostic that points at the wrong thing:

    task: spawn 'console' failed: LoadFailed(TooSmall)

`TooSmall` means the kernel embedded the empty PLACEHOLDER, because the name was missing from that
arch's list in `kernel/build.rs`. It reads like a corrupt or truncated binary. It is not: the binary
was never built, or was built and never listed. First time it was `nic-driver` + `net-stack` on the
Pi 4; second time `console`, `time` and `control` on the Pi 4, after all three were added for the
Pi 2 and the aarch64 list was not kept in step.

Three facts had to agree and nothing checked that they did:
  1. `services/supervisor` MANAGED - who gets spawned,
  2. `kernel/build.rs` <arch>_built  - whose ELF is embedded for real,
  3. the build script's service list  - who gets compiled.

(3) no longer exists as a separate fact: both build scripts now DERIVE it from (2). This closes the
remaining gap, (1) against (2).

A name may be absent from an arch's list only by being named here, with a reason. That is the whole
design: an exemption is a sentence someone wrote, not a silence.
"""
import io
import os
import re

# Services the supervisor manages that are legitimately absent on a GIVEN arch, and why.
#
# Keyed BY ARCH, deliberately. The first version of this was one flat set, and it made the check blind
# in exactly the place it was most needed: `xhci` was exempted because the Pi 2 has no xHCI controller,
# which then also excused it from the aarch64 list - where it is the Pi 4's only USB driver and the
# service being edited all day. Deleting it from `aarch64_built` still passed. An exemption is a claim
# about ONE machine; letting it span every machine is how a guard stops guarding.
#
# Anything not listed here must be embedded for that arch. A new exemption is a sentence someone
# writes, not a silence.
ARCH_EXEMPT = {
    "arm": {
        "ehci": "x86-only USB2 controller driver; the Pi 2 has no EHCI",
        "xhci": "the Pi 2 has no PCIe and no xHCI controller; its USB host is dwc2",
        "hw-enumerator": "its authority is legacy PCI CF8/CFC PORT I/O, and ARM has no port I/O "
                         "address space at all - `in`/`out` are x86 instructions with no equivalent. "
                         "Not 'not ported yet': there is nothing here for it to read. A hardware "
                         "enumerator for this board would reach devices by device tree instead, "
                         "which is a different implementation behind the same service contract "
                         "(docs/service-ownership.md, D2).",
    },
    "aarch64": {
        "ehci": "x86-only USB2 controller driver; the Pi 4's USB host is the VL805 xHCI",
        "dwc2": "arm32-only (Pi 2) USB host driver; the Pi 4 drives xhci over PCIe",
    },
}

NAME = chr(34) + "([a-z0-9" + chr(45) + "]+)" + chr(34)


def _strip_comments(text):
    return chr(10).join(ln.split("//")[0] for ln in text.split(chr(10)))


def _block(src, head, what, where):
    """The source text of the array/expression introduced by `head`, comments stripped.

    Terminated by the first `];` or `};` after the head, whichever comes first - the three rosters
    this reads are written in three shapes (an array closed on its own line, one closed inline, and a
    conditional returning two arrays), and the check is worth more than a preference about which.
    """
    at = src.find(head)
    if at < 0:
        raise SystemExit(
            "service_embed_check: cannot find " + what + " in " + where + "." + chr(10)
            + "Refusing to pass a check I could not actually run - a gate that silently finds"
            + chr(10) + "nothing to check is worse than no gate, because it reports success."
        )
    ends = [e for e in (src.find("];", at), src.find("};", at)) if e >= 0]
    if not ends:
        raise SystemExit(
            "service_embed_check: found " + what + " in " + where + " but not its end."
        )
    return _strip_comments(src[at:min(ends)])


def managed(root):
    """The service names the supervisor spawns and watches."""
    src = io.open(os.path.join(root, "services", "supervisor", "src", "main.rs"),
                  encoding="utf-8").read()
    body = _block(src, "const MANAGED: [&str; MANAGED_N] =", "`MANAGED`",
                  "services/supervisor/src/main.rs")
    return re.findall(NAME, body)


def embedded_arms(root, arch):
    """[(label, names)] - one entry per ARM of the arch's embed list.

    Per arm, deliberately, NOT the union. `aarch64_built` is a conditional: one array when the
    `pi4-demo-services` feature is on and another when it is off. The first version of this check
    took the union, and so PASSED a build with `time` deleted from the shipping arm because the demo
    arm still mentioned it - an inert gate that reported success on the very defect it was written
    for. A name has to be present in every arm that can be built, because every arm can be shipped.
    """
    src = io.open(os.path.join(root, "kernel", "build.rs"), encoding="utf-8").read()
    head = "let " + arch + "_built: &[&str] = "
    body = _block(src, head, "`" + arch + "_built`", "kernel/build.rs")
    arms = body.split("} else {")
    out = []
    for i, arm in enumerate(arms):
        seen, names = set(), []
        for n in re.findall(NAME, arm):
            if n not in seen:
                seen.add(n)
                names.append(n)
        label = arch if len(arms) == 1 else arch + " arm " + str(i + 1) + " of " + str(len(arms))
        out.append((label, names))
    return out


def embedded(root, arch):
    """Every name embedded for `arch` in ANY arm - what to BUILD (over-building is free)."""
    seen, out = set(), []
    for _, names in embedded_arms(root, arch):
        for n in names:
            if n not in seen:
                seen.add(n)
                out.append(n)
    return out


def check(root, arch):
    """Return a list of failure lines; empty means every arm embeds every managed service."""
    names = managed(root)
    bad = []
    for label, have in embedded_arms(root, arch):
        have = set(have)
        exempt = ARCH_EXEMPT.get(arch, {})
        for name in names:
            if name in have or name in exempt:
                continue
            bad.append(
                "  " + name + " [" + label + "]: the supervisor SPAWNS it, but it is not in that "
                "arm of `" + arch + "_built` in kernel/build.rs, so the kernel embeds an empty "
                "placeholder and the boot fails with LoadFailed(TooSmall)."
            )
    return bad


def enforce(root, arch):
    bad = check(root, arch)
    if bad:
        raise SystemExit(
            "service_embed_check FAILED for " + arch + ":" + chr(10)
            + chr(10).join(bad) + chr(10) + chr(10)
            + "Add each name to that list, or - if it genuinely does not exist on this arch - add it"
            + chr(10) + "to ARCH_EXEMPT in scripts/service_embed_check.py with the reason."
        )
    print("OK  service-embed check (" + arch + "): every managed service is embedded for real")


if __name__ == "__main__":
    import sys
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    failed = False
    for a in ("arm", "aarch64"):
        try:
            enforce(here, a)
        except SystemExit as e:
            print(e)
            failed = True
    sys.exit(1 if failed else 0)
