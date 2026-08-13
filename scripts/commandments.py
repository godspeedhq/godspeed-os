#!/usr/bin/env python3
"""Enforce the Ten Commandments of Godspeed mechanically, and say plainly what it cannot enforce.

`COMMANDMENTS.md` is the law. This is the part of the law a machine can check, and it fails the build
when that part is broken.

    python scripts/commandments.py              enforce (exit 1 on any violation)
    python scripts/commandments.py --report     the scoreboard for all ten, including what is NOT checked
    python scripts/commandments.py --selftest   prove every check still fires

THE HONESTY RULE
----------------
A check that can be satisfied without the property being true is WORSE than no check, because it turns
a live obligation into a green tick. Banning `unwrap` does not stop a service hanging. So:

  1. Every check declares what it PROVES and what it explicitly DOES NOT. Both are printed.
  2. Commandments with no mechanical check are listed as UNMECHANISED on every report. Ten green ticks
     covering three commandments is a lie; three green and seven "human review" is the truth.
  3. Every check states the SCOPE it scanned and everything it EXCLUDED, next to its result. A pass
     that does not say what it looked at is the most convincing kind of lie.

CONSTITUTIONAL INTEGRITY - why this file is built the way it is
---------------------------------------------------------------
The dangerous failure is not "someone violated a Commandment". It is "someone violated a Commandment
and edited the interpretation layer until the violation became acceptable". That happened during this
file's own construction: the first version flagged `build.rs`, and the author (me) responded by editing
the scanner to skip those files - a red check made green by changing what red means. The exclusion was
correct on its merits. The method was exactly the drift this tool exists to catch.

No checker can prevent that, because anything writable is rewritable. So the goal is not prevention, it
is UNMISSABILITY. Three structural defences:

  * SCOPE IS DATA, NOT CODE. What each check scans, and every exclusion with its reason, lives in the
    check definition and is PRINTED with the result. Narrowing a scan is a visible diff and a visible
    line of output, never a quiet `if name != ...` buried in a helper.

  * A SELF-TEST CORPUS WITH TEETH. Each check carries known-bad snippets it MUST flag, and known-good
    ones it must NOT. Weaken a pattern, narrow a scan, or delete a check, and `--selftest` goes red.
    This is the only defence that fights back from inside the system, so it runs in CI beside the
    checks themselves. The corpus is INLINE rather than a directory of .rs files, deliberately: real
    files full of deliberate violations would be scanned by the real checks, and the only ways out of
    that are excluding them (shrinking scope, the very thing being guarded) or living with false
    failures.

  * THE BASELINE ONLY SHRINKS, and an exemption must cite a CLAUDE.md amendment that already accepts
    the violation. If nothing in the constitution accepts it, there is no exemption to write: fix it,
    or amend the constitution deliberately. `COMMANDMENTS.baseline.toml` has the full rules.
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "COMMANDMENTS.baseline.toml")

RED, GREEN, YELLOW, BOLD, DIM, OFF = (
    "\033[31m", "\033[32m", "\033[33m", "\033[1m", "\033[2m", "\033[0m")


class Violation:
    def __init__(self, path, line, detail):
        self.path = path.replace("\\", "/")
        self.line = line
        self.detail = detail


def read(p):
    with open(p, encoding="utf-8", errors="replace") as f:
        return f.read()


def files_in_scope(check):
    """Every file a source check covers, plus the ones its declared exclusions removed.

    Returns (included, excluded_count). Both are reported, because a check that quietly stops looking
    at things is indistinguishable from a check that passes.
    """
    inc, exc = [], 0
    excludes = [e["glob"] for e in check.get("exclude", [])]
    for d in check["dirs"]:
        for base, _, names in os.walk(os.path.join(ROOT, d)):
            for n in sorted(names):
                if not n.endswith(".rs"):
                    continue
                if n in excludes:
                    exc += 1
                    continue
                inc.append(os.path.join(base, n))
    return sorted(inc), exc


def scan_source(check, _pins):
    """Report each line matching the check's pattern, ignoring comments.

    Comment-skipping is deliberate: these Commandments govern what the code DOES. A doc comment
    explaining why `unwrap` is forbidden must not itself be a violation, or the checker punishes the
    documentation that makes it understandable.
    """
    rx = re.compile(check["pattern"])
    sk = re.compile(check["skip"]) if check.get("skip") else None
    out = []
    inc, _ = files_in_scope(check)
    for f in inc:
        for i, ln in enumerate(read(f).split("\n"), 1):
            if matches_line(ln, rx, sk):
                out.append(Violation(os.path.relpath(f, ROOT), i, check["fix"]))
    return out


def matches_line(ln, rx, sk):
    s = ln.strip()
    if s.startswith("//") or s.startswith("*") or s.startswith("#["):
        return False
    if sk and sk.search(ln):
        return False
    return bool(rx.search(ln))


# --------------------------------------------------------------------------------------------------
# Commandment I - thou shalt not expand the responsibilities of the kernel
# --------------------------------------------------------------------------------------------------

def check_syscall_surface(check, pins):
    """The syscall table is the kernel's authority surface, so pin it by name AND number.

    Every expansion of what the kernel DOES for userspace arrives as a syscall. An unlisted entry is
    the earliest mechanical signal of scope creep - sharper than any line count, because it NAMES the
    new responsibility. Adding one becomes deliberate: change the code and the pin, and answer "why
    isn't this a service?" in between.
    """
    src = read(os.path.join(ROOT, "kernel/src/syscall/dispatch.rs"))
    body = re.search(r"pub enum SyscallNumber\s*\{(.*?)\n\}", src, re.S)
    if not body:
        return [Violation("kernel/src/syscall/dispatch.rs", 0,
                          "cannot find `enum SyscallNumber`: the pin cannot be verified, and an "
                          "unverifiable pin is a failure, never a pass")]
    found = dict(re.findall(r"^\s*([A-Za-z][A-Za-z0-9]*)\s*=\s*(\d+)", body.group(1), re.M))
    pinned = {k: str(v) for k, v in pins.get("syscalls", {}).items()}
    out = []
    for name, num in sorted(found.items(), key=lambda kv: int(kv[1])):
        if name not in pinned:
            out.append(Violation("kernel/src/syscall/dispatch.rs", 0,
                                 f"syscall {name} = {num} is NOT pinned. A new syscall is a new kernel "
                                 f"responsibility: ask 'why isn't this a service?'. If it must exist, "
                                 f"add it to [kernel.syscalls] deliberately."))
        elif pinned[name] != num:
            out.append(Violation("kernel/src/syscall/dispatch.rs", 0,
                                 f"syscall {name} moved {pinned[name]} -> {num}. These numbers are the "
                                 f"ABI every built service binary already depends on."))
    for name in sorted(set(pinned) - set(found)):
        out.append(Violation("kernel/src/syscall/dispatch.rs", 0,
                             f"pinned syscall {name} is gone. Removing a responsibility is welcome - "
                             f"update the pin so the recorded surface stays true."))
    return out


def check_kernel_modules(check, pins):
    """One name per kernel responsibility. A new directory under `kernel/src` is a new thing the
    kernel claims to be, and that claim should be impossible to make by accident."""
    pinned = set(pins.get("modules", []))
    found = {n for n in os.listdir(os.path.join(ROOT, "kernel/src")) if not n.startswith(".")}
    out = [Violation("kernel/src/" + n, 0,
                     "new top-level kernel module, which is a new kernel responsibility. If it is "
                     "genuinely mechanism and not policy, add it to [kernel.modules].")
           for n in sorted(found - pinned)]
    out += [Violation("kernel/src/" + n, 0, "pinned kernel module is gone - update [kernel.modules].")
            for n in sorted(pinned - found)]
    return out


# --------------------------------------------------------------------------------------------------
# The checks. `scope`, `exclude`, `proves` and `does_not_prove` are all printed with the result.
# `probes` is the self-test corpus: what this check MUST catch, and what it must NOT.
# --------------------------------------------------------------------------------------------------

def check_arch_device_drivers(check, pins):
    """Commandment I, where it actually bites: a DEVICE driver in the kernel.

    "New hardware support, new CPU architectures, and bug fixes are welcome. New RESPONSIBILITIES are
    not." So `arch/` is legitimate - a kernel must bring up its own CPU, MMU, timer, interrupt
    controller and console. What is not legitimate is a driver for a PERIPHERAL: a USB host stack, a
    NIC, a disk. Those are services, and the Pi 4 port proved it by deleting 2742 lines of ring-0 xhci.

    Pinning module NAMES cannot see this, because a USB stack added under `arch/` hides behind a
    legitimate module. So every file under `arch/` declares its ROLE, and roles come from a fixed
    vocabulary of things a kernel is allowed to be. A new file must be classified before it builds, and
    classifying it honestly as a device driver fails - which is the point. The lie required to sneak one
    past is now explicit and in the diff, rather than a filename nobody re-reads.
    """
    roles = pins.get("arch_roles", {})
    permitted = set(pins.get("arch_permitted_roles", []))
    out = []
    for base, _, names in os.walk(os.path.join(ROOT, "kernel/src/arch")):
        for n in sorted(names):
            if not n.endswith(".rs"):
                continue
            f = os.path.relpath(os.path.join(base, n), os.path.join(ROOT, "kernel/src/arch"))
            f = f.replace("\\", "/")
            path = "kernel/src/arch/" + f
            role = roles.get(f)
            if role is None:
                out.append(Violation(path, 0,
                                     "this arch file has no declared role. Every file under arch/ must "
                                     "say what it IS, from the permitted vocabulary: "
                                     + ", ".join(sorted(permitted))))
            elif role not in permitted:
                out.append(Violation(path, 0,
                                     f"role '{role}' is a PERIPHERAL DEVICE DRIVER, which is a service, "
                                     f"not a kernel responsibility (Commandment I). Ask why it is not a "
                                     f"service; if the hardware genuinely cannot support one yet, that "
                                     f"needs a CLAUDE.md amendment and an exemption citing it."))
    # A role for a file that no longer exists is rot, and rot in the pin is the same failure as a
    # stale exemption: the record stops describing the system while still looking authoritative.
    # (Found by writing this check's own probe badly - the wrong test asserted nothing, and the
    # nothing it asserted turned out to be a real hole.)
    on_disk = {os.path.relpath(os.path.join(b, n), os.path.join(ROOT, "kernel/src/arch")).replace("\\", "/")
               for b, _, ns in os.walk(os.path.join(ROOT, "kernel/src/arch")) for n in ns
               if n.endswith(".rs")}
    for f in sorted(set(roles) - on_disk):
        out.append(Violation("kernel/src/arch/" + f, 0,
                             "a role is pinned for a file that no longer exists. Removing kernel code "
                             "is welcome - delete the entry so the record stays true."))
    return out



def check_kernel_authorities(check, pins):
    """Commandment I: a new KERNEL AUTHORITY is a new kernel responsibility.

    The syscall pin catches new kernel VERBS. This catches new kernel NOUNS: the well-known resources
    the kernel itself mints authority over (LOG_WRITE, SPAWN, REBOOT, NET_DEVICE ...). Adding one means
    the kernel has taken responsibility for something new, whether or not a syscall was added with it -
    and several of these exist precisely because a syscall alone could not express the authority.

    Same instrument as the syscall pin, and pinned by VALUE for the same reason: the numbers are
    baked into every service's capability table.
    """
    src = read(os.path.join(ROOT, "kernel/src/capability/mod.rs"))
    found = dict((m[0], m[1]) for m in re.findall(
        r"pub const ([A-Z_]+)_RESOURCE: ResourceId = ResourceId\((\d+)\)", src))
    if not found:
        return [Violation("kernel/src/capability/mod.rs", 0,
                          "cannot find any well-known ResourceId: the pin cannot be verified, and an "
                          "unverifiable pin is a failure, never a pass")]
    pinned = {k: str(v) for k, v in pins.get("authorities", {}).items()}
    out = []
    for name, num in sorted(found.items(), key=lambda kv: int(kv[1])):
        if name not in pinned:
            out.append(Violation("kernel/src/capability/mod.rs", 0,
                                 f"kernel authority {name} = {num} is NOT pinned. The kernel has taken "
                                 f"responsibility for something new: ask 'why isn't this a service?'. "
                                 f"If it must exist, add it to [kernel.authorities] deliberately."))
        elif pinned[name] != num:
            out.append(Violation("kernel/src/capability/mod.rs", 0,
                                 f"authority {name} moved {pinned[name]} -> {num}. These ids are baked "
                                 f"into every service's capability table."))
    for name in sorted(set(pinned) - set(found)):
        out.append(Violation("kernel/src/capability/mod.rs", 0,
                             f"pinned authority {name} is gone. Removing one is welcome - update the "
                             f"pin so the recorded surface stays true."))
    return out


def check_kernel_dependencies(check, pins):
    """Commandment I: a crate the kernel links is a responsibility it did not write and must trust.

    Every dependency is code running in ring 0 with the kernel's full authority, and it arrives without
    passing any of these checks. "Why isn't this a service?" applies to a crate exactly as it applies to
    a subsystem - more sharply, because a service could link it under isolation instead.
    """
    src = read(os.path.join(ROOT, "kernel/Cargo.toml"))
    found = set()
    for block in re.findall(r"^\[(?:target\.[^\]]*\.)?dependencies\](.*?)(?=^\[|\Z)", src, re.S | re.M):
        for ln in block.split(chr(10)):
            ln = ln.strip()
            if not ln or ln.startswith("#"):
                continue
            m = re.match(r"([A-Za-z0-9_-]+)\s*=", ln)
            if m:
                found.add(m.group(1))
    pinned = set(pins.get("dependencies", []))
    out = [Violation("kernel/Cargo.toml", 0,
                     f"new kernel dependency '{d}': code that will run in ring 0 with the kernel's full "
                     f"authority, having passed none of these checks. Could a SERVICE link it instead? "
                     f"If the kernel genuinely needs it, add it to [kernel] dependencies deliberately.")
           for d in sorted(found - pinned)]
    out += [Violation("kernel/Cargo.toml", 0,
                      f"pinned dependency '{d}' is gone - update the pin so the record stays true.")
            for d in sorted(pinned - found)]
    return out



def check_kernel_spawns(check, pins):
    """Commandment I's ONE sanctioned exception, pinned so it stays one.

    The kernel restarts exactly one service - the supervisor - and it must, because of Commandment V:
    no service is special, so the supervisor has to be restartable too, and only the kernel is beneath
    it to do the restarting. CLAUDE.md 11.1 calls it "the kernel's ONE direct spawn", 6.2 makes the
    respawn unconditional and unbounded, and naming-design.md 3.7 records the trade openly: a sliver of
    26.10 (mechanism, not policy) exchanged for maximum fault tolerance.

    An exception that is not counted stops being an exception. This pins the set of service names the
    kernel spawns on its OWN initiative - not via the Spawn syscall, which is mechanism a service asked
    for - so a second one has to be argued for rather than merely added.
    """
    # THE SHAPE IS THE POLICY. This is a single string, deliberately not a list, because a list is an
    # invitation: it has room for a second entry and appending one looks like configuration rather than
    # a constitutional change. A scalar cannot be appended to. Adding a second kernel-spawned service
    # would mean changing the SCHEMA - visible, arguable, and impossible to do absent-mindedly.
    #
    # Both wrong shapes are refused rather than tolerated, or the affordance leaks straight back in.
    if "kernel_spawned_services" in pins:
        return [Violation("COMMANDMENTS.baseline.toml", 0,
                          "`kernel_spawned_services` is a LIST. It is deliberately singular - "
                          "`kernel_spawned_service = \"supervisor\"` - because the kernel spawns exactly "
                          "one service and a list has room for a second. Restore the scalar.")]
    one = pins.get("kernel_spawned_service")
    if one is not None and not isinstance(one, str):
        return [Violation("COMMANDMENTS.baseline.toml", 0,
                          "`kernel_spawned_service` must be a single string. The shape carries the rule: "
                          "exactly one service, named, not a collection that can grow.")]
    pinned = {one} if one else set()
    found = {}
    for base, _, names in os.walk(os.path.join(ROOT, "kernel/src")):
        for n in sorted(names):
            if not n.endswith(".rs"):
                continue
            f = os.path.join(base, n)
            for m in re.finditer(r'spawn_service_with_config\(\s*"([a-z0-9-]+)"', read(f)):
                found.setdefault(m.group(1), os.path.relpath(f, ROOT).replace("\\", "/"))
    out = [Violation(path, 0,
                     f"the kernel spawns '{svc}' on its own initiative. The kernel restarts exactly ONE "
                     f"service - the supervisor - and only because Commandment V leaves nothing else "
                     f"beneath it to do so. A second one is a new kernel responsibility.")
           for svc, path in sorted(found.items()) if svc not in pinned]
    out += [Violation("kernel/src", 0,
                      f"'{svc}' is pinned as kernel-spawned but nothing spawns it - update the pin.")
            for svc in sorted(pinned - set(found))]
    return out



def check_introspect_queries(check, pins):
    """Commandment I: a syscall that carries a QUERY SPACE is a second dispatch table.

    `InspectKernel` is one pinned syscall with 23 sub-queries behind it, and each query is a distinct
    thing the kernel will answer: allocator counts, scheduler ticks, RTC time, framebuffer dimensions,
    PCI ids, a hardware random number. Adding a 24th is a new kernel responsibility with NO visible
    surface change - the syscall count stays exactly where it was.

    This is the general lesson of Commandment I, not a quirk of one syscall: pins catch ADDITION at the
    surface they pin, and miss growth INSIDE it. Any syscall that dispatches on an id needs its id space
    pinned too, or the pin above it is only watching the door while the room extends out the back.
    """
    src = read(os.path.join(ROOT, "kernel/src/syscall/dispatch.rs"))
    body = re.search(r"fn handle_inspect_kernel\(.*?^\}", src, re.S | re.M)
    if not body:
        return [Violation("kernel/src/syscall/dispatch.rs", 0,
                          "cannot find handle_inspect_kernel: the query pin cannot be verified, and an "
                          "unverifiable pin is a failure, never a pass")]
    found = {int(m) for m in re.findall(r"^\s+(\d+) =>", body.group(0), re.M)}
    pinned = {int(q) for q in pins.get("introspect_queries", [])}
    out = [Violation("kernel/src/syscall/dispatch.rs", 0,
                     f"InspectKernel query {q} is NOT pinned. A new query is a new kernel "
                     f"responsibility that changes no visible surface - the syscall count stays the "
                     f"same. Ask 'why isn't this a service?', then add it to [kernel] "
                     f"introspect_queries deliberately.")
           for q in sorted(found - pinned)]
    out += [Violation("kernel/src/syscall/dispatch.rs", 0,
                      f"pinned InspectKernel query {q} is gone - update the pin so the record stays "
                      f"true.")
            for q in sorted(pinned - found)]
    return out



def check_kernel_features(check, pins):
    """Commandment I: a feature flag is a switch on what the kernel IS.

    Features gate whole behaviours, so an unpinned one can add a responsibility that no other pin sees -
    a different scheduler entry, a different boot path, a service the kernel spawns itself. The C1-1
    finding lives behind exactly two of them.

    Test-only features count too. A build the kernel can be put into is a build someone can ship.
    """
    src = read(os.path.join(ROOT, "kernel/Cargo.toml"))
    block = re.search(r"^\[features\](.*?)(?=^\[|\Z)", src, re.S | re.M)
    found = set(re.findall(r"^([a-z0-9-]+)\s*=", block.group(1), re.M)) if block else set()
    pinned = set(pins.get("features", []))
    out = [Violation("kernel/Cargo.toml", 0,
                     f"new kernel feature '{f}': a switch on what the kernel IS, which can add a "
                     f"responsibility no other pin sees. Add it to [kernel] features deliberately.")
           for f in sorted(found - pinned)]
    out += [Violation("kernel/Cargo.toml", 0,
                      f"pinned feature '{f}' is gone - update the pin so the record stays true.")
            for f in sorted(pinned - found)]
    return out


def check_kernel_service_table(check, pins):
    """Commandment I / 26.10: the kernel holds per-service POLICY, so pin whose.

    `service_config` gives the kernel a table of every service it can start - memory limit, placement
    core, capabilities, send peers, embedded ELF. That is policy, and policy belongs in services (26.10);
    the kernel keeps it because it is also the loader. Whatever the merits, the SET is a fact about how
    much of userspace the kernel knows, and it should not grow unnoticed.

    Pinned by name, all of them, deliberately without trying to separate "real" services from test
    probes. Any rule for that split would be a judgment encoded as a pattern, and the next service named
    outside the pattern would slip through.
    """
    src = read(os.path.join(ROOT, "kernel/src/task/mod.rs"))
    found = set(re.findall(r'"([a-z0-9-]+)" => Some\(\(', src))
    pinned = set(pins.get("service_configs", []))
    out = [Violation("kernel/src/task/mod.rs", 0,
                     f"the kernel holds a service config for '{f}'. This list is DEBT, not an allowance: "
                     f"kernel responsibility does not expand, so it may only ever shrink. A service's "
                     f"memory limit, placement, capabilities and peers are the SUPERVISOR's policy. "
                     f"Do not add a line here.")
           for f in sorted(found - pinned)]
    out += [Violation("kernel/src/task/mod.rs", 0,
                      f"pinned service config '{f}' is gone - update the pin.")
            for f in sorted(pinned - found)]
    return out



# --------------------------------------------------------------------------------------------------
# Commandment II - thou shalt love Chaos and trust in it
# --------------------------------------------------------------------------------------------------

def check_chaos_exclusions(check, pins):
    """Commandment II: pin who does NOT face Maximum Carnage.

    Chaos has no hardcoded target list - it scans the live task table, so a new service is a candidate
    automatically and there is no list to drift. Good design, and it moves the whole risk to the other
    end: the EXCLUSIONS.

    `is_transient()` is three lines inside the chaos service, and every name in it is a service that
    never faces Maximum Carnage. Adding a fourth is how a service becomes special (Commandment V) while
    every suite still reports green - the storm simply never reaches it, and nothing says so.

    This is the same shape as a check narrowing its own scan: exclusions belong in DATA, with a reason
    each, printed and reviewed. It is pinned here rather than in the chaos service so that widening it
    is a baseline diff, not a one-word edit to a predicate nobody re-reads.
    """
    src = read(os.path.join(ROOT, "services/chaos/src/main.rs"))
    body = re.search(r"fn is_transient\(.*?^\}", src, re.S | re.M)
    if not body:
        return [Violation("services/chaos/src/main.rs", 0,
                          "cannot find `is_transient`: who is excluded from Maximum Carnage cannot be "
                          "verified, and an unverifiable exclusion set is a failure, never a pass")]
    found = set(re.findall(r'name == "([a-z0-9-]+)"', body.group(0)))
    found |= {m + "*" for m in re.findall(r'starts_with\("([a-z0-9-]+)"\)', body.group(0))}
    pinned = set(pins.get("chaos_exclusions", []))
    out = [Violation("services/chaos/src/main.rs", 0,
                     f"'{f}' is excluded from Maximum Carnage and is NOT pinned. A service that chaos "
                     f"never reaches is a service that is special (Commandment V), while every suite "
                     f"still reports green. If it must be excluded, add it to [kernel] "
                     f"chaos_exclusions with the reason.")
           for f in sorted(found - pinned)]
    out += [Violation("services/chaos/src/main.rs", 0,
                      f"'{f}' is pinned as chaos-excluded but no longer is - delete the pin. Every "
                      f"service it stops excluding is one more that now faces the storm, which is the "
                      f"direction this should always move.")
            for f in sorted(pinned - found)]
    return out



def check_kernel_responsibilities(check, pins):
    """Commandment I, stated as a NUMBER: the kernel has six responsibilities and no more.

    4.3 names them - memory isolation, scheduling, IPC, capability enforcement, interrupt routing,
    cross-core routing - and 4.4 says "nothing else". Every other Commandment I check pins a surface;
    this one pins the COUNT, which is the thing the commandment is actually about.

    Every top-level module must claim one of the six, or a support role that is sanctioned SOMEWHERE
    ELSE in the constitution and cites where. A module that can claim neither IS a seventh
    responsibility, whatever its size - which is how a responsibility gets added without any surface
    changing at all.

    The claims are data in the baseline, so they are reviewable in a diff. A module can still claim
    dishonestly; the check moves that lie somewhere it can be argued with.
    """
    six = set(pins.get("kernel_responsibilities", []))
    support = pins.get("kernel_support_roles", {})
    claims = pins.get("module_responsibility", {})
    out = []
    if len(six) != 6:
        out.append(Violation("COMMANDMENTS.baseline.toml", 0,
                             f"kernel_responsibilities lists {len(six)}, not 6. 4.3 names exactly six "
                             f"and 4.4 says 'nothing else'. Changing this number is amending the "
                             f"constitution, not editing a config."))
    found = {n for n in os.listdir(os.path.join(ROOT, "kernel/src")) if not n.startswith(".")}
    for m in sorted(found):
        c = claims.get(m)
        if c is None:
            out.append(Violation("kernel/src/" + m, 0,
                                 "this module claims no kernel responsibility. Name which of the six "
                                 "(4.3) it serves, or which sanctioned support role and where the "
                                 "constitution sanctions it."))
        elif c not in six and c not in support:
            out.append(Violation("kernel/src/" + m, 0,
                                 f"claims '{c}', which is neither one of the six responsibilities "
                                 f"(4.3) nor a sanctioned support role. If the kernel genuinely does "
                                 f"this, it is a SEVENTH responsibility and needs an amendment."))
    for m in sorted(set(claims) - found):
        out.append(Violation("kernel/src/" + m, 0,
                             "a responsibility is claimed for a module that no longer exists - delete "
                             "the claim so the record stays true."))
    return out


REQUIRED_PLAIN_KEYS = [
    "modules", "dependencies", "features", "service_configs", "arch_permitted_roles",
    "introspect_queries", "kernel_spawned_service", "kernel_responsibilities", "chaos_exclusions",
]


def check_baseline_shape(check, pins):
    """The baseline's own shape, because TOML will swallow a key without saying so.

    Every key after a `[kernel.sub]` header belongs to that sub-table, so a plain key appended below one
    silently joins it and `pins.get("thing")` quietly returns nothing - which reads as an EMPTY pin, and
    an empty pin passes everything. This happened four times while writing these checks; the self-test
    caught it each time, but only because a probe happened to cover it.

    A pin that vanishes must fail loudly rather than pass vacuously.
    """
    return [Violation("COMMANDMENTS.baseline.toml", 0,
                      f"[kernel] has no '{k}' key. Either it was deleted, or it was appended below a "
                      f"[kernel.sub-table] header and TOML swallowed it into that table - in which case "
                      f"the check that reads it is silently passing everything. Plain keys must come "
                      f"BEFORE any sub-table.")
            for k in REQUIRED_PLAIN_KEYS if k not in pins]


CHECKS = [
    dict(id="integrity-baseline", commandment="-",
         title="the baseline's own pins are readable",
         kind="custom", fn=check_baseline_shape,
         scope="COMMANDMENTS.baseline.toml, [kernel] plain keys",
         proves="no pin has silently vanished into a sub-table, where it would read as empty and "
                "therefore pass everything",
         does_not_prove="that the pins CONTAIN the right things - only that they exist to be read",
         probes=[
             dict(why="a vanished pin must be caught", pins={}, expect=True),
             dict(why="the real baseline must be readable", pins=None, expect=False),
         ]),
    dict(id="I-syscalls", commandment="I", title="the syscall surface is pinned",
         kind="custom", fn=check_syscall_surface,
         scope="kernel/src/syscall/dispatch.rs, enum SyscallNumber",
         proves="no syscall exists that was not deliberately admitted to the kernel's surface",
         does_not_prove="that an admitted syscall is mechanism rather than policy - that is review",
         probes=[
             dict(why="a new syscall must be caught",
                  pins={"syscalls": {}}, expect=True),
             dict(why="the real surface, fully pinned, must pass",
                  pins=None, expect=False),
         ]),
    dict(id="I-modules", commandment="I", title="kernel top-level modules are pinned",
         kind="custom", fn=check_kernel_modules,
         scope="kernel/src/* (top level only)",
         proves="no new top-level kernel responsibility appeared without being named",
         does_not_prove="that an EXISTING module has not grown new responsibilities inside itself. "
                        "A device driver added under arch/ is invisible to this check",
         probes=[
             dict(why="a new kernel module must be caught", pins={"modules": []}, expect=True),
             dict(why="the real module set, fully pinned, must pass", pins=None, expect=False),
         ]),
    dict(id="I-authorities", commandment="I", title="the kernel's own authorities are pinned",
         kind="custom", fn=check_kernel_authorities,
         scope="kernel/src/capability/mod.rs, well-known ResourceIds",
         proves="the kernel mints authority over nothing that was not deliberately admitted",
         does_not_prove="that an admitted authority is granted to the right services - that is "
                        "Commandment VII, and it is not built yet",
         probes=[
             dict(why="a new kernel authority must be caught", pins={"authorities": {}}, expect=True),
             dict(why="the real, fully pinned authority set must pass", pins=None, expect=False),
         ]),
    dict(id="I-kernel-deps", commandment="I", title="what the kernel links is pinned",
         kind="custom", fn=check_kernel_dependencies,
         scope="kernel/Cargo.toml, all [dependencies] blocks including target-gated ones",
         proves="no crate runs in ring 0 that was not deliberately admitted",
         does_not_prove="anything about what those crates DO. A pinned dependency is trusted code "
                        "inside the TCB that none of these checks read",
         probes=[
             dict(why="a new kernel dependency must be caught", pins={"dependencies": []}, expect=True),
             dict(why="the real, fully pinned dependency set must pass", pins=None, expect=False),
         ]),
    dict(id="I-kernel-spawns", commandment="I",
         title="the kernel spawns exactly one service, and it is the supervisor",
         kind="custom", fn=check_kernel_spawns,
         scope="every spawn_service_with_config(\"name\") in kernel/src",
         proves="the kernel initiates a spawn for no service but the pinned one",
         does_not_prove="that the exception is still WARRANTED. It is warranted by Commandment V "
                        "having nothing beneath the supervisor to restart it; if that ever changes, "
                        "this pin should shrink to nothing",
         probes=[
             dict(why="a second kernel-spawned service must be caught",
                  pins={"kernel_spawned_service": "supervisor"}, expect=True),
             dict(why="a pin for a service nothing spawns must be caught",
                  pins={"kernel_spawned_service": "ghost"}, expect=True),
             dict(why="turning the pin back into a LIST must be refused - the shape is the policy",
                  pins={"kernel_spawned_services": ["supervisor"]}, expect=True),
             dict(why="any non-string shape must be refused for the same reason",
                  pins={"kernel_spawned_service": ["supervisor"]}, expect=True),
         ]),
    dict(id="I-introspect", commandment="I", title="the InspectKernel query space is pinned",
         kind="custom", fn=check_introspect_queries,
         scope="kernel/src/syscall/dispatch.rs, handle_inspect_kernel query ids",
         proves="the kernel answers no introspection query that was not deliberately admitted",
         does_not_prove="that OTHER id-dispatching surfaces are pinned. Pins catch addition at the "
                        "surface they pin and miss growth inside it - this check exists because "
                        "InspectKernel grew a second dispatch table behind an already-pinned syscall",
         probes=[
             dict(why="a new introspection query must be caught",
                  pins={"introspect_queries": []}, expect=True),
             dict(why="a pinned query that no longer exists must be caught",
                  pins={"introspect_queries": list(range(0, 40))}, expect=True),
             dict(why="the real, fully pinned query space must pass", pins=None, expect=False),
         ]),
    dict(id="I-features", commandment="I", title="kernel feature flags are pinned",
         kind="custom", fn=check_kernel_features,
         scope="kernel/Cargo.toml [features]",
         proves="no build configuration of the kernel exists that was not deliberately admitted",
         does_not_prove="what a feature DOES. A pinned feature can still gate a new responsibility - "
                        "that is what the other checks are for",
         probes=[
             dict(why="a new kernel feature must be caught", pins={"features": []}, expect=True),
             dict(why="the real, fully pinned feature set must pass", pins=None, expect=False),
         ]),
    dict(id="I-service-table", commandment="I",
         title="the kernel's per-service policy table is pinned",
         kind="custom", fn=check_kernel_service_table,
         scope="kernel/src/task/mod.rs, service_config entries",
         proves="the kernel's catalogue of userspace policy has not grown",
         does_not_prove="that ANY of it belongs there. This is recorded debt, not an allowance: the "
                        "target is one entry - the supervisor, which the kernel must bootstrap - with "
                        "every other service's policy owned by the supervisor and its image handed to "
                        "the Spawn syscall. 218 is the distance from that",
         probes=[
             dict(why="a new kernel service config must be caught",
                  pins={"service_configs": []}, expect=True),
             dict(why="the real, fully pinned service set must pass", pins=None, expect=False),
         ]),
    dict(id="I-responsibilities", commandment="I",
         title="the kernel has six responsibilities, and every module claims one",
         kind="custom", fn=check_kernel_responsibilities,
         scope="kernel/src/* against the six of 4.3 plus sanctioned support roles",
         proves="no top-level module serves something outside the six without saying so",
         does_not_prove="that a module's CLAIM is honest, or that a module serving one of the six "
                        "has not grown a second job inside itself",
         probes=[
             dict(why="a module claiming nothing must be caught",
                  pins={"kernel_responsibilities": ["a", "b", "c", "d", "e", "f"],
                        "module_responsibility": {}}, expect=True),
             dict(why="a module claiming something outside the six must be caught",
                  pins={"kernel_responsibilities": ["a", "b", "c", "d", "e", "f"],
                        "module_responsibility": {"memory": "filesystem"}}, expect=True),
             dict(why="changing the NUMBER of responsibilities must be caught",
                  pins={"kernel_responsibilities": ["a"], "module_responsibility": {}}, expect=True),
             # Reality: four modules claim nothing today (C1-6). This probe asserts that, so closing
             # those findings BREAKS it deliberately and forces the expectation to be flipped - the
             # same ratchet the baseline uses, applied to the corpus.
             dict(why="the real module set still has unclaimed modules (C1-6 open)",
                  pins=None, expect=True),
         ]),
    dict(id="I-arch-drivers", commandment="I", title="no peripheral device driver lives in the kernel",
         kind="custom", fn=check_arch_device_drivers,
         scope="every .rs under kernel/src/arch, by declared role",
         proves="no file under arch/ is an undeclared or peripheral-driver responsibility",
         does_not_prove="that a file's DECLARED role is its true one. A USB stack labelled 'timer' "
                        "passes this check and fails review - the role is a claim, made in a diff",
         probes=[
             dict(why="an unclassified arch file must be caught",
                  pins={"arch_roles": {}, "arch_permitted_roles": ["mmu"]}, expect=True),
             dict(why="a device-driver role must be caught",
                  pins={"arch_roles": {"arm/dwc2.rs": "usb-host-stack"},
                        "arch_permitted_roles": ["mmu"]}, expect=True),
             dict(why="a role pinned for a file that no longer exists must be caught",
                  pins={"arch_roles": {"x86_64/deleted.rs": "mmu"},
                        "arch_permitted_roles": ["mmu"]}, expect=True),
             dict(why="the real, fully classified arch layer must reach only its known drivers",
                  pins=None, expect=True),
         ]),
    dict(id="II-chaos-exclusions", commandment="II",
         title="who is excluded from Maximum Carnage is pinned",
         kind="custom", fn=check_chaos_exclusions,
         scope="services/chaos/src/main.rs, is_transient()",
         proves="no service was quietly removed from the storm's reach",
         does_not_prove="that chaos was ever RUN, or passed. That is the runtime half of this "
                        "Commandment and it is not built: `chaos max-carnage` is still an operator's "
                        "good intentions rather than a gate with a threshold",
         probes=[
             dict(why="a newly excluded service must be caught",
                  pins={"chaos_exclusions": []}, expect=True),
             dict(why="a pin for a service no longer excluded must be caught",
                  pins={"chaos_exclusions": ["chaos", "mem-pressure", "observe*", "ghost"]},
                  expect=True),
             dict(why="the real, fully pinned exclusion set must pass", pins=None, expect=False),
         ]),
    dict(id="V-no-panic", commandment="V", title="no service may halt the machine",
         kind="source", dirs=["services"],
         exclude=[dict(glob="build.rs",
                       reason="host build scripts: they run on the developer's machine at build time, "
                              "never on GodspeedOS, so a panic there correctly fails the build")],
         pattern=r"\.unwrap\(\)|\.expect\(|(^|[^a-z_])panic!|todo!|unimplemented!|unreachable!",
         fix="a service must never halt the machine: return a loud error instead "
             "(unwrap_or / unwrap_or_else / let-else / an explicit match)",
         proves="no service contains a construct that panics by design",
         does_not_prove="that a service cannot HANG, which is the other half of the Rule Above The "
                        "Rules and needs the runtime dependency matrix",
         probes=[
             dict(why="unwrap must be caught", code="let x = foo.unwrap();", expect=True),
             dict(why="expect must be caught", code='let x = foo.expect("no");', expect=True),
             dict(why="panic! must be caught", code='panic!("dead");', expect=True),
             dict(why="todo! must be caught", code="todo!();", expect=True),
             dict(why="unreachable! must be caught", code="unreachable!();", expect=True),
             dict(why="unimplemented! must be caught", code="unimplemented!();", expect=True),
             dict(why="unwrap_or is the FIX, never a violation",
                  code="let x = foo.unwrap_or(0);", expect=False),
             dict(why="unwrap_or_else is the FIX, never a violation",
                  code="let x = foo.unwrap_or_else(|| 0);", expect=False),
             dict(why="let-else is the FIX, never a violation",
                  code="let Some(x) = foo else { return };", expect=False),
             dict(why="a comment explaining the rule is not a violation of it",
                  code="// never call .unwrap() in a service", expect=False),
         ]),
    dict(id="VI-static-mut", commandment="VI", title="no unowned global mutable state in services",
         kind="source", dirs=["services"],
         exclude=[dict(glob="build.rs", reason="host build scripts, as above")],
         pattern=r"\bstatic\s+mut\b",
         fix="unowned global mutable state: give it an owner, or pass it explicitly",
         proves="no service holds `static mut`",
         does_not_prove="that state is owned by the RIGHT service, or that two services hold two "
                        "irreducible copies of one truth (Commandment III)",
         probes=[
             dict(why="static mut must be caught", code="static mut COUNT: u32 = 0;", expect=True),
             dict(why="an immutable static is fine", code="static COUNT: u32 = 0;", expect=False),
         ]),
]

# Commandments with no mechanical check yet. Printed on EVERY report so the gap cannot be forgotten.
UNMECHANISED = [
    ("II", "runtime - HALF built", "Chaos. Who is EXCLUDED from the storm is pinned; whether the "
                                   "storm was ever run and passed is not. `chaos max-carnage` needs "
                                   "to become a gate with a threshold, not an operator's good "
                                   "intentions."),
    ("III", "judgment", "One irreducible truth. Whether a stored value is a derived view or a second "
                        "truth is a design question: does it reduce to one source, and does that "
                        "source win?"),
    ("IV", "static, not built", "Contracts. scripts/contract_check.py already reconciles declared "
                                "capabilities against kernel grants; fold it in here."),
    ("V", "runtime, not built", "The other half of V: a service must not HANG. Kill each dependency "
                                "and assert every caller still answers."),
    ("VII", "static, not built", "Ambient authority. Every capability-taking syscall must validate a "
                                 "capability before acting, and by-name kernel grants must be listed."),
    ("VIII", "static heuristic, not built", "Wait on truth. A bound expressed in ITERATIONS rather "
                                            "than a clock, and a sleep in a loop whose exit never "
                                            "reads the thing awaited."),
    ("IX", "runtime, not built", "Recovery. If recovery cannot be tested, it does not exist."),
    ("X", "judgment", "Complexity in the layer that owns it. No machine decides this."),
]


def run_check(check, pins):
    return scan_source(check, pins) if check["kind"] == "source" else check["fn"](check, pins)


# --------------------------------------------------------------------------------------------------

def load_baseline():
    import tomllib
    if not os.path.exists(BASELINE):
        return {}, []
    with open(BASELINE, "rb") as f:
        data = tomllib.load(f)
    return data.get("kernel", {}), data.get("exemption", [])


def apply_baseline(check_id, viols, exemptions):
    """Return (unexcused, ratchet_errors). The baseline may only ever shrink."""
    by_path = {}
    for v in viols:
        by_path.setdefault(v.path, []).append(v)

    unexcused, ratchet = [], []
    for ex in [e for e in exemptions if e.get("check") == check_id]:
        path, count = ex.get("path", ""), int(ex.get("count", 0))
        if not ex.get("amendment") or "TODO" in str(ex.get("reason", "TODO")):
            ratchet.append(f"{check_id} {path}: an exemption must cite a CLAUDE.md amendment that "
                           f"already accepts this, and give a real reason. If nothing accepts it, "
                           f"fix the violation or amend the constitution deliberately.")
            continue
        actual = len(by_path.get(path, []))
        if actual == 0:
            ratchet.append(f"{check_id} {path}: STALE exemption, nothing matches it. Delete the "
                           f"entry - the debt is paid.")
        elif actual < count:
            ratchet.append(f"{check_id} {path}: {actual} violations remain but the baseline allows "
                           f"{count}. Lower it to {actual}: a baseline not tightened when the debt "
                           f"shrinks rots into a permanent exemption.")

    for path, vs in by_path.items():
        allowed = max([int(e.get("count", 0)) for e in exemptions
                       if e.get("check") == check_id and e.get("path") == path
                       and e.get("amendment")] or [0])
        if len(vs) > allowed:
            unexcused.extend(vs[allowed:] if allowed else vs)
    return unexcused, ratchet


def selftest():
    """Prove every check still fires. A guard never observed firing is not evidence."""
    print(f"\n{BOLD}COMMANDMENT CHECKS - self-test{OFF}")
    print(f"{DIM}  Known-bad code each check must catch, and known-good code it must not.{OFF}\n")
    failed = 0
    for c in CHECKS:
        for p in c["probes"]:
            if c["kind"] == "source":
                rx = re.compile(c["pattern"])
                sk = re.compile(c["skip"]) if c.get("skip") else None
                got = matches_line(p["code"], rx, sk)
            else:
                pins, _ = load_baseline()
                got = bool(c["fn"](c, p["pins"] if p["pins"] is not None else pins))
            ok = (got == p["expect"])
            failed += 0 if ok else 1
            mark = f"{GREEN}ok{OFF}" if ok else f"{RED}BROKEN{OFF}"
            print(f"  {mark:<18} {c['id']:<14} {p['why']}")
            if not ok:
                print(f"      {RED}this check no longer does what it claims. Was it weakened?{OFF}")
    total = sum(len(c["probes"]) for c in CHECKS)
    if failed:
        print(f"\n{RED}{BOLD}  {failed}/{total} probes BROKEN - the enforcement layer is damaged.{OFF}")
        print(f"  A weakened check is worse than no check: it reports PASS over code it stopped "
              f"looking at.\n")
        return 1
    print(f"\n{GREEN}  {total}/{total} probes pass - every check still fires.{OFF}\n")
    return 0


def report(pins, exemptions):
    print(f"\n{BOLD}THE TEN COMMANDMENTS OF GODSPEED - mechanical enforcement{OFF}\n")
    for c in CHECKS:
        unexcused, ratchet = apply_baseline(c["id"], run_check(c, pins), exemptions)
        n = len(unexcused) + len(ratchet)
        mark = f"{GREEN}PASS{OFF}" if n == 0 else f"{RED}FAIL ({n}){OFF}"
        print(f"  {c['commandment']:>4}  {c['id']:<14} {mark}  {c['title']}")
        if c["kind"] == "source":
            inc, exc = files_in_scope(c)
            print(f"        scanned:     {len(inc)} files in {', '.join(c['dirs'])}")
            for e in c.get("exclude", []):
                print(f"        EXCLUDED:    {exc} x {e['glob']} - {e['reason']}")
        else:
            print(f"        scanned:     {c['scope']}")
        print(f"        proves:      {c['proves']}")
        print(f"        DOES NOT:    {c['does_not_prove']}")
        print()
    print(f"  {BOLD}Not mechanised - human review, every time:{OFF}")
    for num, bucket, why in UNMECHANISED:
        print(f"  {num:>4}  {DIM}[{bucket}]{OFF} {why}")
    covered = len({c["commandment"] for c in CHECKS})
    print(f"\n  {len(CHECKS)} checks cover {covered} of 10 commandments. The rest are NOT covered, "
          f"and a green build does not claim otherwise.\n")


def main():
    if "--selftest" in sys.argv:
        return selftest()
    pins, exemptions = load_baseline()
    if "--report" in sys.argv:
        report(pins, exemptions)
        return 0

    failures, ratchets = [], []
    for c in CHECKS:
        unexcused, ratchet = apply_baseline(c["id"], run_check(c, pins), exemptions)
        failures += [(c, v) for v in unexcused]
        ratchets += ratchet

    if not failures and not ratchets:
        covered = len({c["commandment"] for c in CHECKS})
        print(f"{GREEN}commandments: {len(CHECKS)} checks pass{OFF} "
              f"({covered}/10 mechanised, {len(UNMECHANISED)} need human review - see --report)")
        return 0

    print(f"\n{RED}{BOLD}{'=' * 94}{OFF}")
    print(f"{RED}{BOLD}  COMMANDMENT VIOLATION - the build stops here{OFF}")
    print(f"{RED}{BOLD}{'=' * 94}{OFF}\n")
    for c, v in failures:
        where = f"{v.path}:{v.line}" if v.line else v.path
        print(f"  {BOLD}Commandment {c['commandment']}{OFF} - {c['title']}")
        print(f"    {where}")
        print(f"    {v.detail}\n")
    for r in ratchets:
        print(f"  {YELLOW}{BOLD}baseline{OFF}  {r}\n")
    print("  COMMANDMENTS.md is the law; docs/anti-patterns.md has the correct pattern.")
    print("  An exemption is legitimate ONLY if a CLAUDE.md amendment already accepts it.\n")
    return 1


if __name__ == "__main__":
    sys.exit(main())
