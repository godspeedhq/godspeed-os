#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Every syscall arm is GATED by a capability check, or ungated BY RECORD (backlog/90 C4, E2).

    python syscall_gate_check.py                       check the tree's kernel/src/syscall/dispatch.rs
    python syscall_gate_check.py --file PATH           check a copy (the negative tests use this)
    python syscall_gate_check.py --root REPO           point at a checkout (default: the godspeed tree)
    python syscall_gate_check.py --report              print the full per-arm table even when green
    python syscall_gate_check.py --print-pin           print GATE_PIN as the tree has it, after a deliberate change
    python syscall_gate_check.py --selftest            the negative tests alone, verbose: mutants of the real
                                                       dispatch.rs (a plain run also runs them, quietly)

Landed 2026-10-26 from the prototype the 2026-10-16 audit wrote and mutation-tested (its evidence copy
is outside this repository). Before it, 15 gate mutations of dispatch.rs were caught by 0 checks: the
I-* pins in commandments.py read names, numbers and id sets, and nothing looked inside a handler.

LEVEL: ARCHITECTURAL_CHECK (source-level, text). It reads Rust as text after blanking comments and
string literals. It never compiles, never runs the kernel, and never sees macro expansions.

WHAT scripts/commandments.py ALREADY CHECKS, so this does not repeat it:
  I-syscalls     the NAMES and NUMBERS in `enum SyscallNumber` equal the pin. Nothing about gating.
  I-introspect   the SET of InspectKernel query ids equals the pin. Nothing about which are ungated.
  I-authorities  the SET of well-known `*_RESOURCE` ids equals the pin. Nothing about who checks them.
None of the three looks inside a handler. A syscall whose gate is deleted keeps its name, its number
and its resource id, so all three stay green over exactly the regression this file exists to catch.

WHAT THIS CHECKS (each is a FAIL unless stated):
  S1  every enum variant has a dispatcher arm, every arm names a variant, a default arm returns < 0.
  S2  every arm is GATED by a recognised check, or is in ALLOW_UNGATED with a reason. An allowlist
      entry for an arm that is now gated, or names no syscall, also fails (the list may only shrink).
  S3  no call that is not known to be read-only precedes the arm's first gate (side effect before gate).
      Calls are resolved into their definitions (neutral kernel, and every arch/<isa> separately) and
      scanned for effect markers, following bare AND module-path calls to depth 3 (a same-file-only
      scan missed `kill_by_name -> scheduler::kill_task_by_slot`, mutant M13); unresolvable calls and
      unknown macros count as effects (fail closed). Inline effects (a static assignment, an atomic
      .store, write_volatile, asm!) written directly above a gate count too.
      A handler with no gate of its own whose FIRST effectful act is a call to a gated local helper
      inherits that helper's gate (`Call` -> `do_call`); one level only.
  S4  a gate's failing branch RETURNS (a gate that logs and falls through is not a gate).
  S5  a gate's right is a constant `Rights::X`, or the arm is in ALLOW_RUNTIME_RIGHT with a reason
      (an empty `Rights(0)` passes `Rights::contains` vacuously - capability/rights.rs:24-26).
  S6  InspectKernel: every query in the ungated `matches!` list is in ALLOW_UNGATED_QUERY with a
      reason AND a declared effect set; the effect markers found in the query's callees (scanned on
      every arch) must be within that set. A declared-ungated id with no arm fails.
  S7  multi-op syscalls: every op arm after the gate is attributed to the gate resource that covers it
      (PowerOff resolves its per-op resource from `let reboot = op == 3 || op == 4`).
  S8  error paths: `Err(..) => 0`, `None => 0`, `else { return 0 }`, `.is_err() { .. return 0 }` and
      `let _ = <call>` in any function reachable from an arm must be in ALLOW_ZERO with a reason.
  S9  GATE_PIN: each arm's full gate signature (EVERY returning gate in order, so a deleted second
      gate such as SpawnImage's IMAGE_SPAWN fails - mutant M18) equals the recorded one (a RECORD, as in
      COMMANDMENTS.baseline.toml: a change is not wrong, it must be deliberate).
  INFO  post-effect error returns (an error returned after a state-changing call) are LISTED, not
      failed: they are where "failed operations are not atomic" (backlog/90) bites.

RECOGNISED GATE VOCABULARY (anything else is "no gate" and fails S2 - a new kind of gate is a visible
change to this list, never a silent pass):
  SLOT   `match scheduler::current_task_lookup_cap(slot, Rights::R) { .. Err(..) => return .. }`
         optionally scoped by `if cap.resource_id != X { return .. }`
  HOLD   `if !scheduler::current_task_holds_resource(X, Rights::R) { .. return .. }`, also through a
         `let v = ..holds_resource(..)` then `if !v ..`, and through a resource chosen by `if`.
  OWNER  `registered_endpoint(irq) != my_ep` with a return, or `delegated::revoke_owned(id, owner)`
         (check-and-act inside the callee; NOT verified here).

WHAT THIS DOES NOT ESTABLISH, said next to every result:
  - that the gate checks the RIGHT resource for the action (S9 only pins it), or that the object the
    arm acts on is the object the gate validated (a gate on slot A and an action on an argument B
    passes; see mutant M10);
  - anything inside a helper or macro the arm calls AFTER its gate;
  - the semantics of `holds_resource` (no generation check, capability/table.rs:115) or of the arch
    functions an arm calls (the effect scan is a marker grep, depth 3, name-resolved without types,
    and can miss or over-report);
  - that a gate's CONDITION is the right one: an extra conjunct that weakens it passes (mutant M17);
  - that the PURE list is true: it is trusted by name, and a change inside one of those functions is
    not seen;
  - that a pure-looking call is pure on every arch beyond the markers listed in EFFECT_MARKERS.
"""

import os
import re
import sys

DEFAULT_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__))).replace("\\", "/")

# ------------------------------------------------------------------------------------------------
# Records. They live HERE, in the gate, rather than in COMMANDMENTS.baseline.toml, deliberately: a
# change to which syscall is ungated, or to a gate's signature, is then an edit to this checker's source,
# made in the open (CLAUDE.md 21, "weakens a gate"). Every entry carries a reason; an entry without one
# is refused at load.
# ------------------------------------------------------------------------------------------------

ALLOW_UNGATED = {
    "Yield": "advisory reschedule of the caller's own task (CLAUDE.md 9.3); no resource, no authority",
    "Sleep": "blocks only the caller (9.3, 26.6); arms the caller's own hi-res slot, keyed by its task slot",
    "AllocMem": "implicit authority: the caller's own memory budget fixed by its spawn request "
                "(10.2, 13.6), bounded in scheduler::current_task_claim_alloc",
    "Park": "blocks the caller itself (scheduler::park_current = block_and_reschedule)",
    "TakePendingCap": "pops the caller's own pending-received-cap FIFO; can reveal only its own slots",
    "QueryCapRights": "reads the rights of a slot in the caller's own table",
    "RemoveCap": "drops a cap from the caller's own table; can only SHRINK the caller's authority",
    "LastRecvBadge": "takes the caller's own last-recv badge (kernel-set, 7.10)",
    "Exit": "ends the CALLER and no one else, by the path a self-kill already takes; it takes no "
            "capability because it reaches no authority (CLAUDE.md 14.5 amendment 2026-10-17)",
}

# Arms whose gate right is computed from an argument rather than a `Rights::X` constant.
ALLOW_RUNTIME_RIGHT = {
    "ResourceInvoke": "the right is the caller's claim, validated against the cap and carried to the "
                      "owner as badge_right; Rights(0) passes vacuously, so the OWNER must refuse a "
                      "badge right of 0 (finding F-invoke-empty-right)",
}

# Ungated InspectKernel queries. `effects` is the set of marker classes the query's callees may show
# on ANY arch (see EFFECT_MARKERS). Declaring a class is a statement that the effect is intended.
ALLOW_UNGATED_QUERY = {
    0:  dict(why="the caller's own alloc bytes", effects=set()),
    3:  dict(why="the cycle counter; discloses no task state", effects={"asm"}),
    10: dict(why="input-ready flag, a boot fact", effects=set()),
    11: dict(why="RTC read; x86 selects the CMOS index register with an out (device_write), and since "
                 "the audit holds the CMOS lock across the index/data pair, so concurrent readers cannot "
                 "interleave (finding F-q11-17-cmos-index-race, fixed)",
             effects={"device_read", "device_write", "asm", "lock"}),
    12: dict(why="boot datetime captured once", effects=set()),
    13: dict(why="is the CALLER the console foreground; on arm and aarch64(pi4) it also RELEASES a "
                 "lapsed claim, wakes the waiter and writes the notice to the UART (known: "
                 "audits/kernel-audit.md A8-19)",
             effects={"store", "wake", "log", "asm", "device_read", "device_write", "lock"}),
    14: dict(why="NIC vendor/device from the boot PCI scan", effects=set()),
    29: dict(why="which SoC ethernet controller the boot probe found (backlog/fixed/21): query 14's twin for a MAC on no bus",
             effects=set()),
    15: dict(why="NIC BAR from the boot PCI scan", effects={"device_read", "device_write", "lock"}),
    16: dict(why="TSC ticks per quantum, a calibration constant (aarch64 reads the counter frequency "
                 "with an mrs)", effects={"asm"}),
    17: dict(why="deglitched monotonic epoch; x86 STORES the high-water mark LAST_NOW_EPOCH, so every "
                 "caller moves a shared filter, reads the clock under the CMOS lock, and logs once when a "
                 "glitched read starts and ends a hold (rtc.rs now_epoch_monotonic)",
             effects={"store", "device_read", "device_write", "asm", "lock", "log"}),
    18: dict(why="driver-presence bits from the boot PCI scan", effects=set()),
    19: dict(why="hardware random word; reading CONSUMES the RNG FIFO, and on aarch64(pi4) an ungated "
                 "caller can drive the RNG200 enable and its soft-reset restart sequence and a bounded "
                 "200000-read spin (finding F-q19-ungated-device-writes)",
             effects={"device_read", "device_write", "store", "static_write", "asm", "log", "lock"}),
    20: dict(why="EMMC base clock learned at boot", effects=set()),
    23: dict(why="the board MAC, a hardware identity fact", effects={"device_read", "device_write", "store", "lock", "asm", "log"}),
    28: dict(why="pending power action, ungated by design so a planted deadline cannot be hidden "
                 "(docs/shutdown.md 6)", effects=set()),
}

# Error-path sites returning 0 / discarding a result. Keyed by (function, normalised source line).
ALLOW_ZERO = {
    ("deliver_to_console_service", "letSome(ep)=scheduler::live_endpoint_of_kind(crate::task::kind::FRAMEBUFFER)else{return0};"):
        "no terminal on this machine: serial already has the bytes (11.4 floor)",
    ("deliver_to_console_service", "letOk(msg)=crate::ipc::message::Message::new(bytes)else{return0};"):
        "bytes <= 256 always fit a message; the else is unreachable, but a refusal here would be a SILENT "
        "drop of the display copy (serial still has it)",
    ("deliver_to_console_service", "Err(_)=>{"):
        "terminal died between lookup and enqueue; counted and reported every 100th (CONSOLE_LOST)",
    ("handle_sleep", "let_=scheduler::block_and_reschedule(TaskState::BlockedOnRecv);"):
        "a sleep woken early reports success: see finding F-sleep-early-wake-0",
    ("do_call", "let_=scheduler::block_and_reschedule(TaskState::BlockedOnRecv);"):
        "the wake result is not the outcome: the loop asks call_dequeue again, which returns the reply, "
        "ReplyDead or EndpointDead from the routing state, and a delivered reply must not lose to a death "
        "that overwrote its wake (backlog/90 D6)",
    ("handle_resource_mint", "let_=delegated::revoke_owned(id,owner);"):
        "cleanup after a failed copy-out; the id was just allocated by this owner",
}

# Calls proven read-only enough to precede a gate (by reading them; reason given). Everything else
# that precedes a gate is resolved and scanned for effect markers.
PURE = {
    "read_user_bytes": "copies from validated user memory into a kernel view; no kernel state written",
    "validate_user_ptr": "range check only",
    "from_utf8": "core::str",
    "current_task_slot": "atomic load of CORE_CURRENT",
    "current_task_endpoint": "atomic loads (scheduler.rs current_task_endpoint)",
    "current_core_id": "per-core id read",
    "registered_endpoint": "interrupt route table read",
    "task_name": "reads the task name slot",
}

# A gate pin (RECORD): arm -> signature. Regenerate with --print-pin after a deliberate change.
GATE_PIN = {
    "Send": "SLOT(SEND)",
    "Recv": "SLOT(RECV)",
    "TrySend": "SLOT(SEND)",
    "Yield": "UNGATED",
    "Log": "SLOT(WRITE)@LOG_WRITE_RESOURCE",
    "AllocMem": "UNGATED",
    "Kill": "HOLD(SERVICE_CONTROL_RESOURCE,WRITE)",
    "AcquireSendCap": "HOLD(ACQUIRE_ANY_RESOURCE,WRITE)|PEER",
    "SendWithCap": "SLOT(SEND)+SLOT(GRANT)",
    "TakePendingCap": "UNGATED",
    "InspectKernel": "HOLD(INTROSPECT_RESOURCE,READ)",
    "QueryCapRights": "UNGATED",
    "RemoveCap": "UNGATED",
    "TaskStat": "HOLD(INTROSPECT_RESOURCE,READ)",
    "ConsoleRead": "SLOT(READ)@CONSOLE_READ_RESOURCE",
    "Reboot": "HOLD(REBOOT_RESOURCE,WRITE)",
    "ConsolePush": "SLOT(WRITE)@CONSOLE_PUSH_RESOURCE",
    "Park": "UNGATED",
    "Print": "SLOT(WRITE)@LOG_WRITE_RESOURCE",
    "ConsoleWrite": "SLOT(WRITE)@LOG_WRITE_RESOURCE",
    "TryConsoleRead": "SLOT(READ)@CONSOLE_READ_RESOURCE",
    "ConsoleEcho": "SLOT(READ)@CONSOLE_READ_RESOURCE",
    "ConsoleBootComplete": "SLOT(READ)@CONSOLE_READ_RESOURCE",
    "SignalInputReady": "SLOT(WRITE)@CONSOLE_PUSH_RESOURCE",
    "TaskCaps": "HOLD(INTROSPECT_RESOURCE,READ)",
    "DeriveCap": "SLOT(GRANT)",
    "ResourceMint": "HOLD(RESOURCE_MINT_RESOURCE,WRITE)",
    "ResourceInvoke": "SLOT(<runtime>)+SLOT(GRANT)",
    "ResourceRevoke": "OWNER(delegated)",
    "LastRecvBadge": "UNGATED",
    "TryRecv": "SLOT(RECV)",
    "RecvTimeout": "SLOT(RECV)",
    "IrqUnmask": "OWNER(irq-route)",
    "Sleep": "UNGATED",
    "ConsoleForeground": "SLOT(READ)@CONSOLE_READ_RESOURCE",
    "Call": "SLOT(SEND)+SLOT(GRANT)+SLOT(RECV)",
    "CallDeadline": "SLOT(SEND)+SLOT(GRANT)+SLOT(RECV)",
    "FireIrq": "HOLD(FIRE_IRQ_RESOURCE,WRITE)",
    "Gpio": "HOLD(GPIO_DEVICE_RESOURCE,WRITE)",
    # + the console read a flag asks for, held with GRANT like every privilege bit (backlog/90 D22, 2026-10-29)
    "SpawnImage": "SLOT(WRITE)@SPAWN_RESOURCE+HOLD(IMAGE_SPAWN_RESOURCE,WRITE)+HOLD(CONSOLE_READ_RESOURCE,GRANT)+SLOT(GRANT)",
    "PciCfgRead": "HOLD(PCI_CFG_RESOURCE,READ)",
    "DevicePower": "HOLD(DEVICE_POWER_RESOURCE,WRITE)",
    "CpuClock": "HOLD(CPU_CLOCK_RESOURCE,WRITE)",
    "PowerOff": "HOLD({POWER_OFF_RESOURCE,REBOOT_RESOURCE},WRITE)",
    "Exit": "UNGATED",
}

# Effect markers, by class. A marker grep, not a semantics: see the module docstring.
EFFECT_MARKERS = [
    ("store",        re.compile(r"\.(?:store|swap|fetch_\w+|compare_exchange\w*)\s*\(")),
    ("static_write", re.compile(r"(?<![\w.])[A-Z][A-Z0-9_]{2,}\s*(?:\[[^\]\n]*\])*\s*(?:\.\d+\s*)?(?:=|\+=|-=|\|=|&=)(?![=>])")),
    ("static_write", re.compile(r"\b(?:assume_init_mut|as_mut_ptr|get_mut)\s*\(|\bptr::write\w*\s*\(|&mut\s+[A-Z][A-Z0-9_]{2,}")),
    ("device_write", re.compile(r"\b(?:write_volatile|outb|outw|outl|mmio_write\w*|write32|write64|wr32)\s*\(")),
    ("device_read",  re.compile(r"\b(?:read_volatile|inb|inw|inl|mmio_read\w*|read32|read64|rd32)\s*\(")),
    ("wake",         re.compile(r"\b(?:wake_by_slot|wake_console_waiter|block_and_reschedule)\s*\(")),
    ("log",          re.compile(r"\b(?:kprintln!|kprint!|put_str\s*\(|pl011_write\w*\s*\(|serial_write\w*\s*\()")),
    ("lock",         re.compile(r"\.(?:lock|lock_irq|try_lock)\s*\(")),
    ("asm",          re.compile(r"\basm!\s*\(")),
]
EFFECTFUL_CLASSES = {"store", "static_write", "device_write", "device_read", "wake", "lock", "asm"}

KEYWORDS = {"if", "while", "for", "match", "return", "loop", "let", "fn", "as", "in", "else",
            "Some", "Ok", "Err", "None", "move", "break", "continue", "unsafe", "mut", "ref", "where"}
LOCAL_METHOD_EFFECT = {"store", "swap", "fetch_add", "fetch_sub", "fetch_or", "fetch_and", "fetch_xor",
                       "fetch_update", "compare_exchange", "compare_exchange_weak", "lock", "lock_irq",
                       "try_lock"}
PURE_MACROS = {"matches", "format_args", "assert", "debug_assert", "cfg"}
LOG_MACROS = {"kprintln", "kprint"}

# ------------------------------------------------------------------------------------------------
# Rust as text
# ------------------------------------------------------------------------------------------------

def strip_rust(src):
    """Blank comments and the CONTENTS of string/char literals; keep offsets and newlines."""
    out = list(src)
    n = len(src)

    def blank(a, b):
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    i = 0
    while i < n:
        c = src[i]
        if c == "/" and i + 1 < n and src[i + 1] == "/":
            j = src.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
            continue
        if c == "/" and i + 1 < n and src[i + 1] == "*":
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank(i, j)
            i = j
            continue
        if c == "r" and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")) and i + 1 < n \
                and src[i + 1] in '#"':
            j, hashes = i + 1, 0
            while j < n and src[j] == "#":
                hashes, j = hashes + 1, j + 1
            if j < n and src[j] == '"':
                end = src.find('"' + "#" * hashes, j + 1)
                end = n if end < 0 else end
                blank(j + 1, end)
                i = end + 1 + hashes
                continue
        if c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            blank(i + 1, j)
            i = j + 1
            continue
        if c == "'":
            if i + 1 < n and src[i + 1] == "\\":
                j = src.find("'", i + 3)
                j = n if j < 0 else j
                blank(i + 1, j)
                i = j + 1
                continue
            if i + 2 < n and src[i + 2] == "'":
                blank(i + 1, i + 2)
                i += 3
                continue
        i += 1
    return "".join(out)


def match_close(t, i):
    depth = 0
    for j in range(i, len(t)):
        ch = t[j]
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
            if depth == 0:
                return j
    return -1


def line_of(t, off):
    return t.count("\n", 0, off) + 1


def split_top(s, sep=","):
    parts, depth, cur = [], 0, []
    for ch in s:
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        if ch == sep and depth == 0:
            parts.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    parts.append("".join(cur))
    return [p.strip() for p in parts]


FN_RE = re.compile(r"\bfn\s+([A-Za-z_]\w*)\s*(?:<[^>{(]*>)?\s*\(")


class Fn:
    def __init__(self, name, path, t, popen, pclose, bopen, bclose):
        self.name, self.path, self.t = name, path, t
        self.params = t[popen + 1:pclose]
        self.bopen, self.bclose = bopen, bclose
        self.line = line_of(t, bopen)

    @property
    def body(self):
        return self.t[self.bopen + 1:self.bclose]


def index_functions(t, path):
    fns = {}
    for m in FN_RE.finditer(t):
        popen = m.end() - 1
        pclose = match_close(t, popen)
        if pclose < 0:
            continue
        bopen = t.find("{", pclose)
        semi = t.find(";", pclose)
        if bopen < 0 or (0 <= semi < bopen):
            continue
        bclose = match_close(t, bopen)
        if bclose < 0:
            continue
        fns.setdefault(m.group(1), []).append(Fn(m.group(1), path, t, popen, pclose, bopen, bclose))
    return fns


def stmt_start(t, off, floor):
    """Start of the statement containing `off`: just after the last ; { or } before it."""
    k = off - 1
    while k > floor:
        if t[k] in ";{}":
            return k + 1
        k -= 1
    return floor


def parse_arm_expr(t, p, limit):
    """From p (after `=>`), return (expr_start, expr_end_exclusive, next_pos)."""
    while p < limit and t[p].isspace():
        p += 1
    if p < limit and t[p] == "{":
        e = match_close(t, p)
        q = e + 1
        while q < limit and t[q].isspace():
            q += 1
        if q < limit and t[q] == ",":
            q += 1
        return p, e + 1, q
    depth, q = 0, p
    while q < limit:
        ch = t[q]
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            if depth == 0:
                return p, q, q
            depth -= 1
        elif ch == "," and depth == 0:
            return p, q, q + 1
        q += 1
    return p, limit, limit


def split_arms(t, a, b):
    """Arms of a match body t[a:b]: list of (pat, pat_off, expr, expr_off, expr_end)."""
    arms, p = [], a
    while p < b:
        while p < b and (t[p].isspace() or t[p] == ","):
            p += 1
        if p >= b:
            break
        depth, q = 0, p
        while q < b - 1:
            ch = t[q]
            if ch in "([{":
                depth += 1
            elif ch in ")]}":
                depth -= 1
            elif depth == 0 and t.startswith("=>", q):
                break
            q += 1
        if not t.startswith("=>", q):
            break
        pat = t[p:q].strip()
        es, ee, nxt = parse_arm_expr(t, q + 2, b)
        arms.append((pat, p, t[es:ee].strip(), es, ee))
        p = nxt
    return arms

# ------------------------------------------------------------------------------------------------
# Effect resolution
# ------------------------------------------------------------------------------------------------

class Universe:
    """Every kernel source file, stripped, with a function index: neutral and per arch."""

    def __init__(self, root):
        self.root = root
        ksrc = os.path.join(root, "kernel", "src")
        self.neutral = {}           # name -> [Fn]
        self.arch = {}              # isa -> {name -> [Fn]}
        self.files = {}
        for base, _, names in os.walk(ksrc):
            for n in names:
                if not n.endswith(".rs"):
                    continue
                full = os.path.join(base, n)
                rel = os.path.relpath(full, root).replace("\\", "/")
                with open(full, encoding="utf-8", errors="replace") as f:
                    t = strip_rust(f.read())
                self.files[rel] = t
                fns = index_functions(t, rel)
                parts = rel.split("/")
                if len(parts) > 4 and parts[2] == "arch":
                    tgt = self.arch.setdefault(parts[3], {})
                elif len(parts) == 4 and parts[2] == "arch":
                    continue        # arch/mod.rs is the seam alias, not an ISA
                else:
                    tgt = self.neutral
                for k, v in fns.items():
                    tgt.setdefault(k, []).extend(v)

    SCAN_DEPTH = 3

    def resolve(self, fn, table, path, name):
        """Callees for `path::name(` written inside fn. Bare: same file, else anywhere in the table.
        A module path prefers the file named after its last segment (`scheduler::x` -> scheduler.rs);
        `arch::imp` from neutral code is not followed here (effects_of_call does that per arch)."""
        cands = table.get(name, [])
        if not cands or "arch::imp" in path:
            return []
        if not path:
            same = [c for c in cands if c.path == fn.path]
            return same or (cands if len(cands) == 1 else [])
        seg = path.rstrip(":").split("::")[-1]
        named = [c for c in cands if c.path.endswith(f"/{seg}.rs") or f"/{seg}/" in c.path]
        return named or cands

    def scan_fn(self, fn, table, depth, seen):
        """Effect classes found in fn's body, following calls (bare and module-path) to `depth`."""
        key = (fn.path, fn.bopen, depth)
        memo = self.__dict__.setdefault("_memo", {})
        if key in memo:
            return memo[key]
        memo[key] = {}
        found = {}
        body = fn.body
        for cls, rx in EFFECT_MARKERS:
            if rx.search(body):
                found.setdefault(cls, set()).add(f"{fn.path}:{fn.line}")
        if depth > 0:
            for m in re.finditer(r"(?<![\w.])((?:[A-Za-z_]\w*::)*)([a-z_]\w*)\s*\(", body):
                path, name = m.group(1), m.group(2)
                if name in KEYWORDS or (path + name) in seen:
                    continue
                for callee in self.resolve(fn, table, path, name):
                    for k, v in self.scan_fn(callee, table, depth - 1, seen | {path + name}).items():
                        found.setdefault(k, set()).update(v)
        memo[key] = found
        return found

    def effects_of_call(self, path, name, arch_names):
        """{isa or 'neutral': {class: {where}}} for a call. Unresolved -> {'?': {'unresolved'}}."""
        is_arch = "arch::imp" in path or (not path and name in arch_names)
        out = {}
        if is_arch:
            for isa, table in sorted(self.arch.items()):
                defs = table.get(name, [])
                acc = {}
                for d in defs:
                    for k, v in self.scan_fn(d, table, self.SCAN_DEPTH, {name}).items():
                        acc.setdefault(k, set()).update(v)
                if not defs:
                    acc = {"unresolved": {f"no fn {name} in arch/{isa}"}}
                out[isa] = acc
            return out
        defs = self.neutral.get(name, [])
        if not defs:
            return {"neutral": {"unresolved": {f"no fn {name} in neutral kernel"}}}
        acc = {}
        for d in defs:
            for k, v in self.scan_fn(d, self.neutral, self.SCAN_DEPTH, {name}).items():
                acc.setdefault(k, set()).update(v)
        out["neutral"] = acc
        return out


CALL_RE = re.compile(r"(?<![\w.])((?:[A-Za-z_]\w*::)*)([A-Za-z_]\w*)\s*(!?)\s*\(")
METHOD_RE = re.compile(r"\.([A-Za-z_]\w*)\s*\(")


def calls_in(text):
    """(path, name, kind) for each call in text. kind: fn | macro | method."""
    out = []
    for m in CALL_RE.finditer(text):
        path, name, bang = m.group(1), m.group(2), m.group(3)
        if name in KEYWORDS:
            continue
        out.append((path, name, "macro" if bang else "fn", m.start()))
    for m in METHOD_RE.finditer(text):
        out.append(("", m.group(1), "method", m.start()))
    return sorted(out, key=lambda c: c[3])


class Classifier:
    """Classify a call as pure / log / effect. Local helpers (same file) are classified by what they
    call, recursively and cycle-safe, so a pure helper like `cap_err_to_i64` is not an effect and a
    helper that enqueues is."""

    def __init__(self, u, fns, arch_names):
        self.u, self.fns, self.arch_names = u, fns, arch_names
        self.memo = {}

    def local(self, name, stack=()):
        if name in self.memo:
            return self.memo[name]
        if name in stack:
            return set()
        out = set()
        for f in self.fns.get(name, []):
            for p2, n2, k2, _ in calls_in(f.body):
                v, d, cl = self.call(p2, n2, k2, stack + (name,))
                if v == "effect":
                    out.add((d, frozenset(cl)))
        self.memo[name] = out
        return out

    def call(self, path, name, kind, stack=()):
        """Return (verdict, detail, classes). verdict in: pure, log, effect."""
        if kind == "macro":
            if name in LOG_MACROS:
                return "log", f"{name}!", {"log"}
            if name in PURE_MACROS:
                return "pure", f"{name}!", set()
            return "effect", f"macro {name}! (expansion not visible)", {"unresolved"}
        if kind == "method":
            if name in LOCAL_METHOD_EFFECT:
                return "effect", f".{name}() (atomic/lock method)", {"lock" if "lock" in name else "store"}
            return "pure", f".{name}() on a value", set()
        if name[:1].isupper():
            return "pure", f"{name}(..) constructor", set()
        if name in PURE:
            return "pure", f"{name} (PURE: {PURE[name]})", set()
        if name in ("current_task_lookup_cap", "current_task_holds_resource"):
            return "pure", f"{name} (gate)", set()
        if not path and name in self.fns:
            inner = self.local(name, stack)
            if not inner:
                return "pure", f"{name}() (local, no effect inside)", set()
            cls = set().union(*[set(c) for _, c in inner])
            shown = ", ".join(sorted({d.split(" [")[0] for d, _ in inner})[:4])
            return "effect", f"{name}() -> {shown}", cls
        eff = self.u.effects_of_call(path, name, self.arch_names)
        classes = set()
        for _, cl in eff.items():
            classes |= set(cl)
        bad = classes & (EFFECTFUL_CLASSES | {"unresolved"})
        if not bad:
            return ("log" if "log" in classes else "pure"), f"{path}{name} (scanned, no effect marker)", classes
        where = "; ".join(f"{isa}:{','.join(sorted(set(cl) & (EFFECTFUL_CLASSES | {'unresolved'})))}"
                          for isa, cl in sorted(eff.items()) if set(cl) & (EFFECTFUL_CLASSES | {"unresolved"}))
        return "effect", f"{path}{name} [{where}]", classes

# ------------------------------------------------------------------------------------------------
# Gates
# ------------------------------------------------------------------------------------------------

class Gate:
    def __init__(self, kind, sig, start, end, right_const, returns, line, note=""):
        self.kind, self.sig, self.start, self.end = kind, sig, start, end
        self.right_const, self.returns, self.line, self.note = right_const, returns, line, note


def short_res(expr):
    return expr.strip().split("::")[-1]


def right_of(expr):
    m = re.fullmatch(r"\s*Rights::([A-Z]+)(\s*\|\s*Rights::[A-Z]+)*\s*", expr)
    if m:
        return "|".join(re.findall(r"Rights::([A-Z]+)", expr)), True
    return "<runtime>", False


def block_returns(t, bopen):
    bclose = match_close(t, bopen)
    blk = t[bopen:bclose + 1]
    return bool(re.search(r"\breturn\b", blk)), bclose


def find_gates(t, a, b):
    """All recognised gates in t[a:b], in source order."""
    gates = []
    # SLOT
    for m in re.finditer(r"current_task_lookup_cap\s*\(", t[a:b]):
        cs = a + m.start()
        po = a + m.end() - 1
        pc = match_close(t, po)
        args = split_top(t[po + 1:pc])
        right, const = right_of(args[1] if len(args) > 1 else "")
        ss = stmt_start(t, cs, a)
        prefix = t[ss:cs]
        q = pc + 1
        while q < b and t[q].isspace():
            q += 1
        returns, end = False, pc
        if "match" in prefix and q < b and t[q] == "{":
            bc = match_close(t, q)
            arms = split_arms(t, q + 1, bc)
            returns = any(p.startswith("Err") and re.search(r"\breturn\b", e) for p, _, e, _, _ in arms) \
                or any(p.startswith("Err") and re.search(r"\breturn\b", t[es:ee]) for p, _, _, es, ee in arms)
            end = bc
            capvar = re.search(r"let\s+(?:mut\s+)?(\w+)\s*=", prefix)
        else:
            capvar = None
            ib = t.find("{", pc)
            if "if" in prefix and 0 <= ib < b:
                returns, end = block_returns(t, ib)
        scope = ""
        if capvar:
            # first scope check on the bound cap, within the next statements
            sm = re.compile(r"if\s+" + re.escape(capvar.group(1)) +
                            r"\.resource_id\s*!=\s*([\w:]+)\s*\{").search(t, end, min(b, end + 400))
            if sm:
                r2, e2 = block_returns(t, sm.end() - 1)
                if r2:
                    scope = "@" + short_res(sm.group(1))
                    end = e2
        gates.append(Gate("SLOT", f"SLOT({right}){scope}", ss, end, const, returns, line_of(t, cs)))
    # HOLD
    for m in re.finditer(r"current_task_holds_resource\s*\(", t[a:b]):
        cs = a + m.start()
        po = a + m.end() - 1
        pc = match_close(t, po)
        args = split_top(t[po + 1:pc])
        rid_expr = args[0] if args else ""
        right, const = right_of(args[1] if len(args) > 1 else "")
        ss = stmt_start(t, cs, a)
        prefix = t[ss:cs]
        rid = short_res(rid_expr)
        note = ""
        if re.fullmatch(r"[a-z_]\w*", rid_expr.strip()):
            # resolve `let (var, ..) = if C { (A, ..) } else { (B, ..) }`
            vm = re.search(r"let\s*\(\s*" + re.escape(rid_expr.strip()) +
                           r"\s*,[^=]*=\s*if\s+(\w+)\s*\{\s*\(\s*([\w:]+)\s*,.*?\}\s*else\s*\{\s*\(\s*([\w:]+)\s*,",
                           t[a:cs], re.S)
            if vm:
                rid = "{" + ",".join(sorted([short_res(vm.group(2)), short_res(vm.group(3))])) + "}"
                note = f"cond={vm.group(1)}:{short_res(vm.group(2))}/else:{short_res(vm.group(3))}"
            else:
                rid = f"<var {rid_expr.strip()}>"
        returns, end, peer = False, pc, ""
        lv = re.search(r"let\s+(\w+)\s*=\s*$", prefix.strip() + " ") or re.search(r"let\s+(\w+)\s*=", prefix)
        if lv and "if" not in prefix:
            var = lv.group(1)
            im = re.compile(r"\bif\s+([^{]*!\s*" + re.escape(var) + r"\b[^{]*)\{").search(t, pc, b)
            if im:
                returns, end = block_returns(t, im.end() - 1)
                if "declares_peer" in im.group(1):
                    peer = "|PEER"
        else:
            negated = t[max(a, cs - 14):cs].rstrip().endswith("scheduler::") and \
                t[max(a, cs - 15):cs - 11].strip().endswith("!") or \
                re.search(r"!\s*(?:scheduler::)?$", t[max(a, cs - 14):cs]) is not None
            ib = t.find("{", pc)
            if "if" in prefix and 0 <= ib < b and negated:
                returns, end = block_returns(t, ib)
        exempt = None
        em = re.search(r"!\s*matches!\s*\(\s*(\w+)\s*,\s*([\d\s|]+)\)", t[ss:pc])
        if em:
            exempt = (em.group(1), sorted(int(x) for x in re.findall(r"\d+", em.group(2))))
        g = Gate("HOLD", f"HOLD({rid},{right}){peer}", ss, end, const, returns, line_of(t, cs), note)
        g.exempt = exempt
        gates.append(g)
    # OWNER: irq route
    for m in re.finditer(r"registered_endpoint\s*\(", t[a:b]):
        cs = a + m.start()
        ss = stmt_start(t, cs, a)
        if "if" in t[ss:cs]:
            ib = t.find("{", cs)
            r, e = block_returns(t, ib)
            gates.append(Gate("OWNER", "OWNER(irq-route)", ss, e, True, r, line_of(t, cs)))
    # OWNER: delegated revoke (check-and-act in the callee) - only when it decides the result
    for m in re.finditer(r"revoke_owned\s*\(", t[a:b]):
        cs = a + m.start()
        ss = stmt_start(t, cs, a)
        if re.search(r"let\s+_\s*=", t[ss:cs]):
            continue
        pc = match_close(t, a + m.end() - 1)
        gates.append(Gate("OWNER", "OWNER(delegated)", ss, pc, True, True, line_of(t, cs),
                          "check and act are one callee; not verified here"))
    gates.sort(key=lambda g: g.start)
    return gates

# ------------------------------------------------------------------------------------------------
# The check
# ------------------------------------------------------------------------------------------------

class Result:
    def __init__(self):
        self.fail, self.warn, self.info, self.rows = [], [], [], []


def reachable(fns, start_names):
    seen, stack = set(), list(start_names)
    while stack:
        n = stack.pop()
        if n in seen or n not in fns:
            continue
        seen.add(n)
        for f in fns[n]:
            for path, name, kind, _ in calls_in(f.body):
                if kind == "fn" and not path and name in fns and name not in seen:
                    stack.append(name)
    return seen


def zero_sites(t, f):
    """Error-path-returns-0 and discarded-result sites in fn f."""
    sites = []
    a, b = f.bopen + 1, f.bclose

    def norm_line(off):
        ls = t.rfind("\n", 0, off) + 1
        le = t.find("\n", off)
        return re.sub(r"\s+", "", t[ls:le])

    for m in re.finditer(r"(?<![\w])(Err\s*\([^()]*\)|None)\s*=>", t[a:b]):
        p = a + m.end()
        es, ee, _ = parse_arm_expr(t, p, b)
        expr = t[es:ee].strip()
        val = expr
        if expr.startswith("{"):
            inner = expr[1:-1]
            depth, cut = 0, 0
            for k, ch in enumerate(inner):
                if ch in "([{":
                    depth += 1
                elif ch in ")]}":
                    depth -= 1
                    if depth == 0 and ch == "}":
                        cut = k + 1
                elif ch == ";" and depth == 0:
                    cut = k + 1
            val = inner[cut:].strip()
            if re.search(r"\breturn\s+0\s*[;}]", inner):
                val = "0"
        val = re.sub(r"^return\s+", "", val).strip().rstrip(";")
        if val == "0":
            sites.append(("err-arm-0", line_of(t, a + m.start()), norm_line(a + m.start())))
    for m in re.finditer(r"\belse\s*\{\s*return\s+0\s*;?\s*\}", t[a:b]):
        sites.append(("let-else-0", line_of(t, a + m.start()), norm_line(a + m.start())))
    for m in re.finditer(r"\.(?:is_err|is_none)\s*\(\s*\)\s*\{", t[a:b]):
        bo = a + m.end() - 1
        blk = t[bo:match_close(t, bo) + 1]
        if re.search(r"\breturn\s+0\s*;", blk):
            sites.append(("if-err-0", line_of(t, a + m.start()), norm_line(a + m.start())))
    for m in re.finditer(r"\blet\s+_\s*=", t[a:b]):
        sites.append(("discard", line_of(t, a + m.start()), norm_line(a + m.start())))
    return sites


_UNIVERSES = {}


def check(root, file_path, report=False, print_pin=False):
    r = Result()
    with open(file_path, encoding="utf-8", errors="replace") as f:
        raw = f.read()
    t = strip_rust(raw)
    disp_rel = os.path.relpath(file_path, root).replace("\\", "/") if file_path.startswith(root) \
        else file_path
    # The kernel's own files are the same for every mutant the self-test checks, so the index is built
    # once per root (it is read-only after construction); only the file under test is re-read.
    u = _UNIVERSES.get(root) or _UNIVERSES.setdefault(root, Universe(root))
    fns = index_functions(t, disp_rel)
    arch_names = set()
    for um in re.finditer(r"use\s+crate::arch::imp::(?:\w+::)*\{([^}]*)\}", t):
        arch_names |= {x.strip() for x in um.group(1).split(",") if x.strip()}
    for um in re.finditer(r"use\s+crate::arch::imp::(?:\w+::)*(\w+)\s*;", t):
        arch_names.add(um.group(1))
    C = Classifier(u, fns, arch_names)

    # --- S1: enum <-> dispatcher
    em = re.search(r"pub\s+enum\s+SyscallNumber\s*\{", t)
    if not em:
        r.fail.append("S1 cannot find `enum SyscallNumber`: unverifiable is a failure, never a pass")
        return r
    eb = match_close(t, em.end() - 1)
    enum = {m.group(1): int(m.group(2))
            for m in re.finditer(r"^\s*([A-Za-z]\w*)\s*=\s*(\d+)", t[em.end():eb], re.M)}
    if "syscall_handler" not in fns:
        r.fail.append("S1 cannot find `fn syscall_handler`")
        return r
    sh = fns["syscall_handler"][0]
    mm = re.search(r"\bmatch\s+number\s*\{", sh.body)
    if not mm:
        r.fail.append("S1 cannot find `match number` in syscall_handler")
        return r
    mo = sh.bopen + 1 + mm.end() - 1
    arms = split_arms(t, mo + 1, match_close(t, mo))
    seen_arms, default = {}, None
    for pat, poff, expr, eoff, eend in arms:
        km = re.fullmatch(r"n\s+if\s+n\s*==\s*SyscallNumber::(\w+)\s+as\s+u64", pat)
        if km:
            name = km.group(1)
            if name in seen_arms:
                r.fail.append(f"S1 {disp_rel}:{line_of(t, poff)} syscall {name} has two arms; the second is dead")
            seen_arms[name] = (expr, eoff, eend, line_of(t, poff))
        elif pat == "_":
            default = (expr, line_of(t, poff))
        else:
            r.fail.append(f"S1 {disp_rel}:{line_of(t, poff)} arm `{pat}` is not keyed by SyscallNumber: "
                          f"cannot be cross-checked against the enum or the pin")
    for name in sorted(set(enum) - set(seen_arms)):
        r.fail.append(f"S1 enum variant {name} = {enum[name]} has no dispatcher arm")
    for name in sorted(set(seen_arms) - set(enum)):
        r.fail.append(f"S1 arm names SyscallNumber::{name}, which is not in the enum")
    if default is None or not re.fullmatch(r"-\s*\d+", default[0]):
        r.fail.append(f"S1 the default arm must return a negative constant (unknown syscall); found "
                      f"{default[0] if default else 'none'}")

    for k, v in list(ALLOW_UNGATED.items()) + list(ALLOW_RUNTIME_RIGHT.items()):
        if not v or not v.strip():
            r.fail.append(f"allowlist entry {k} has no reason")

    handler_names = set()
    pin_now = {}
    for name in sorted(seen_arms, key=lambda n: enum.get(n, 999)):
        expr, eoff, eend, aline = seen_arms[name]
        num = enum.get(name, "?")
        cm = re.fullmatch(r"([a-z_]\w*)\s*\((.*)\)", expr, re.S)
        if cm and cm.group(1) in fns:
            f = fns[cm.group(1)][0]
            handler_names.add(f.name)
            a, b, where = f.bopen + 1, f.bclose, f"{f.name} ({disp_rel}:{f.line})"
        else:
            a, b, where = eoff, eend, f"inline arm ({disp_rel}:{aline})"
            for path, cname, kind, _ in calls_in(expr):
                if kind == "fn" and not path and cname in fns:
                    handler_names.add(cname)
        gates = find_gates(t, a, b)
        primary = next((g for g in gates if g.returns), None)
        # DELEGATION: a handler with no gate of its own whose first effectful act is a call to a local
        # helper that IS gated inherits that helper's gate (`Call` -> `do_call`). One level only, and
        # only when nothing effectful precedes the call in the handler: anything else stays UNGATED.
        via = ""
        ga, gb = a, b
        if primary is None:
            for path, cname, kind, off in calls_in(t[a:b]):
                if kind == "fn" and not path and cname in fns and cname != name:
                    hf = fns[cname][0]
                    hg = find_gates(t, hf.bopen + 1, hf.bclose)
                    hp = next((g for g in hg if g.returns), None)
                    if hp is not None:
                        prior = [c for c in calls_in(t[a:a + off]) if C.call(c[0], c[1], c[2])[0] == "effect"]
                        if not prior:
                            primary, gates, via = hp, hg, f" via {cname}()"
                            ga, gb = hf.bopen + 1, hf.bclose
                            handler_names.add(cname)
                        break
                if C.call(path, cname, kind)[0] == "effect":
                    break
        sig = primary.sig if primary else "UNGATED"
        # The PIN is every returning gate in order, not just the first: a handler's SECOND gate
        # (SpawnImage's IMAGE_SPAWN, the GRANT on a transferred cap) is often the one that matters.
        pin_now[name] = "+".join(g.sig for g in gates if g.returns) if primary else "UNGATED"
        row = [f"{num:>3} {name:<20} {sig:<46} {where}{via}"]

        # S2 + S4
        if primary is None:
            nonret = [g for g in gates if not g.returns]
            for g in nonret:
                r.fail.append(f"S4 {name} ({num}): {g.sig} at {disp_rel}:{g.line} - its failing branch "
                              f"does not RETURN, so the action runs anyway")
            if name not in ALLOW_UNGATED:
                r.fail.append(f"S2 {name} ({num}) is UNGATED and not in ALLOW_UNGATED - {where}. Gate it, "
                              f"or record why it needs no capability.")
        else:
            if name in ALLOW_UNGATED:
                r.fail.append(f"S2 {name} is in ALLOW_UNGATED but is now gated by {primary.sig}: remove "
                              f"the stale entry (the list may only shrink)")
            for g in gates:
                if not g.returns and g.start < primary.start:
                    r.fail.append(f"S4 {name} ({num}): {g.sig} at {disp_rel}:{g.line} precedes the "
                                  f"primary gate and does not return")
        # S5
        if primary is not None and not primary.right_const:
            if name not in ALLOW_RUNTIME_RIGHT:
                r.fail.append(f"S5 {name} ({num}): gate right is computed at runtime ({primary.sig}); "
                              f"Rights(0) passes Rights::contains vacuously")
            else:
                r.warn.append(f"S5 {name}: runtime right allowed - {ALLOW_RUNTIME_RIGHT[name]}")
        # S3: effects before the gate (or anywhere, if ungated)
        region_end = primary.start if primary else gb
        pre = t[ga:region_end]
        effects = []
        for path, cname, kind, off in calls_in(pre):
            verdict, detail, _cls = C.call(path, cname, kind)
            if verdict == "effect":
                effects.append((line_of(t, ga + off), detail))
            elif verdict == "log" and primary is not None:
                r.warn.append(f"S3 {name}: logs before its gate ({detail}, line {line_of(t, ga + off)})")
        # ...and effects written INLINE before the gate, not through a call: a static assignment, an
        # atomic store, a volatile write, inline asm.
        for cls, rx in EFFECT_MARKERS:
            if cls in ("log", "wake"):
                continue
            for m in rx.finditer(pre):
                effects.append((line_of(t, ga + m.start()), f"inline {cls}: `{pre[m.start():m.end()].strip()}`"))
        if primary is not None:
            for ln, d in effects:
                r.fail.append(f"S3 {name} ({num}): state-changing call BEFORE the gate at line {ln}: {d}")
        elif effects:
            row.append("      ungated effects: " + "; ".join(f"L{ln} {d}" for ln, d in effects[:6]))
        a, b = ga, gb
        # S6: InspectKernel queries
        if primary is not None and getattr(primary, "exempt", None):
            var, ungated = primary.exempt
            qm = re.compile(r"\bmatch\s+" + re.escape(var) + r"\s*\{").search(t, primary.end, b)
            qarms = {}
            if qm:
                for pat, poff, qexpr, qs, qe in split_arms(t, qm.end(), match_close(t, qm.end() - 1)):
                    for qid in re.findall(r"\d+", pat) if re.fullmatch(r"[\d\s|]+", pat) else []:
                        qarms[int(qid)] = (qs, qe, line_of(t, poff))
            for q in ungated:
                if q not in qarms:
                    r.fail.append(f"S6 InspectKernel query {q} is in the ungated list but has no arm: an "
                                  f"authority list naming nothing (cf. backlog/fixed/80 K18)")
                    continue
                if q not in ALLOW_UNGATED_QUERY:
                    r.fail.append(f"S6 InspectKernel query {q} is UNGATED and not in ALLOW_UNGATED_QUERY "
                                  f"({disp_rel}:{qarms[q][2]}) - it discloses or changes state with no "
                                  f"capability. Gate it, or record why and what it touches.")
                    continue
                qs, qe, ql = qarms[q]
                found = {}
                for path, cname, kind, _ in calls_in(t[qs:qe]):
                    if kind == "method" and cname not in LOCAL_METHOD_EFFECT:
                        continue
                    if kind == "macro" or cname[:1].isupper():
                        continue
                    if not path and cname in fns:
                        continue
                    for isa, cl in u.effects_of_call(path, cname, arch_names).items():
                        for c in cl:
                            if c in EFFECTFUL_CLASSES | {"log"}:
                                found.setdefault(c, set()).add(f"{isa}:{cname}")
                undeclared = set(found) - ALLOW_UNGATED_QUERY[q]["effects"]
                if undeclared:
                    r.fail.append(f"S6 ungated InspectKernel query {q} ({disp_rel}:{ql}) shows effect "
                                  f"class(es) {sorted(undeclared)} not declared in its allowlist entry: "
                                  + "; ".join(f"{c}<-{','.join(sorted(found[c]))}" for c in sorted(undeclared)))
                if found:
                    row.append(f"      q{q:<2} ungated, effects: " + ", ".join(
                        f"{c}({','.join(sorted(found[c]))})" for c in sorted(found)))
            for q in sorted(set(ALLOW_UNGATED_QUERY) - set(ungated)):
                r.fail.append(f"S6 ALLOW_UNGATED_QUERY lists query {q}, which the kernel now GATES: remove "
                              f"the stale entry")
            gated = sorted(set(qarms) - set(ungated))
            row.append(f"      queries gated by {primary.sig}: {gated}")
            row.append(f"      queries ungated by the kernel's own list: {ungated}")

        # S7: op dispatch after the gate
        if primary is not None and primary.kind == "HOLD" and primary.note.startswith("cond="):
            cond = re.match(r"cond=(\w+):(\w+)/else:(\w+)", primary.note)
            cv, tres, fres = cond.group(1), cond.group(2), cond.group(3)
            bm = re.search(r"let\s+" + re.escape(cv) + r"\s*=\s*([^;]+);", t[a:b])
            cset = set()
            if bm:
                cset = {int(x) for x in re.findall(r"==\s*(\d+)", bm.group(1))}
            om = re.compile(r"\bmatch\s+(\w+)\s*\{").search(t, primary.end, b)
            if om:
                ops = []
                for pat, poff, _, _, _ in split_arms(t, om.end(), match_close(t, om.end() - 1)):
                    for o in re.findall(r"\d+", pat):
                        ops.append(f"op{o}->{tres if int(o) in cset else fres}")
                    if pat == "_":
                        ops.append("op_->(gated, returns -1)")
                row.append("      ops: " + ", ".join(ops))
        elif primary is not None:
            om = re.compile(r"\b(?:match\s+(\w+)\s*\{|if\s+(\w+)\s*==\s*0\s*\{)").search(t, primary.end, b)
            if om and (om.group(1) or om.group(2)) == "op":
                row.append(f"      ops on `{om.group(1) or om.group(2)}`: all after the gate ({primary.sig})")

        # INFO: error returns after the first state change (non-atomic failure candidates).
        if primary is not None:
            post = t[primary.end:b]
            first_eff = None
            for path, cname, kind, off in calls_in(post):
                if kind != "fn":
                    continue
                v, d, cls = C.call(path, cname, kind)
                if v == "effect" and (cls - {"lock", "log", "asm"}):
                    first_eff = (off, cname)
                    break
            if first_eff:
                tail = post[first_eff[0]:]
                errs = re.findall(r"cap_err_to_i64\s*\(|ipc_err_to_i64\s*\(|=>\s*-1\b|return\s+-1\b|return\s+-11\b|=>\s*-11\b", tail)
                if errs:
                    r.info.append(f"{name}: {len(errs)} error return(s) after the first state change "
                                  f"`{first_eff[1]}` (line {line_of(t, primary.end + first_eff[0])})")
        r.rows.append("\n".join(row))

    # S8
    for fname in sorted(reachable(fns, handler_names)):
        for f in fns[fname]:
            for kind, ln, key in zero_sites(t, f):
                k = (fname, key)
                if k not in ALLOW_ZERO:
                    r.fail.append(f"S8 {fname} {disp_rel}:{ln} [{kind}] an error/None path yields 0 (success) "
                                  f"or a result is discarded: `{key}` - not in ALLOW_ZERO")
    live_keys = {(fname, key) for fname in reachable(fns, handler_names) for f in fns[fname]
                 for _, _, key in zero_sites(t, f)}
    for k in sorted(set(ALLOW_ZERO) - live_keys):
        r.fail.append(f"S8 ALLOW_ZERO entry {k} matches nothing: remove it (the list may only shrink)")

    # S9
    for name in sorted(set(pin_now) | set(GATE_PIN)):
        a_, b_ = GATE_PIN.get(name), pin_now.get(name)
        if a_ != b_:
            r.fail.append(f"S9 gate of {name} changed: pinned `{a_}` -> now `{b_}`. Deliberate? Update "
                          f"GATE_PIN with the reason in the commit.")
    if print_pin:
        for name in sorted(pin_now, key=lambda n: enum.get(n, 999)):
            print(f'    "{name}": "{pin_now[name]}",')
    r.enum = enum
    return r


# ------------------------------------------------------------------------------------------------
# --selftest: the negative tests. Each mutant is an exact, single-occurrence text replacement on a COPY
# of the real dispatch.rs; one that does not apply exactly once FAILS the self-test (a probe that silently
# does nothing would report "missed" for the wrong reason). The repository is never written.
#   catch  a real violation: the check must fail, with the named rule id(s)
#   miss   a real violation the check is NOT expected to see (a stated limit): it must pass, so a limit
#          that silently changes is noticed
#   fp     not a violation: the check fails closed (a known false positive)
# ------------------------------------------------------------------------------------------------

REBOOT_GATE = """    if !scheduler::current_task_holds_resource(crate::capability::REBOOT_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    crate::kprintln!("reboot: hardware reset");"""

MUTANTS = [
    dict(id="M1", what="remove the REBOOT gate from handle_reboot", expect="catch", rules=["S2", "S9"],
         edits=[(REBOOT_GATE, """    crate::kprintln!("reboot: hardware reset");""")]),
    dict(id="M2", what="move ConsolePush's side effect above its gate", expect="catch", rules=["S3"],
         edits=[("""fn handle_console_push(cap_slot: u64, byte: u64) -> i64 {
    use crate::capability::CONSOLE_PUSH_RESOURCE;
""", """fn handle_console_push(cap_slot: u64, byte: u64) -> i64 {
    use crate::capability::CONSOLE_PUSH_RESOURCE;
    crate::arch::imp::console_push_byte(byte as u8);
"""), ("""        return cap_err_to_i64(CapError::CapWrongScope);
    }
    crate::arch::imp::console_push_byte(byte as u8);
    0
}""", """        return cap_err_to_i64(CapError::CapWrongScope);
    }
    0
}""")]),
    dict(id="M3", what="add a new ungated syscall arm (DebugPoke = 58) that pushes a console byte",
         expect="catch", rules=["S2", "S9"],
         edits=[("""    Exit                   = 57,
}""", """    Exit                   = 57,
    DebugPoke              = 58,
}"""), ("""        n if n == SyscallNumber::Exit as u64 => handle_exit(arg0),
""", """        n if n == SyscallNumber::Exit as u64 => handle_exit(arg0),
        n if n == SyscallNumber::DebugPoke as u64 => handle_debug_poke(arg0),
"""), ("""fn ipc_err_to_i64(e: IpcError) -> i64 {""", """fn handle_debug_poke(b: u64) -> i64 {
    crate::arch::imp::console_push_byte(b as u8);
    0
}

fn ipc_err_to_i64(e: IpcError) -> i64 {""")]),
    dict(id="M4", what="DeriveCap's cap-table-full error path returns 0 (success)", expect="catch",
         rules=["S8"],
         edits=[("""        Err(_)   => -1, // cap table full
    }
}""", """        Err(_)   => 0, // cap table full
    }
}""")]),
    dict(id="M5", what="make InspectKernel query 4 (system-wide free frames) ungated", expect="catch",
         rules=["S6"],
         edits=[("if !matches!(query_id, 0 | 3 | 10 |", "if !matches!(query_id, 0 | 3 | 4 | 10 |")]),
    dict(id="M6", what="FireIrq gate logs the refusal and FALLS THROUGH (no return)", expect="catch",
         rules=["S4"],
         edits=[("""    if !scheduler::current_task_holds_resource(crate::capability::FIRE_IRQ_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }""", """    if !scheduler::current_task_holds_resource(crate::capability::FIRE_IRQ_RESOURCE, Rights::WRITE) {
        crate::kprintln!("fire-irq: refused");
    }""")]),
    dict(id="M7", what="Send checks a caller-chosen right instead of Rights::SEND", expect="catch",
         rules=["S5", "S9"],
         edits=[("""fn handle_send(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::SEND) {""",
                 """fn handle_send(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights((msg_len >> 32) as u8)) {""")]),
    dict(id="M8", what="Reboot gated on LOG_WRITE (any logging service may reset the machine)",
         expect="catch", rules=["S9"],
         edits=[("current_task_holds_resource(crate::capability::REBOOT_RESOURCE, Rights::WRITE) {\n"
                 "        return cap_err_to_i64(CapError::CapNotHeld);\n    }\n    crate::kprintln!(\"reboot:",
                 "current_task_holds_resource(crate::capability::LOG_WRITE_RESOURCE, Rights::WRITE) {\n"
                 "        return cap_err_to_i64(CapError::CapNotHeld);\n    }\n    crate::kprintln!(\"reboot:")]),
    dict(id="M9", what="state change hidden in a LOCAL helper called before Reboot's gate", expect="catch",
         rules=["S3"],
         edits=[("""fn handle_reboot() -> i64 {
""", """fn note_attempt() {
    CONSOLE_LOST.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

fn handle_reboot() -> i64 {
    note_attempt();
""")]),
    dict(id="M10", what="Send gate validates slot A but enqueues to an endpoint taken from an ARGUMENT",
         expect="miss", rules=[],
         edits=[("""fn handle_send(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::SEND) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    let endpoint_id = EndpointId(cap.resource_id.0);""", """fn handle_send(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::SEND) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    let endpoint_id = EndpointId(msg_len >> 32);""")]),
    dict(id="M11", what="Reboot's (correct) gate moved into a macro_rules! macro", expect="fp",
         rules=["S2"],
         edits=[(REBOOT_GATE, """    require_hold!(REBOOT_RESOURCE);
    crate::kprintln!("reboot: hardware reset");"""),
                ("""fn handle_reboot() -> i64 {""", """macro_rules! require_hold {
    ($r:ident) => {
        if !scheduler::current_task_holds_resource(crate::capability::$r, Rights::WRITE) {
            return cap_err_to_i64(CapError::CapNotHeld);
        }
    };
}

fn handle_reboot() -> i64 {""")]),
    dict(id="M12", what="ConsoleRead publishes itself as the console waiter BEFORE its gate (inline .store)",
         expect="catch", rules=["S3"],
         edits=[("""    // Validate cap: must hold CONSOLE_READ_RESOURCE with READ right.
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::READ) {""",
                 """    crate::arch::imp::CONSOLE_READ_WAITER.store(scheduler::current_task_slot() as u32, Ordering::Release);
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::READ) {""")]),
    dict(id="M13", what="Kill: kill_by_name runs before the SERVICE_CONTROL check (gate reordered)",
         expect="catch", rules=["S3"],
         edits=[("""fn handle_kill(name_ptr: u64, name_len: u64) -> i64 {
""", """fn handle_kill(name_ptr: u64, name_len: u64) -> i64 {
    let _early = crate::task::kill_by_name("ping", crate::task::Cause::Killed { by: scheduler::current_task_slot() });
""")]),
    dict(id="M14", what="delete the Gpio dispatcher arm (enum variant left dangling)",
         expect="catch", rules=["S1"],
         edits=[("""        n if n == SyscallNumber::Gpio           as u64 => handle_gpio(arg0, arg1),
""", "")]),
    dict(id="M15", what="AcquireSendCap: drop the declared-peer half, keep ACQUIRE_ANY (narrower - safe)",
         expect="fp", rules=["S9"],
         edits=[("    if !broad && !crate::task::current_task_declares_peer(name) {",
                 "    if !broad {")]),
    dict(id="M17", what="Kill's gate WEAKENED by an extra conjunct: refuses only names longer than 32 bytes",
         expect="miss", rules=[],
         edits=[("""    if !scheduler::current_task_holds_resource(
        crate::capability::SERVICE_CONTROL_RESOURCE, Rights::WRITE)
    {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    let len = name_len as usize;""", """    if !scheduler::current_task_holds_resource(
        crate::capability::SERVICE_CONTROL_RESOURCE, Rights::WRITE) && name_len > 32
    {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    let len = name_len as usize;""")]),
    dict(id="M18", what="SpawnImage loses its SECOND gate (IMAGE_SPAWN): any SPAWN holder may start arbitrary bytes",
         expect="catch", rules=["S9"],
         edits=[("""    if !scheduler::current_task_holds_resource(
        crate::capability::IMAGE_SPAWN_RESOURCE, Rights::WRITE)
    {
        crate::kprintln!("task: SpawnImage refused - caller holds SPAWN but not IMAGE_SPAWN (starting arbitrary bytes is the supervisor's authority alone; ask it via supcmd::SPAWN)");
        return cap_err_to_i64(CapError::CapNotHeld);
    }
""", "")]),
    dict(id="M16", what="TakePendingCap (allowlisted ungated) gains a gate: the allowlist entry is now stale",
         expect="fp", rules=["S2"],
         edits=[("""fn handle_take_pending_cap() -> i64 {
""", """fn handle_take_pending_cap() -> i64 {
    if !scheduler::current_task_holds_resource(crate::capability::INTROSPECT_RESOURCE, Rights::READ) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
""")]),
]


def selftest(root, quiet=False):
    import tempfile
    src_path = os.path.join(root, "kernel", "src", "syscall", "dispatch.rs")
    with open(src_path, encoding="utf-8") as f:
        pristine = f.read()
    bad = 0
    with tempfile.TemporaryDirectory() as tmp:
        def run(text, name):
            path = os.path.join(tmp, name).replace("\\", "/")
            with open(path, "w", encoding="utf-8", newline="") as f:
                f.write(text)
            return check(root, path, False, False).fail
        fails = run(pristine, "dispatch.orig.rs")
        if fails:
            print("  selftest: the unmutated dispatch.rs must pass before a mutant means anything:")
            for x in fails:
                print("    " + x)
            return 1
        for m in MUTANTS:
            text = pristine
            for old, new in m["edits"]:
                n = text.count(old)
                if n != 1:
                    print(f"  {m['id']:<4} PROBE BROKEN: its mutation text occurs {n} times in dispatch.rs "
                          f"(must be 1) - update the probe to the code as it is now")
                    bad += 1
                    break
                text = text.replace(old, new)
            else:
                fails = run(text, m["id"] + ".rs")
                hit = sorted({x.split()[0] for x in fails})
                if m["expect"] in ("catch", "fp"):
                    ok = bool(fails) and all(r in hit for r in m["rules"])
                    verdict = ("CAUGHT" if m["expect"] == "catch" else "FLAGGED (known false positive)")                         if ok else "MISSED"
                else:
                    ok = not fails
                    verdict = "MISSED (as expected: a stated limit)" if ok else "unexpectedly CAUGHT"
                bad += 0 if ok else 1
                if not ok or not quiet:
                    print(f"  {m['id']:<4} {verdict:<38} rules={hit}  - {m['what']}")
    print(f"  selftest: {len(MUTANTS) - bad}/{len(MUTANTS)} mutants of dispatch.rs behaved as expected")
    return 1 if bad else 0


def main(argv):
    root = DEFAULT_ROOT
    file_path = None
    report = "--report" in argv
    print_pin = "--print-pin" in argv
    if "--root" in argv:
        root = argv[argv.index("--root") + 1]
    if "--file" in argv:
        file_path = argv[argv.index("--file") + 1]
    root = os.path.abspath(root).replace("\\", "/")
    if "--selftest" in argv:
        return selftest(root)
    if file_path is None:
        file_path = os.path.join(root, "kernel", "src", "syscall", "dispatch.rs")
    file_path = os.path.abspath(file_path).replace("\\", "/")
    r = check(root, file_path, report, print_pin)
    print(f"syscall_gate_check (ARCHITECTURAL_CHECK) on {file_path}")
    print(f"  scope: syscall_handler arms in that file; callee effect scan over {root}/kernel/src (all arches)")
    if report or r.fail:
        print("  per-arm gate table:")
        for row in r.rows:
            print("   " + row)
    for w in r.warn:
        print("  WARN " + w)
    if report:
        for i in r.info:
            print("  INFO " + i)
    for f in r.fail:
        print("  FAIL " + f)
    print(f"  {len(r.rows)} arms, {len(r.fail)} failure(s), {len(r.warn)} warning(s), {len(r.info)} info")
    print("  does NOT establish: that a gate checks the right object or resource for the action, "
          "anything inside helpers/macros after the gate, or arch semantics beyond a marker grep")
    if r.fail:
        return 1
    # The negative tests run with every plain check (about 3 s more), so no build path can run the gate
    # without proving it still fires: a checker on one build path is a checker on none, and a gate whose
    # mutants are run by hand is one nobody runs. `--file` checks a copy, and is what the mutants use.
    if "--file" not in argv:
        return selftest(root, quiet=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
