# 24. The §22 adversarial fault tests have only ever run on x86-64, under QEMU

**Severity:** coverage, not correctness. Nothing is broken; a constitutional claim is pinned on one
of four ports and asserted on the other three.
**Status: OPEN.** Recorded rather than closed because closing it is a build-posture decision, not a
missing function.

## The claim, and where it is actually tested

§22 A14 and C2 pin one of the system's louder promises: **a ring-3 CPU fault kills the faulting task
and never the machine** (invariant 12, kernel-audit C1). The executable form is three primitives in
`sdk/rust/src/adversarial.rs`, called by the `probe` service:

| primitive | x86-64 | arm | aarch64 | riscv64 |
|-----------|--------|-----|---------|---------|
| `fault_null_read` | yes (arch-neutral) | yes | yes | yes |
| `fault_noncanonical_read` | `#GP` on a non-canonical address | unmapped high address | the Sv39-style VA hole | **absent** |
| `fault_divide_by_zero` | `div` by zero -> `#DE` | `udf #0` | `udf #0` | **absent** |

So two of the three exist for three ISAs and not the fourth. That is the visible half.

**The half that matters more: none of the non-x86 bodies is reachable by any build that exists.**
`probe` is gated `#[cfg(not(feature = "bare-metal"))]` in the supervisor, and `arm_build.py`,
`pi4_build.py` and `riscv_build.py` all build the supervisor **with** `bare-metal` - correctly, since
§4.4 says a real board ships no adversary. x86 bare-metal images exclude it too (§23.3). So the
adversarial suite runs in exactly one configuration: **x86-64 under QEMU.**

The arm and aarch64 bodies were written, reviewed and have never executed. They are not wrong - they
encode real per-ISA knowledge (ARMv7 has no non-canonical form so an unmapped high address is the
analog; neither ARM port traps integer divide, so `udf #0` is; AArch64's VA hole is the true
equivalent of x86 non-canonical) - they are simply unexercised, which is the state §26.2 warns about:
code that is not run does not stay working.

## What IS evidence, so the gap is stated fairly

The property itself is observed constantly on the non-x86 ports, just not as a test. The Pi 4 log from
2026-09-13 has the shell taking an EL0 instruction abort at `0x2020202020202020`, the kernel printing
a full dump, killing the task, and every other service continuing - `backlog/22` / the shell-smash
memory carry the detail. `chaos max-carnage` kills hundreds of tasks per run on all four ports without
a panic. So "a ring-3 fault kills the task, not the machine" is **demonstrated** on ARM and RISC-V by
accident and by chaos; what is missing is that it is **pinned** there.

The difference is the one §22.1 exists to make: a demonstration tells you it worked once; a pinned
test tells you when it stops.

## What is ruled out

- **Not missing assembly.** Two thirds of the per-ISA work is already done and committed.
- **Not a riscv64 seam gap.** riscv64 answers every `arch::imp` member (`arch_seam_check.py`); this is
  userspace.
- **Not the reason `riscv_build.py` used to give.** Its comment said the primitives "are x86
  instructions", which was true when written and false since the ARM ports landed. Corrected in the
  same commit as this entry, because it made a build-posture decision read as a blocked port.

## The next concrete step, and what it costs

Two separable pieces, in this order:

1. **Write the riscv64 bodies** (small, and checkable without hardware): the non-canonical analog is an
   address in the Sv39 hole - bits 63:39 not a sign-extension of bit 38 - and since RISC-V integer
   division by zero does **not** trap (it returns all-ones by specification), the divide primitive
   becomes an illegal instruction, `unimp`. That is the same substitution both ARM ports already made.
2. **Decide how the adversarial suite runs off x86 at all** - which is the actual blocker and is a
   posture question, not a coding one. `bare-metal` excluding `probe` is right for a shipping image,
   so this needs either a non-bare-metal ARM/RISC-V build used only by the harness, or the two or
   three fault cases lifted into something that does ship (the `chaos` service already ships and
   already kills tasks deliberately, so it is the closer fit).

Doing 1 without 2 adds two functions nothing calls, on a port that already has none reachable - which
is why they are recorded together rather than half-done (§26.7).
