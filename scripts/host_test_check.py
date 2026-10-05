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

Exit: 0 if every listed file compiles and every test in it passes, 1 otherwise.
"""
import os
import subprocess
import sys
import tempfile

# Each entry: a dependency-free Rust source file with a `#[cfg(test)] mod tests`.
FILES = [
    # The AIC8800D80's frames, patch table walk and message parameter blocks (docs/wifi-aic8800.md).
    "services/wifi-driver/src/aic_wire.rs",
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
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
