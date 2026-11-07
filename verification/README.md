# verification/

How the MISCIS kernel is checked, and what a result means (`backlog/90`, "The programme from here"). The
verification lives outside the running system: nothing here is built into an image, and no kernel service exists
to check the kernel.

**Start with [`invariants/registry.md`](invariants/registry.md).** It is the authoritative list: each invariant,
the production code that keeps it, what checks it, how strongly, and what is not established.

## The layers, and how to run each

| Layer | What runs | Command | Where it runs on its own |
|---|---|---|---|
| A. Architectural checks | `scripts/commandments.py`, `scripts/syscall_gate_check.py`, the boundary, seam, scope and unsafe checkers | `cargo run -p osdev --release -- build` (every gate), or `py scripts/build_gates.py` | every `osdev build` |
| B. Host tests over the real kernel files | `cargo test -p kernel --lib`; `tests/host-ipc` (the real `routing.rs`, `queue.rs`, `names.rs`), whose `#[ignore]`d scenarios must still fail | `py scripts/host_test_check.py` | every `osdev build` |
| C. Bounded model checking | `tests/kani`, over the real capability files | in WSL or Linux: `cd tests/kani && cargo kani`; one harness: `cargo kani --harness <name>` | `.github/workflows/verify.yml`, on a pull request touching `kernel/` |
| C. Deductive proof | `tests/creusot`, against contracts written on nine production functions (rights, `Generation::bump`, the IPC queue) | in WSL or Linux: `tests/creusot/prove.sh` | by hand, or in the container |
| D. QEMU suites | the section 22 suites (identity, property, adversarial, chaos, ...) | `cargo run -p osdev --release -- test <suite>` | by hand; x86 QEMU only |
| E. Hardware | the five-board cards recorded beside each fix in `backlog/90` | by hand | none |

The mutation run of the kernel's host tests (`scripts/mutants_check.py`, about 50 minutes) and Miri run only when
their workflows are dispatched.

## Reading a result

Every result in the registry is one of five words, defined at its top: PROVED WITHIN STATED BOUNDS, TESTED - NO
VIOLATION OBSERVED, COUNTEREXAMPLE FOUND, PROOF GAP, NOT EXAMINED. A proof is bounded by what its harness states;
a test is never a proof; an emulator result says nothing about a board. A change that touches an invariant no
check exercises is a coverage gap, and the registry row is where it is written down.

## A failing check

- **A gate** prints what it found and the rule it enforces; fix the code, never the baseline (CLAUDE.md 21).
- **A host test** names the scenario; the `t<N>` files in `tests/host-ipc/tests` say which defect each pins.
- **A Kani harness** prints a counterexample trace: the concrete input that breaks the assertion.
  `cargo kani -Z concrete-playback --concrete-playback=print --harness <name>` turns it into a unit test.

## The container

`verification/Containerfile` is the same Ubuntu, Rust toolchain, Kani and Creusot, pinned, so a failing proof
reruns exactly. Its first lines say how to build and run it.
