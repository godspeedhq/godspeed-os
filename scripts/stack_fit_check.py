"""Refuse to build a service whose stack frames cannot fit the stack it is given.

The Pi 2 learned this the hard way: a DEBUG build gave `fs` a 503 KiB `service_main` frame against a
256 KiB user stack, so it faulted on the first store of its own prologue and crash-looped forever under
the supervisor. `scripts/arm_build.py` grew a gate for it - and that gate lived only on the arm32 build
path, which is the same as not having one (a rule enforced on one path is enforced on none). The Pi 4
had no gate at all.

Two things are borrowed rather than reinvented:

  * from the Pi 2 gate: sum the WHOLE run of stack adjustments, not the first one. A big frame is
    several instructions and only their sum is the real depth; reading the first alone reported 824
    bytes for a 503 KiB frame, which is how it stayed invisible.
  * from Linux (`-Wframe-larger-than=`, CONFIG_FRAME_WARN): check EVERY function, not just the entry
    point. `service_main` is rarely the deepest frame - it is simply the one somebody thought to look
    at. A 200 KiB local in a leaf command is exactly as fatal and far easier to miss.

What this CANNOT do, stated plainly so nobody reads more into a pass than it means: it bounds a SINGLE
frame, not the sum along a call path. Eleven nested 16 KiB frames overflow a 256 KiB stack while every
one of them passes. Bounding the true depth needs a call graph and recursion analysis; this catches the
one-function case, which is the one that has actually bitten twice.
"""
import io
import os
import re
import subprocess

# `sub sp, sp, #N` on ARM; `sub sp, sp, #0xN` and `#0xN, lsl #12` on AArch64. objdump helpfully
# appends `// =0x...` with the effective value for the shifted form, which is preferred when present.
SUB_SP = re.compile(r"\bsub\s+sp,\s*sp,\s*#(0x[0-9a-f]+|\d+)(?:,\s*lsl\s*#(\d+))?")

# RISC-V grows the stack with `addi sp, sp, -N` (and the compressed `c.addi16sp`, which objdump
# renders in the same form). The ARM pattern above requires a `#` immediate, so it matches ZERO of
# them: run unchanged against a riscv64 service it censused 0 frames out of 412 real prologues and
# reported a pass. That is precisely what this file's header calls "a gate that is blind to the case
# it was written for", so the arch is TAUGHT rather than the call site skipped.
SUB_SP_RV = re.compile(r"\baddi\s+sp,\s*sp,\s*-(0x[0-9a-f]+|\d+)")

# x86-64 grows the stack with `sub $0x...,%rsp` (AT&T, objdump's default). This was MISSING, so the
# blind-guard above fired on every x86 service and the gate was simply unavailable on the
# architecture every QEMU suite runs. It is the "a rule enforced on one path is enforced on none"
# problem one layer down: this gate was called from `arm_build.py` and `pi4_build.py`, hand-written
# scripts for two boards, and from nothing else.
#
# It surfaced when the console gained a 64 KiB scrollback ring and there was no way to ask whether
# it fit. That the checker REFUSED rather than reporting a pass is the blind-guard doing exactly its
# job - the answer was "this instrument cannot see here", not "you are fine".
SUB_SP_X86 = re.compile(r"\bsub\s+\$(0x[0-9a-f]+|\d+),%rsp")
EFFECTIVE = re.compile(r"//\s*=(0x[0-9a-f]+)")
FUNC = re.compile(r"^[0-9a-f]+\s+<(.+)>:")


def _amount(line):
    x86 = SUB_SP_X86.search(line)
    if x86:
        raw = x86.group(1)
        return int(raw, 16) if raw.startswith("0x") else int(raw)
    rv = SUB_SP_RV.search(line)
    if rv:
        raw = rv.group(1)
        return int(raw, 16) if raw.startswith("0x") else int(raw)
    m = SUB_SP.search(line)
    if not m:
        return 0
    eff = EFFECTIVE.search(line)
    if eff:
        return int(eff.group(1), 16)
    raw = m.group(1)
    val = int(raw, 16) if raw.startswith("0x") else int(raw)
    if m.group(2):
        val <<= int(m.group(2))
    return val


# LLVM emits a stack PROBE LOOP for a large frame, and the loop body is one page:
#
#     sub x9, sp, #0x23, lsl #12   // =0x23000   <- the real target depth
#     sub sp, sp, #0x1, lsl #12                  <- loop body, ONE page
#     str xzr, [sp]
#     b.ne <back to the sub>
#     sub sp, sp, #0x300                         <- the remainder
#
# Summing `sub sp` statically therefore counts ONE iteration and reports 4,864 bytes for a 144 KiB
# frame - undercounting by 30x, and undercounting precisely the large frames this exists to catch. The
# first version of this checker did exactly that and cleared `shell::pipe_run` at 17 KiB while the ARM
# build, whose prologues are plain sub-sequences with no loop, measured the same function at 143,884.
# A gate that is blind to the case it was written for is not a gate, which is the third time that shape
# has appeared in this cycle.
#
# So the probe TARGET is read too, and the larger of the two readings wins.
PROBE_TARGET = re.compile(r"sub\s+([xw]\d+),\s*sp,\s*#(0x[0-9a-f]+|\d+)(?:,\s*lsl\s*#(\d+))?")

# x86-64 emits the SAME probe loop, in AT&T order and against a scratch register:
#
#     mov    %rsp,%r11            <- the scratch register is SEEDED from rsp
#     sub    $0x1d000,%r11        <- the real target depth, 116 KiB
#     sub    $0x1000,%rsp         <- loop body, ONE page
#     movq   $0x0,(%rsp)
#     jne    <back to the sub>
#     sub    $0x908,%rsp          <- the remainder
#
# Summing only the `%rsp` subtractions therefore reports 6,408 bytes for a 116 KiB frame - an 18x
# undercount, and again precisely on the LARGE frames this file exists to catch. Teaching the
# checker `sub $N,%rsp` WITHOUT this was worse than leaving it blind: blind REFUSES to answer, and
# half-taught answers confidently and wrongly. Measured on `console::service_main`.
#
# IT TAKES TWO INSTRUCTIONS, AND THE FIRST ONE IS WHAT MAKES IT A PROBE. The first attempt here
# matched any `sub $imm,%<not-rsp>` on one line, which is ordinary ARITHMETIC almost everywhere it
# appears: it read `sub $0xfffffffffffffffc,%rax` as an 18-quintillion-byte frame and refused the
# build on five `xhci` functions. Unlike ARM, where `sub x9, sp, #N` names `sp` in the same
# instruction, x86 SEEDS a scratch register from `%rsp` first - so the seed has to be tracked, and
# only a subtraction from THAT register counts.
PROBE_SEED_X86 = re.compile(r"\bmov\s+%rsp,%([a-z0-9]+)\b")
PROBE_SUB_X86 = re.compile(r"\bsub\s+\$(0x[0-9a-f]+|\d+),%([a-z0-9]+)\b")


def _probe_target(line, seed=None):
    """Bytes this line names as a stack-probe target, or 0.

    `seed` is the register most recently loaded from `%rsp` in this function (x86 only); a
    subtraction from any other register is arithmetic and is ignored.
    """
    if seed is not None:
        x86 = PROBE_SUB_X86.search(line)
        if x86 and x86.group(2) == seed:
            raw = x86.group(1)
            return int(raw, 16) if raw.startswith("0x") else int(raw)
    m = PROBE_TARGET.search(line)
    if not m:
        return 0
    eff = EFFECTIVE.search(line)
    if eff:
        return int(eff.group(1), 16)
    raw = m.group(2)
    val = int(raw, 16) if raw.startswith("0x") else int(raw)
    if m.group(3):
        val <<= int(m.group(3))
    return val


def frames(objdump, elf):
    """{function: total bytes of stack it subtracts}. Empty if the ELF cannot be read."""
    r = subprocess.run([objdump, "-d", elf], capture_output=True, text=True)
    if r.returncode != 0:
        return {}
    out, cur, total, probes, seed = {}, None, 0, 0, None
    for line in r.stdout.splitlines():
        m = FUNC.match(line)
        if m:
            if cur is not None:
                out[cur] = max(total, probes)
            cur, total, probes, seed = m.group(1), 0, 0, None
            continue
        if cur is not None:
            total += _amount(line)
            probe = _probe_target(line, seed)
            if probe > probes:
                probes = probe
            # Track the x86 probe seed LAST, so `sub $N,%rX` on the line that also seeds `rX`
            # cannot be read as its own target. Reset per function, above.
            sm = PROBE_SEED_X86.search(line)
            if sm:
                seed = sm.group(1)
    if cur is not None:
        out[cur] = max(total, probes)
    return out


def check(objdump, root, target, profile, services, stack_limit, top=5):
    """Report the deepest frames; return the list of (service, function, bytes) that do not fit."""
    over, census, mute = [], [], []
    for svc in services:
        elf = os.path.join(root, "target", target, profile, svc)
        if not os.path.exists(elf):
            continue
        f = frames(objdump, elf)
        # MEASURED NOTHING, SAID NOTHING. A binary full of functions, not one of which adjusts the
        # stack pointer, does not mean "no deep frames" - it means the prologue form on this target is
        # not one this file matches, which is exactly how an unsupported arch earns a pass. The
        # instrument must report that it is blind rather than report a zero it did not earn
        # (invariant 12). This guard is the general fix; SUB_SP_RV above is the specific one.
        if f and not any(f.values()):
            mute.append(svc)
        for name, size in f.items():
            if size > stack_limit:
                over.append((svc, name, size))
            census.append((size, svc, name))
    if mute:
        raise SystemExit(
            "\nSTACK-FIT CHECK IS BLIND on target %s: %s\n"
            "Every function in those binaries has a zero frame, which no real service has. The\n"
            "stack-pointer prologue on this target is a form this checker does not match, so it\n"
            "measured nothing and would have reported a pass. Add the pattern (see SUB_SP /\n"
            "SUB_SP_RV) before trusting this gate here."
            % (target, ", ".join(mute)))
    census.sort(reverse=True)
    if census:
        print("stack fit: deepest single frames (limit %d KiB)" % (stack_limit // 1024))
        for size, svc, name in census[:top]:
            print("    %7d bytes (%4.1f%%)  %s: %s" % (size, size * 100.0 / stack_limit, svc, name[:60]))
    return over


def enforce(objdump, root, target, profile, services, stack_limit):
    over = check(objdump, root, target, profile, services, stack_limit)
    if over:
        print()
        for svc, name, size in over:
            print("  %-14s %s: frame %d bytes > %d byte stack (over by %d)"
                  % (svc, name, size, stack_limit, size - stack_limit))
        raise SystemExit(
            "\nBUILD REFUSED: the function(s) above cannot fit the user stack. Each faults on the\n"
            "first store of its own prologue, and a service that does that crash-loops forever under\n"
            "the supervisor.\n"
            "Build with --release, or shrink the frame (CLAUDE.md 26.6.1: change the data shape -\n"
            "stream it, refer to it by span, or give it a bounded arena - do not reach for a heap).")


# A COMMAND LINE, so the x86 build can call this the way `arm_build.py` and `pi4_build.py` already
# do. Until now this file was importable only, and only those two hand-written board scripts imported
# it - so the gate existed on two boards and on nothing else, including every QEMU suite. That is
# "a rule enforced on one path is enforced on none", and it is why a 45%-of-stack frame in `console`
# went unnoticed until it was measured by hand.
#
#   python scripts/stack_fit_check.py <target> <profile> <stack-bytes> <service>...
if __name__ == "__main__":
    import sys
    if len(sys.argv) < 5:
        raise SystemExit(
            "usage: stack_fit_check.py <target> <profile> <stack-bytes> <service>...")
    _target, _profile, _limit = sys.argv[1], sys.argv[2], int(sys.argv[3])
    _root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    enforce(os.environ.get("OBJDUMP", "objdump"), _root, _target, _profile, sys.argv[4:], _limit)
