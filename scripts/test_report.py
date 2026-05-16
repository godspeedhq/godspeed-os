#!/usr/bin/env python3
"""Render a Unicode-bordered table of GodspeedOS kernel unit test results.

Runs:  cargo test -p kernel -- -Z unstable-options --format json
Exit:  0 if all tests pass, 1 if any fail.
"""

import json
import subprocess
import sys
from collections import defaultdict
from datetime import datetime, timezone

PHASE_MAP = {
    "capability::cap":        "Capability",
    "capability::generation": "Generation",
    "capability::rights":     "Rights",
    "ipc::queue":             "IPC Queue",
}

PHASE_ORDER = list(PHASE_MAP.keys())


def run_tests():
    proc = subprocess.run(
        ["cargo", "test", "-p", "kernel", "--",
         "-Z", "unstable-options", "--format", "json"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return proc.stdout, proc.returncode


def parse_events(raw):
    events = []
    for line in raw.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "test" and ev.get("event") in ("ok", "failed"):
            events.append(ev)
    return events


def phase_key(name):
    for prefix in PHASE_ORDER:
        if name.startswith(prefix + "::"):
            return prefix
    return "other"


def short_name(full):
    sep = "::tests::"
    short = full.split(sep)[-1] if sep in full else full.split("::")[-1]
    return short.replace("_", " ")


def fmt_ms(ev):
    return f"{ev.get('exec_time', 0.0) * 1000:.2f}"


def render(events):
    by_phase = defaultdict(list)
    for ev in events:
        by_phase[phase_key(ev["name"])].append(ev)

    # Column widths (content only, no surrounding spaces)
    PW = max(len("Phase"),  max((len(PHASE_MAP.get(k, k)) for k in by_phase), default=5))
    TW = max(len("Test"),   max((len(short_name(e["name"])) for e in events), default=4))
    RW = len("Result")
    MW = max(len("ms"),     max((len(fmt_ms(e)) for e in events), default=2))

    # Inner width: 1+PW+1 | 1+TW+1 | 1+RW+1 | 1+MW+1  →  PW+TW+RW+MW+11
    IW = PW + TW + RW + MW + 11

    def seg(w, h="═"):
        return h * (w + 2)

    def hline(left, sep, right, h="═"):
        return left + sep.join([seg(PW, h), seg(TW, h), seg(RW, h), seg(MW, h)]) + right

    def data_row(phase, test, result, ms):
        return f"║ {phase:<{PW}} │ {test:<{TW}} │ {result:^{RW}} │ {ms:>{MW}} ║"

    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    title = f"GodspeedOS — Kernel Unit Tests   {now}"

    out = []
    out.append("╔" + "═" * IW + "╗")
    out.append("║" + f" {title} ".center(IW) + "║")
    out.append(hline("╠", "╤", "╣"))
    out.append(data_row("Phase", "Test", "Result", "ms"))
    out.append(hline("╠", "╪", "╣"))

    total_ms = 0.0
    passed = 0
    failed = 0
    first = True

    for pk in PHASE_ORDER + ["other"]:
        if pk not in by_phase:
            continue
        label = PHASE_MAP.get(pk, pk.title())

        if not first:
            out.append(hline("╟", "┼", "╢", "─"))
        first = False

        for ev in by_phase[pk]:
            name = short_name(ev["name"])
            ok = ev["event"] == "ok"
            res = "PASS" if ok else "FAIL"
            ms = fmt_ms(ev)
            total_ms += float(ms)
            passed += ok
            failed += not ok
            out.append(data_row(label, name, res, ms))

    out.append(hline("╠", "╧", "╣"))

    left_part = f" {passed} passed · {failed} failed"
    right_part = f"Total: {total_ms:.2f} ms "
    gap = IW - len(left_part) - len(right_part)
    out.append("║" + left_part + " " * max(gap, 1) + right_part + "║")
    out.append("╚" + "═" * IW + "╝")

    return "\n".join(out), passed, failed


def main():
    raw, rc = run_tests()
    events = parse_events(raw)

    if not events:
        print("No test results parsed. Cargo output:")
        print(raw)
        sys.exit(rc if rc != 0 else 1)

    table, passed, failed = render(events)
    print(table)
    sys.exit(0 if failed == 0 else 1)


if __name__ == "__main__":
    main()
