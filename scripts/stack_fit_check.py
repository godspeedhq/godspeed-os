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
EFFECTIVE = re.compile(r"//\s*=(0x[0-9a-f]+)")
FUNC = re.compile(r"^[0-9a-f]+\s+<(.+)>:")


def _amount(line):
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


def _probe_target(line):
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
    out, cur, total, probes = {}, None, 0, 0
    for line in r.stdout.splitlines():
        m = FUNC.match(line)
        if m:
            if cur is not None:
                out[cur] = max(total, probes)
            cur, total, probes = m.group(1), 0, 0
            continue
        if cur is not None:
            total += _amount(line)
            probe = _probe_target(line)
            if probe > probes:
                probes = probe
    if cur is not None:
        out[cur] = max(total, probes)
    return out


def check(objdump, root, target, profile, services, stack_limit, top=5):
    """Report the deepest frames; return the list of (service, function, bytes) that do not fit."""
    over, census = [], []
    for svc in services:
        elf = os.path.join(root, "target", target, profile, svc)
        if not os.path.exists(elf):
            continue
        for name, size in frames(objdump, elf).items():
            if size > stack_limit:
                over.append((svc, name, size))
            census.append((size, svc, name))
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
