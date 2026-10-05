#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""ONE WAY TO WRITE A SERVICE: a ratchet on raw-SDK calls that the standard library already covers.

The operator, 2026-10-05, preparing v1: "I want there to be a consistent way of writing services and
drivers ... I can't have different ways." The one way is `gs` (`backlog/71`): a service or a driver is
written on the standard library, and where it needs a mechanism `gs` does not have yet, `gs` gains it.

This counts, per service crate (`services/*`, `examples/*`), the calls on a `ServiceContext` to SDK methods
that HAVE a `gs` equivalent - `ctx.recv()` where `gs::ipc::recv` exists, `ctx.sleep_ms()` where
`gs::task::sleep_ms` does - and holds each count to a baseline that may FALL freely and may not RISE. A
crate missing from the baseline - a new service - is held to zero, so new code is written the one way
from its first line, and the old code converges as it is touched (the stdlib-dogfood work).

WHAT IT DOES NOT COUNT, on purpose:
  - SDK methods with no `gs` equivalent (spawning, the hardware accessors `Mmio`/`Dma`, the raw syscall
    seam). Asking for a replacement that does not exist would make the gate a wall instead of a ratchet;
    those are `gs::driver`'s gaps to close (`docs/driver-library.md`), recorded there, not here.
  - `ctx.log` / `ctx.log_fmt`: logging is the kernel floor every service writes to directly, by design
    (CLAUDE.md 11.4), and there is no second way to do it.
  - Anything in a comment.

`--bless` rewrites the baseline from the tree; say why in the commit. `--report` prints each crate's calls by
method, with the `gs` replacement for each.
"""
import io
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "scripts", "ONE-WAY.baseline.txt")

# SDK method -> the `gs` call that replaces it. ONLY methods with a real replacement belong here; each one
# was read off `stdlib/rust/src` (the function that wraps it), so the message below never names a `gs`
# call that does not exist.
REPLACEMENT = {
    "recv": "gs::ipc::recv",
    "try_recv": "gs::ipc::try_recv",
    "recv_timeout": "gs::ipc::recv_within_ms (or gs::driver::irq for an interrupt wait)",
    "take_pending_cap": "gs::ipc::take_sent_cap",
    "send": "gs::ipc::send",
    "try_send": "gs::ipc::try_send",
    "send_by_handle": "gs::ipc::send_to",
    "try_send_by_handle": "gs::ipc::try_send_to, or gs::ipc::reply for an answer",
    "send_with_cap_by_handle": "gs::ipc::send_granting",
    "send_peer_at": "gs::ipc::peer_at",
    "park": "gs::ipc::park",
    "request_with_reply": "gs::call::request / request_within",
    "request_with_reply_call_err": "gs::call::request_within",
    "request_with_reply_qhint": "gs::call::request_within_notice",
    "reacquire_by_name": "gs::cap::reacquire",
    "reacquire_cap": "gs::cap::reacquire",
    "acquire_send_cap": "gs::cap::acquire",
    "acquire_send_grant_cap": "gs::cap::acquire_grantable",
    "remove_cap": "gs::cap::remove",
    "derive_cap": "gs::cap::duplicate",
    "self_grant_handle": "gs::cap::self_grant",
    "yield_cpu": "gs::task::yield_now",
    "sleep_ms": "gs::task::sleep_ms, or gs::driver::delay for a hardware hold",
    "sleep": "gs::driver::delay::hold / hold_parked",
    "uptime_secs": "gs::task::uptime_secs",
    "epoch_secs_monotonic": "gs::task::epoch_secs_monotonic",
    "datetime": "gs::task::datetime",
    "core_id": "gs::task::core_id",
    "irq_unmask": "gs::driver::irq",
    "irq_vector": "gs::driver::irq",
    "read_tsc": "gs::driver::wait (Deadline, elapsed_us)",
    "duration_cycles": "gs::driver::wait::Budget",
    "tsc_ticks_per_10ms": "gs::driver::wait::calibrated",
}
CALL = re.compile(r"(?:\bctx|\.ctx)\s*\.\s*(" + "|".join(sorted(REPLACEMENT, key=len, reverse=True)) + r")\s*\(")
COMMENT = re.compile(r"//[^\n]*")


def crates():
    for top in ("services", "examples"):
        base = os.path.join(ROOT, top)
        if not os.path.isdir(base):
            continue
        for name in sorted(os.listdir(base)):
            src = os.path.join(base, name, "src")
            if os.path.isdir(src):
                yield f"{top}/{name}", src


def count(src):
    per = {}
    for dirpath, _, files in os.walk(src):
        for f in sorted(files):
            if not f.endswith(".rs"):
                continue
            text = io.open(os.path.join(dirpath, f), encoding="utf-8", errors="replace").read()
            for m in CALL.finditer(COMMENT.sub("", text)):
                per[m.group(1)] = per.get(m.group(1), 0) + 1
    return per


def load_baseline():
    out = {}
    if not os.path.exists(BASELINE):
        return out
    for line in io.open(BASELINE, encoding="utf-8"):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        n, crate = line.split(None, 1)
        out[crate] = int(n)
    return out


def main():
    found = {crate: count(src) for crate, src in crates()}
    totals = {c: sum(v.values()) for c, v in found.items()}
    if "--report" in sys.argv:
        for c in sorted(found, key=lambda c: -totals[c]):
            if totals[c]:
                print(f"{totals[c]:5}  {c}")
                for meth, n in sorted(found[c].items(), key=lambda kv: -kv[1]):
                    print(f"         {n:4}  ctx.{meth}()  ->  {REPLACEMENT[meth]}")
        return 0
    if "--bless" in sys.argv:
        with io.open(BASELINE, "w", encoding="utf-8", newline="\n") as fh:
            fh.write("# Raw-SDK calls that `gs` already covers, per crate (scripts/one_way_check.py). MAY FALL, MAY\n")
            fh.write("# NOT RISE; a crate absent from this list is held to zero. Rewritten by `--bless`.\n")
            for c in sorted(totals):
                if totals[c]:
                    fh.write(f"{totals[c]:5}  {c}\n")
        print(f"one-way: baseline rewritten - {sum(totals.values())} call(s) in {sum(1 for v in totals.values() if v)} crate(s); say why in the commit.")
        return 0
    base = load_baseline()
    grew, shrank = [], []
    for c, n in sorted(totals.items()):
        allowed = base.get(c, 0)
        if n > allowed:
            grew.append((c, allowed, n))
        elif n < allowed:
            shrank.append((c, allowed, n))
    if grew:
        print("ONE-WAY CHECK FAILED - raw-SDK calls the standard library already covers were ADDED:\n")
        for c, was, now in grew:
            print(f"  {c}: {was} -> {now}")
            for meth, k in sorted(found[c].items(), key=lambda kv: -kv[1]):
                print(f"      {k:4}  ctx.{meth}()  ->  {REPLACEMENT[meth]}")
        print("\nThere is one way to write a service or a driver, and it is `gs` (backlog/71). Use the replacement")
        print("named beside each call. A NEW service starts at zero. If `gs` lacks what you need, the gap is in")
        print("`gs` - add the mechanism there (docs/driver-library.md) rather than reaching past it.")
        return 1
    if shrank:
        print("one-way: " + ", ".join(f"{c} {was}->{now}" for c, was, now in shrank)
              + " - fewer raw-SDK calls; run `--bless` to lock the lower count in.")
    print(f"One-way check passed - {sum(totals.values())} raw-SDK call(s) with a `gs` equivalent remain, "
          f"none added; every crate not in the baseline is at zero.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
