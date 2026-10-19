#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""`godspeed-platform.toml`: the facts a release says to developer tooling (docs/developer-services-design.md 11).

The `godspeed` CLI builds a developer's service the way the OS builds its own, so it needs seven facts about
a release. Until this file it read six of them from this repository's own build inputs, at their paths, and
hard-coded the seventh. Those files are not a contract: moving `services/user.ld`, or renaming a key in
`.cargo/config.toml`, would break every installed CLI with nothing here to notice. This script is the
contract, and it is GENERATED from those same files, never written by hand, so each fact still has one
source (CLAUDE.md 13.6's rule about contracts, applied to the platform).

THE SEVEN FACTS, and where each is read:

  | fact                                  | read from                                             |
  |---------------------------------------|-------------------------------------------------------|
  | the OS version                        | `Cargo.toml`, `[workspace.package] version`           |
  | the stdlib crate, named `godspeed`    | `stdlib/rust/Cargo.toml`                              |
  | the toolchain pin and the targets     | `rust-toolchain.toml`                                 |
  | per-target compiler flags             | `.cargo/config.toml`, `[target.<triple>] rustflags`   |
  | the service linker script             | `services/user.ld`, carried as text                   |
  | the entry symbol and the linker script| every service's and example's `build.rs`              |
  | the contract schema                   | `contracts/schema/service.schema.json`, as text       |

The entry symbol is the one fact with no single source: each `build.rs` repeats `--entry=service_main` and
`-T` the linker script. So this REFUSES unless every one of them names the same symbol and the same script,
and a disagreement fails here rather than being published as whichever was read first.

Only the targets `rust-toolchain.toml` lists are platform targets. `.cargo/config.toml` also carries flags
for the stub ISAs (riscv32, loongarch64, s390x), which no release builds; they are left out, and every
listed target must have flags.

MODES:
  --check          read every fact and refuse on any problem; write nothing. In osdev's EXTRA_CHECKS, so a
                   rename fails the next local build, not a developer's machine.
  --out <path>     also write the file there (the release workflow, into `dist/` before SHA256SUMS).
  --commit <sha>   record the commit it was generated from (the release workflow passes it).
  --selftest       prove the refusals fire: a disagreeing build.rs, a missing target's flags, a renamed
                   linker script.

The output is read back with `tomllib` where Python has it (and the repository's subset parser where it
does not) and compared with the facts, so a quoting mistake cannot publish a file that says something else.
"""
from __future__ import annotations

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import toml_compat as tomllib  # noqa: E402  tomllib on 3.11+, the subset parser below it (README: 3.8)

FORMAT = 1
BUILD_RS_DIRS = ("services", "examples")
ENTRY_RE = re.compile(r'rustc-link-arg=--entry=([A-Za-z_][A-Za-z0-9_]*)')
SCRIPT_RE = re.compile(r'\.join\("([^"]+\.ld)"\)')


class Refused(Exception):
    pass


def _load(root: str, rel: str) -> dict:
    path = os.path.join(root, rel)
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except FileNotFoundError:
        raise Refused(f"{rel} is missing")
    except tomllib.TOMLDecodeError as e:
        raise Refused(f"{rel} is not valid TOML: {e}")


def _text(root: str, rel: str) -> str:
    try:
        with open(os.path.join(root, rel), encoding="utf-8", newline="") as f:
            return f.read().replace("\r\n", "\n")
    except FileNotFoundError:
        raise Refused(f"{rel} is missing")


def _link_facts(root: str) -> tuple:
    """The entry symbol and linker script every build.rs names, refused unless they all agree."""
    entries, scripts, seen = {}, {}, 0
    for d in BUILD_RS_DIRS:
        base = os.path.join(root, d)
        if not os.path.isdir(base):
            continue
        for name in sorted(os.listdir(base)):
            rel = f"{d}/{name}/build.rs"
            path = os.path.join(root, rel)
            if not os.path.isfile(path):
                continue
            text = _text(root, rel)
            e = ENTRY_RE.findall(text)
            s = SCRIPT_RE.findall(text)
            if not e or not s:
                raise Refused(f"{rel} names no entry symbol or no linker script "
                              f"(expected `--entry=<symbol>` and a `.join(\"<script>.ld\")`)")
            seen += 1
            for v in e:
                entries.setdefault(v, []).append(rel)
            for v in s:
                scripts.setdefault(v, []).append(rel)
    if seen == 0:
        raise Refused("no service build.rs found under services/ or examples/")
    for what, got in (("entry symbol", entries), ("linker script", scripts)):
        if len(got) != 1:
            parts = "; ".join(f"`{k}` in {len(v)} ({v[0]}...)" for k, v in sorted(got.items()))
            raise Refused(f"the build.rs files disagree about the {what}: {parts}")
    return next(iter(entries)), next(iter(scripts)), seen


def facts(root: str) -> dict:
    ws = _load(root, "Cargo.toml")
    try:
        version = ws["workspace"]["package"]["version"]
    except KeyError:
        raise Refused("Cargo.toml has no [workspace.package] version")

    stdlib = _load(root, "stdlib/rust/Cargo.toml")
    crate = stdlib.get("package", {}).get("name")
    if crate != "godspeed":
        raise Refused(f"stdlib/rust/Cargo.toml is the `{crate}` crate, not `godspeed`")

    tc = _load(root, "rust-toolchain.toml").get("toolchain", {})
    channel, targets = tc.get("channel"), tc.get("targets")
    if not isinstance(channel, str) or not channel:
        raise Refused("rust-toolchain.toml has no [toolchain] channel")
    if not isinstance(targets, list) or not targets or not all(isinstance(t, str) for t in targets):
        raise Refused("rust-toolchain.toml has no [toolchain] targets")

    cfg = _load(root, ".cargo/config.toml").get("target", {})
    flags = {}
    for t in targets:
        f = cfg.get(t, {}).get("rustflags")
        if not isinstance(f, list) or not all(isinstance(x, str) for x in f):
            raise Refused(f".cargo/config.toml has no [target.{t}] rustflags, and rust-toolchain.toml lists {t}")
        flags[t] = f

    entry, script, nbuild = _link_facts(root)
    if script != "user.ld":
        # The CLI carries the script under this name; a rename is a new format, not a silent change.
        raise Refused(f"the build.rs files name `{script}`, not `user.ld`, as the linker script")
    ld = _text(root, f"services/{script}")
    schema = _text(root, "contracts/schema/service.schema.json")
    return {
        "format": FORMAT, "os": version, "stdlib_crate": crate, "stdlib_tag": f"v{version}",
        "channel": channel, "targets": targets, "rustflags": flags,
        "entry": entry, "script": script, "build_rs": nbuild,
        "files": {"user.ld": ld, "service.schema.json": schema},
    }


def _q(s: str) -> str:
    """A TOML basic string."""
    out = []
    for ch in s:
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\t":
            out.append("\\t")
        elif ord(ch) < 0x20 or ord(ch) == 0x7f:
            out.append("\\u%04x" % ord(ch))
        else:
            out.append(ch)
    return '"' + "".join(out) + '"'


def _arr(xs: list) -> str:
    return "[" + ", ".join(_q(x) for x in xs) + "]"


def render(f: dict, commit) -> str:
    lines = [
        "# godspeed-platform.toml - GENERATED by scripts/platform_file.py from the files the OS builds from.",
        "# Never edit by hand. docs/developer-services-design.md section 11 is the contract it implements.",
        f"format = {f['format']}",
        f"os = {_q(f['os'])}",
    ]
    if commit:
        lines.append(f"commit = {_q(commit)}")
    lines += [
        "",
        "[stdlib]",
        f"crate = {_q(f['stdlib_crate'])}",
        f"tag = {_q(f['stdlib_tag'])}",
        "",
        "[toolchain]",
        f"channel = {_q(f['channel'])}",
        f"targets = {_arr(f['targets'])}",
        "",
        "[link]",
        f"entry = {_q(f['entry'])}",
        f"script = {_q(f['script'])}",
        "",
        "[rustflags]",
    ]
    for t in f["targets"]:
        lines.append(f"{_q(t)} = {_arr(f['rustflags'][t])}")
    lines += ["", "[files]"]
    for name, text in f["files"].items():
        lines.append(f"{_q(name)} = {_q(text)}")
    return "\n".join(lines) + "\n"


def read_back(text: str, f: dict, commit) -> None:
    """Parse what was rendered and compare it with the facts, so the file cannot say something else."""
    import io
    d = tomllib.load(io.BytesIO(text.encode("utf-8")))
    want = {
        "format": f["format"], "os": f["os"],
        "stdlib": {"crate": f["stdlib_crate"], "tag": f["stdlib_tag"]},
        "toolchain": {"channel": f["channel"], "targets": f["targets"]},
        "link": {"entry": f["entry"], "script": f["script"]},
        "rustflags": f["rustflags"], "files": f["files"],
    }
    if commit:
        want["commit"] = commit
    if d != want:
        diff = sorted(k for k in set(d) | set(want) if d.get(k) != want.get(k))
        raise Refused(f"the rendered file does not read back as the facts it was made from: {diff}")


def selftest() -> int:
    import shutil
    import tempfile
    failures = 0

    def copy_tree() -> str:
        tmp = tempfile.mkdtemp(prefix="platform-selftest-")
        for rel in ("Cargo.toml", "rust-toolchain.toml", ".cargo/config.toml", "stdlib/rust/Cargo.toml",
                    "services/user.ld", "contracts/schema/service.schema.json"):
            os.makedirs(os.path.dirname(os.path.join(tmp, rel)) or tmp, exist_ok=True)
            shutil.copyfile(os.path.join(ROOT, rel), os.path.join(tmp, rel))
        for d in BUILD_RS_DIRS:
            for name in os.listdir(os.path.join(ROOT, d)):
                src = os.path.join(ROOT, d, name, "build.rs")
                if os.path.isfile(src):
                    os.makedirs(os.path.join(tmp, d, name), exist_ok=True)
                    shutil.copyfile(src, os.path.join(tmp, d, name, "build.rs"))
        return tmp

    def expect(label: str, mutate, needle: str) -> None:
        nonlocal failures
        tmp = copy_tree()
        try:
            mutate(tmp)
            try:
                facts(tmp)
                print(f"  FAIL  {label}: not refused")
                failures += 1
            except Refused as e:
                if needle in str(e):
                    print(f"  ok    {label}: refused ({e})")
                else:
                    print(f"  FAIL  {label}: refused for the wrong reason ({e})")
                    failures += 1
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

    def one_build_rs(tmp: str) -> str:
        return os.path.join(tmp, "services", "time", "build.rs")

    def entry_differs(tmp):
        p = one_build_rs(tmp)
        s = open(p, encoding="utf-8").read().replace("--entry=service_main", "--entry=start")
        open(p, "w", encoding="utf-8").write(s)

    def script_differs(tmp):
        p = one_build_rs(tmp)
        s = open(p, encoding="utf-8").read().replace('"user.ld"', '"other.ld"')
        open(p, "w", encoding="utf-8").write(s)

    def flags_missing(tmp):
        p = os.path.join(tmp, ".cargo", "config.toml")
        s = open(p, encoding="utf-8").read().replace("[target.aarch64-unknown-none]", "[target.aarch64-renamed]")
        open(p, "w", encoding="utf-8").write(s)

    def script_gone(tmp):
        os.remove(os.path.join(tmp, "services", "user.ld"))

    expect("a build.rs names another entry symbol", entry_differs, "disagree about the entry symbol")
    expect("a build.rs names another linker script", script_differs, "disagree about the linker script")
    expect("a listed target has no rustflags", flags_missing, "[target.aarch64-unknown-none] rustflags")
    expect("the linker script is gone", script_gone, "services/user.ld is missing")
    try:
        f = facts(ROOT)
        read_back(render(f, "0" * 40), f, "0" * 40)
        print("  ok    the real tree renders and reads back")
    except Refused as e:
        print(f"  FAIL  the real tree: {e}")
        failures += 1
    print(f"platform file selftest: {'PASS' if failures == 0 else 'FAIL'}")
    return 1 if failures else 0


def main(argv: list) -> int:
    if "--selftest" in argv:
        return selftest()
    out = commit = None
    if "--out" in argv:
        out = argv[argv.index("--out") + 1]
    if "--commit" in argv:
        commit = argv[argv.index("--commit") + 1]
    try:
        f = facts(ROOT)
        text = render(f, commit)
        read_back(text, f, commit)
    except Refused as e:
        print(f"platform file: REFUSED - {e}")
        print("  The godspeed CLI builds against these facts (docs/developer-services-design.md 11). Fix the")
        print("  file named above, or change this script and that section together if the fact really moved.")
        return 1
    if out:
        with open(out, "w", encoding="utf-8", newline="\n") as w:
            w.write(text)
    print(f"platform file: every fact found - GodspeedOS {f['os']}, {f['channel']}, {len(f['targets'])} target(s), "
          f"entry `{f['entry']}` and `{f['script']}` agreed by {f['build_rs']} build.rs"
          + (f"; written to {out}" if out else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
