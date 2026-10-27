#!/usr/bin/env python3
"""Run the host-side unit tests of driver code that cannot otherwise be tested off the board.

WHY THIS EXISTS. A service is built only for its target, so a `#[cfg(test)]` module inside one never
runs - `cargo test` cannot build a `no_std` service binary for the host. For most code that is fine: the
board is the test. For a driver's WIRE FORMAT it is not. A wrong byte offset in a message the chip has
never seen costs a flash to find, and the AIC8800's bring-up messages are hundreds of bytes of fields
read from a vendor driver.

So a driver keeps its byte-level code in a file that names nothing outside `core` (no SDK, no other
module), and this compiles that one file with `rustc --test` and runs it. No crate, no workspace entry,
nothing for the other checkers to account for - one file, one compiler invocation.

THE LIST IS EXPLICIT for the reason `EXTRA_CHECKS` is: adding a file is a decision, not a side effect of
a directory listing. A listed file that is missing is a failure, not a skip.

Exit: 0 if every listed file compiles and every test in it passes, and `cargo test -p kernel --lib`
passes, 1 otherwise.
"""
import os
import re
import subprocess
import sys
import tempfile

# Each entry: a dependency-free Rust source file with a `#[cfg(test)] mod tests`.
FILES = [
    # The AIC8800D80's frames, patch table walk and message parameter blocks (docs/wifi-aic8800.md).
    "services/wifi-driver/src/aic_wire.rs",
    # The RTL8188CUS firmware file: its header, its hash, and the page/block plan of its download
    # (docs/wifi-usb.md, R2).
    "services/wifi-usb/src/rtl_fw.rs",
    # Its transmit queues: the endpoint-to-queue map, the page reservation and the priority word (R2).
    "services/wifi-usb/src/rtl_queues.rs",
    # Its bulk IN transfers: the receive descriptor, the 128-byte packet walk and the signal (R3b).
    "services/wifi-usb/src/rtl_rx.rs",
    # Its transmit descriptor, for a management frame, signed (R5a).
    "services/wifi-usb/src/rtl_tx.rs",
    "services/wifi-usb/src/rtl_power.rs",
    # A beacon or probe response read from the raw frame, for every radio that forwards frames.
    "sdk/wifi/src/mgmt.rs",
    # An 802.11 data frame and the ethernet frame it carries, both ways (moved from aic_wire.rs, R5c).
    "sdk/wifi/src/data.rs",
    # `ask`'s vocabulary and every truthfulness rule: what a question is read as, and what each answer
    # must and must not say for fixtures of source replies (docs/ask-design.md 8).
    "services/ask/src/know.rs",
    "services/godspeed-pm/src/config.rs",
    "services/godspeed-pm/src/http.rs",
    "services/godspeed-pm/src/toml_lines.rs",
    "services/time/src/ntp_file.rs",
    # A progress line narrowed to the console: the bar gives way first (2026-10-19).
    "services/shell/src/fit.rs",
]


def main() -> int:
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    failed = 0
    with tempfile.TemporaryDirectory() as tmp:
        for rel in FILES:
            src = os.path.join(root, rel)
            if not os.path.exists(src):
                print(f"host tests: {rel} is listed and does not exist")
                failed += 1
                continue
            exe = os.path.join(tmp, os.path.basename(rel).replace(".rs", "") + (".exe" if os.name == "nt" else ""))
            c = subprocess.run(["rustc", "--edition", "2021", "--test", src, "-o", exe],
                               capture_output=True, text=True)
            if c.returncode != 0:
                print(f"host tests: {rel} does not compile on its own (it must name nothing outside core):")
                print(c.stderr)
                failed += 1
                continue
            r = subprocess.run([exe], capture_output=True, text=True)
            summary = [l for l in r.stdout.splitlines() if l.startswith("test result:")]
            if r.returncode != 0:
                print(f"host tests: {rel} FAILED")
                print(r.stdout)
                failed += 1
            else:
                print(f"host tests: {rel} - {summary[0] if summary else 'ok'}")
    # THE KERNEL'S OWN HOST TESTS (backlog/90 E3). `kernel/src/lib.rs` compiles the hardware-free half of
    # the kernel for the host - the capability table, the rights, the routing and naming models, the clock
    # deglitch - and its unit and property tests ran on no automatic path at all: written, green, and run
    # only when someone remembered. A test nobody runs is not evidence. About 35 s cold, a few warm.
    r = subprocess.run(["cargo", "test", "-q", "-p", "kernel", "--lib"], cwd=root,
                       capture_output=True, text=True)
    summary = [l for l in r.stdout.splitlines() if l.startswith("test result:")]
    if r.returncode != 0 or not summary:
        print("host tests: cargo test -p kernel --lib FAILED")
        print(r.stdout[-4000:])
        print(r.stderr[-4000:])
        failed += 1
    else:
        print(f"host tests: kernel --lib - {summary[-1]}")
    failed += host_ipc(root)
    return 1 if failed else 0


def host_ipc(root):
    """tests/host-ipc: the REAL routing code on the host (backlog/90 E4), and its known defects.

    Two runs. The ordinary one must pass: positive controls, and the real files' own unit tests. Then
    `--ignored`, where every test is a DEFECT the 2026-10-16 audit found, marked with its backlog/90 id,
    and each must still FAIL. One that passes means its defect was fixed - or the scenario stopped
    exercising it - and either way the ignore must go, so the test becomes the fix's regression test
    rather than a reproduction nobody runs.
    """
    crate = os.path.join(root, "tests", "host-ipc")
    bad = 0
    # The stubbed constants must match the kernel's, or the harness tests a different table size.
    with open(os.path.join(crate, "src", "lib.rs"), encoding="utf-8") as f:
        stubs = f.read()
    for const, home in (("MAX_TASKS", "scheduler.rs"), ("PEER_NAME_BYTES", "mod.rs")):
        with open(os.path.join(root, "kernel", "src", "task", home), encoding="utf-8") as f:
            k = re.search(r"pub const %s: usize = (\d+);" % const, f.read())
        h = re.search(r"pub const %s: usize = (\d+);" % const, stubs)
        if not k or not h or k.group(1) != h.group(1):
            print(f"host tests: tests/host-ipc stubs {const} = {h and h.group(1)}, the kernel has "
                  f"{k and k.group(1)} - they must agree")
            bad += 1
    r = subprocess.run(["cargo", "test", "-q", "--no-fail-fast"], cwd=crate, capture_output=True, text=True)
    if r.returncode != 0:
        print("host tests: tests/host-ipc FAILED")
        print(r.stdout[-4000:])
        print(r.stderr[-4000:])
        return bad + 1
    passed = sum(int(m) for m in re.findall(r"test result: ok\. (\d+) passed", r.stdout))
    # How many reproductions the SOURCE holds, counted before running them: with none left (every
    # known defect fixed and its test unignored), there is nothing for `--ignored` to run, and that is
    # the good outcome, said as such. With some, a run that reports none is still a failure below.
    tests_dir = os.path.join(crate, "tests")
    marked = 0
    for name in sorted(os.listdir(tests_dir)):
        if name.endswith(".rs"):
            with open(os.path.join(tests_dir, name), encoding="utf-8") as f:
                marked += len(re.findall(r"^\s*#\[ignore\b", f.read(), re.M))
    if marked == 0:
        print(f"host tests: tests/host-ipc - {passed} passed, no known defect left to reproduce")
        return bad
    r = subprocess.run(["cargo", "test", "--no-fail-fast", "--", "--ignored"], cwd=crate,
                       capture_output=True, text=True)
    outcomes = re.findall(r"^test (\S+) \.\.\. (ok|FAILED)$", r.stdout, re.M)
    if not outcomes:
        print("host tests: tests/host-ipc --ignored ran nothing - the defect scenarios cannot be read")
        print(r.stdout[-2000:])
        return bad + 1
    for name, outcome in outcomes:
        if outcome == "ok":
            print(f"host tests: tests/host-ipc `{name}` PASSES now. It reproduced a backlog/90 defect; if the "
                  f"fix landed, delete its #[ignore] so it guards the fix, and close the defect")
            bad += 1
    print(f"host tests: tests/host-ipc - {passed} passed, {len(outcomes)} known defect(s) still reproduce")
    return bad


if __name__ == "__main__":
    sys.exit(main())
