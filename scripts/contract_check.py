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
    cls = {"Ahci": "ahci", "Nic": "nic", "Xhci": "xhci", "Ehci": "ehci", "None": None}
    out: dict = {}
    for arm in re.finditer(r'((?:"[^"]+"\s*\|?\s*)+)=>\s*\(\s*HwClass::(\w+)\s*,\s*(true|false)\s*\)', body):
        names = re.findall(r'"([^"]+)"', arm.group(1))
        hw = cls.get(arm.group(2))
        mint = arm.group(3) == "true"
        for nm in names:
            out[nm] = (hw, mint)
    return out


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
    cm = re.search(r'preferred_core:\s*(\d+)', body)
    if cm:
        core = int(cm.group(1))

    send = []
    sm = re.search(r'send_peers:\s*&\[([^\]]*)\]', body)
    if sm:
        send = [s.strip().strip('"') for s in sm.group(1).split(",") if s.strip()]

    return {"limit": limit, "core": core, "send": send}


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
            failures.append(f"  FAIL  {name}: no `service_config` arm found in kernel/src/task/mod.rs")
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
        if t["hw_device"] != khw:
            failures.append(
                f"  FAIL  {name}: hw_device {t['hw_device']!r} (.toml) != {khw!r} (kernel service_hw)")
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
