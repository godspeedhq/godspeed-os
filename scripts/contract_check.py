#!/usr/bin/env python3
"""Reconcile each real service's `.toml` contract against the kernel's `service_config` (audit T1).

The kernel is `no_std` and cannot parse TOML at spawn, so it carries a compiled `service_config(name)`
table (`kernel/src/task/mod.rs`) that is the ACTUAL source of a service's caps/placement/memory at
spawn. The human-facing `.toml` contract is a SECOND declaration - and the two drifted (audit M6: a
contract that mis-stated the driver's authority; T1 found logger/supervisor memory + supervisor peers
diverged too). Commandment III: what RUNS cannot differ from what is DECLARED.

This check makes drift impossible for the services that HAVE a contract: it parses each `.toml` and the
kernel `service_config` for that name and fails CI on any mismatch of the reconcilable fields -
`memory.limit` <-> `memory_limit`, `placement.core` <-> `preferred_core`, `ipc_send` <-> `send_peers`.
Structural fields (elf, probe_mode, has_recv_endpoint) are kernel-only and not reconciled. Test/probe
fixtures have no `.toml` (single source, the kernel) and are not checked.

Exit: 0 if every contract matches its kernel config, 1 otherwise.
"""

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).parent.parent
KERNEL_CFG = REPO_ROOT / "kernel" / "src" / "task" / "mod.rs"
SERVICES   = REPO_ROOT / "services"

# Every service that ships a contract, derived from the tree (audit U13): a NEW service that adds a
# `contracts/<name>.toml` is reconciled automatically - it cannot ship a contract that silently
# disagrees with the kernel just because a hand-list forgot it. (Test/probe fixtures deliberately have
# no `.toml`, so they are not in this set.)
CONTRACTED = sorted(d.name for d in SERVICES.iterdir()
                    if (d / "contracts" / f"{d.name}.toml").exists())


def parse_toml(path: Path) -> dict:
    """Extract the reconcilable fields from a service .toml (small, fixed shape - no toml dep needed)."""
    text = path.read_text(encoding="utf-8")
    # Strip comments so a `# ... ipc_send ...` note never reads as a declaration.
    text = "\n".join(line.split("#", 1)[0] for line in text.splitlines())

    limit = None
    m = re.search(r'limit\s*=\s*"(\d+)\s*MiB"', text)
    if m:
        limit = int(m.group(1)) * 1024 * 1024

    core = None
    m = re.search(r'\bcore\s*=\s*(\d+)', text)
    if m:
        core = int(m.group(1))

    send = []
    m = re.search(r'ipc_send\s*=\s*\[([^\]]*)\]', text)
    if m:
        send = [s.strip().strip('"') for s in m.group(1).split(",") if s.strip()]

    hw_device = None
    m = re.search(r'hw_device\s*=\s*"(\w+)"', text)
    if m:
        hw_device = m.group(1)

    resource_mint = bool(re.search(r'resource_mint\s*=\s*true', text))

    return {"limit": limit, "core": core, "send": send,
            "hw_device": hw_device, "resource_mint": resource_mint}


def parse_service_hw(source: str) -> dict:
    """Map each name -> (hw_device str|None, resource_mint bool) from the kernel `service_hw` match."""
    m = re.search(r'fn service_hw\(name: &str\)\s*->\s*\(HwClass, bool\)\s*\{(.*?)\n\}', source, re.DOTALL)
    if not m:
        return {}
    body = m.group(1)
    # Every HwClass the kernel can return must appear here. A missing entry silently maps to None and
    # then reports "contract says X, kernel says None" - blaming the contract for the parser's gap.
    # `Dwc2` was missing, which is how a service written this session looked like a contract error.
    cls = {"Ahci": "ahci", "Nic": "nic", "Xhci": "xhci", "Ehci": "ehci", "Dwc2": "dwc2",
           "Framebuffer": "framebuffer", "None": None}
    out: dict = {}
    for arm in re.finditer(r'((?:"[^"]+"\s*\|?\s*)+)=>\s*\(\s*HwClass::(\w+)\s*,\s*(true|false)\s*\)', body):
        names = re.findall(r'"([^"]+)"', arm.group(1))
        hw = cls.get(arm.group(2))
        mint = arm.group(3) == "true"
        for nm in names:
            out[nm] = (hw, mint)
    return out


def kernel_core_consts(source: str) -> dict:
    """`pub const XHCI_CORE: u32 = 2;` -> {"XHCI_CORE": 2}.

    `preferred_core` is often a NAMED constant rather than a literal, and a parser that reads only
    digits reports `None` for it - which then looks like the contract disagreeing with the kernel when
    in fact the two agree and the reader cannot see it. Resolve the names instead of blaming the file.
    """
    return {m.group(1): int(m.group(2))
            for m in re.finditer(r"pub const ([A-Z_]+_CORE): u32 = (\d+);", source)}


def parse_kernel(name: str, source: str) -> dict | None:
    """Extract memory_limit/preferred_core/send_peers from the kernel `service_config` arm for `name`."""
    # The arm: `"name" => Some(("name", ServiceConfig { ... })),`  - grab up to the closing `})),`.
    m = re.search(
        r'"' + re.escape(name) + r'"\s*=>\s*Some\(\(\s*"' + re.escape(name) + r'"\s*,\s*ServiceConfig\s*\{(.*?)\}\)\)',
        source, re.DOTALL)
    if not m:
        return None
    body = m.group(1)

    limit = None
    lm = re.search(r'memory_limit:\s*(\d+)\s*\*\s*1024\s*\*\s*1024', body)
    if lm:
        limit = int(lm.group(1)) * 1024 * 1024

    core = None
    # `preferred_core` may be arch-conditional: `if cfg!(target_arch = "arm") { 0 } else { 1 }`. This
    # check runs on the host (x86) and the .toml states the x86-intended core, so take the `else` (x86)
    # value; a plain `preferred_core: N` is captured as-is. The ARM core is arch-specific (noted in the
    # service's .toml where it differs) and not reconciled here.
    cm_cfg = re.search(r'preferred_core:\s*if cfg!\([^)]*\)\s*\{\s*\d+\s*\}\s*else\s*\{\s*(\d+)\s*\}', body)
    if cm_cfg:
        core = int(cm_cfg.group(1))
    else:
        cm = re.search(r'preferred_core:\s*(\d+)', body)
        if cm:
            core = int(cm.group(1))
        else:
            # A NAMED constant (`preferred_core: XHCI_CORE`). Reading only digits reported None here,
            # which then looks like the contract disagreeing with the kernel when the two actually
            # agree - the parser's gap, presented as the file's fault.
            cn = re.search(r'preferred_core:\s*([A-Z_]+_CORE)', body)
            if cn:
                core = kernel_core_consts(source).get(cn.group(1))

    send = []
    sm = re.search(r'send_peers:\s*&\[([^\]]*)\]', body)
    if sm:
        send = [s.strip().strip('"') for s in sm.group(1).split(",") if s.strip()]

    return {"limit": limit, "core": core, "send": send}



# Step C (docs/service-ownership.md): a service's config moves from the kernel's `service_config` to
# the SUPERVISOR's `IMAGES` table as its image moves. The contract is still the source of truth; this
# reconciles it against wherever the config actually LIVES, so a service moving out of the kernel does
# not quietly escape the check. The table deliberately carries the same fields the kernel row did.
SUPERVISOR_MAIN = SERVICES / "supervisor" / "src" / "main.rs"

def _core_of(expr: str):
    """A `preferred_core` expression as an int, or None for `u32::MAX` (no preference)."""
    if "u32::MAX" in expr:
        return None
    m = re.search(r'if cfg!\([^)]*\)\s*\{\s*\d+\s*\}\s*else\s*\{\s*(\d+)\s*\}', expr)
    if m:
        return int(m.group(1))
    m = re.search(r'(\d+)', expr)
    return int(m.group(1)) if m else None

def _supervisor_row(name: str):
    """The text of the supervisor's `IMAGES` row for `name`, or None.

    Split on row boundaries rather than matching a regex across one: a row spans several lines once it
    carries privileges, and a single-line pattern silently matches nothing - which is how a check stops
    checking without anyone noticing.
    """
    try:
        src = SUPERVISOR_MAIN.read_text(encoding="utf-8")
    except OSError:
        return None
    needle = '"' + name + '"' + ","
    for row in src.split('\n' + "    ("):
        if row.startswith(needle) and "_ELF" in row:
            return row
    return None


def _supervisor_grants_resource_mint(name: str) -> bool:
    """Does the supervisor's IMAGES row for `name` request the RESOURCE_MINT privilege?

    RESOURCE_MINT follows the config out of the kernel (step C): a moved service carries it as a
    privilege BIT in the supervisor's table rather than as `service_hw`'s second element.
    """
    row = _supervisor_row(name)
    return row is not None and "privbits::RESOURCE_MINT" in row


def _supervisor_hw_device(name: str):
    """The device CLASS the supervisor's IMAGES row asks for, as the .toml spells it, or None.

    A moved driver names its hardware by class ordinal (`hwclass::AHCI`) because the kernel resolves
    the actual MMIO/DMA/BDF from its own PCI scan - the supervisor may not name addresses. The .toml
    still says `hw_device = "ahci"`, so the check reconciles the two by name, and a driver that moves
    house does not thereby escape having its contract checked.
    """
    row = _supervisor_row(name)
    if row is None:
        return None
    m = re.search(r"hwclass::(\w+)", row)
    if not m or m.group(1) == "NONE":
        return None
    return m.group(1).lower()


def parse_supervisor_images(name: str):
    """The supervisor's IMAGES row for `name`, in the same shape `parse_kernel` returns, or None."""
    try:
        src = SUPERVISOR_MAIN.read_text(encoding="utf-8")
    except OSError:
        return None
    # ("name", NAME_ELF, flags, mem, core, &[peers])
    # Tolerant of columns being APPENDED to the tuple (privileges was added after peers): match up to
    # the peer list and ignore whatever follows. A parser that silently stops matching when a table
    # grows is the failure this check exists to prevent - it did fail loudly, which is why this is here.
    m = re.search(r'\(\s*"' + re.escape(name) + r'"\s*,\s*[A-Z0-9_]+_ELF\s*,\s*([^,]+),\s*([^,]+),'
                  r'\s*([^,]+),\s*&\[([^\]]*)\]', src)
    if not m:
        return None
    mem_expr, core_expr, peers_raw = m.group(2).strip(), m.group(3).strip(), m.group(4)
    lm  = re.search(r'(\d+)\s*\*\s*1024\s*\*\s*1024', mem_expr)
    return {
        "limit": int(lm.group(1)) * 1024 * 1024 if lm else None,
        # Same arch-conditional handling as `parse_kernel`: `if cfg!(target_arch = "arm") { 1 } else
        # { 0 }` takes the ELSE (x86) branch, because this check runs on the host and the .toml states
        # the x86-intended core. Without it the shell's row raised a ValueError rather than reporting a
        # mismatch - a checker that CRASHES is worse than one that fails, since it reports nothing at all.
        "core":  _core_of(core_expr),
        "send":  sorted(re.findall(r'"([^"]+)"', peers_raw)),
    }


def main() -> int:
    source = KERNEL_CFG.read_text(encoding="utf-8")
    kernel_hw = parse_service_hw(source)
    failures: list[str] = []

    for name in CONTRACTED:
        toml_path = SERVICES / name / "contracts" / f"{name}.toml"
        if not toml_path.exists():
            failures.append(f"  FAIL  {name}: contract not found at {toml_path.relative_to(REPO_ROOT).as_posix()}")
            continue

        t = parse_toml(toml_path)
        k = parse_kernel(name, source)
        if k is None:
            # Not in the kernel: it may have moved to the supervisor (step C). Reconcile there.
            k = parse_supervisor_images(name)
        if k is None:
            failures.append(f"  FAIL  {name}: no config found - neither a `service_config` arm in "
                            f"kernel/src/task/mod.rs nor an `IMAGES` row in services/supervisor")
            continue

        if t["limit"] != k["limit"]:
            failures.append(
                f"  FAIL  {name}: memory limit {t['limit']} (.toml) != {k['limit']} (kernel memory_limit)")
        if t["core"] != k["core"]:
            failures.append(
                f"  FAIL  {name}: placement.core {t['core']} (.toml) != preferred_core {k['core']} (kernel)")
        if sorted(t["send"]) != sorted(k["send"]):
            failures.append(
                f"  FAIL  {name}: ipc_send {t['send']} (.toml) != send_peers {k['send']} (kernel)")

        khw, kmint = kernel_hw.get(name, (None, False))
        # RESOURCE_MINT follows the config out of the kernel (step C): a moved service carries it as a
        # privilege BIT in the supervisor's IMAGES row, not as `service_hw`'s second element. Same rule
        # as the rest of this check - reconcile against wherever the config actually lives, so moving a
        # service does not quietly escape it.
        if not kmint and _supervisor_grants_resource_mint(name):
            kmint = True
        # Name the file the value was actually read from. A message that says "kernel service_hw" for
        # a value that came from the supervisor sends the reader to the wrong file to fix it.
        hw_src = "kernel service_hw"
        if khw is None:
            moved = _supervisor_hw_device(name)
            if moved is not None:
                khw, hw_src = moved, "supervisor IMAGES hwclass"
        if t["hw_device"] != khw:
            failures.append(
                f"  FAIL  {name}: hw_device {t['hw_device']!r} (.toml) != {khw!r} ({hw_src})")
        if t["resource_mint"] != kmint:
            failures.append(
                f"  FAIL  {name}: resource_mint {t['resource_mint']} (.toml) != {kmint} (kernel service_hw)")

    if failures:
        print("Contract reconcile - FAILURES (a .toml disagrees with the kernel service_config):")
        for f in failures:
            print(f)
        print()
        print(f"{len(failures)} mismatch(es). The contract is the source of truth (Commandment III / "
              "audit T1): fix the .toml AND kernel/src/task/mod.rs to agree.")
        return 1

    print(f"Contract reconcile passed - {len(CONTRACTED)} contracts match their kernel service_config "
          "(memory limit, placement core, ipc_send).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
