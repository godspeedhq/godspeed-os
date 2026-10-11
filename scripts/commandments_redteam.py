"""DO NOT RUN THIS IN PARALLEL WITH ANYTHING, INCLUDING ITSELF.

It MUTATES the working tree: inject a violation, run the checker, restore. A concurrent reader sees the
injected state and reports a violation that is not really there; two concurrent copies corrupt each
other's restore and can leave the tree dirty. `scripts/commandments.py` itself is read-only and
deterministic - repeated runs are byte-identical - so parallelise THAT freely. This is the one that
cannot be.

Inject a real violation for each currently-GREEN check, run the real checker, restore.

The self-test corpus proves the check FUNCTIONS fire on synthetic input. This proves they fire against
the real tree, through the real file-reading path - a different claim, and the gap between them has
already hidden a bug once (a pin vanished into a TOML sub-table while every probe kept passing, because
the probes handed in explicit pins and never read the file).

Only the green checks are exercised. When this was written three checks were failing against real code
(I-kernel-spawns, I-responsibilities, II-chaos-exclusions) and so were validated by reality. (Note
2026-10-09: all three pass now and have no CASES row here except II, which has its own block below; the
case labels "52nd syscall", "23rd query" and "14th authority" are the counts of that time, not today's.)

It does not gate a build: it mutates the tree, so it runs alone, on a committed tree. Its EXIT CODE is
its verdict since 2026-10-10 (backlog/80 T8) - it exited 0 whatever it printed - so a MISSED or ANCHOR?
row, a WRONG Commandment II case, or a RESTORED count unequal to the baseline exits 1.
"""
import io, os, re, subprocess, sys

ANSI = re.compile(chr(27) + r'\[[0-9;]*m')

# THE TREE THIS SCRIPT IS IN. This was the literal path of one machine's checkout, so running the copy
# in any other checkout - a worktree, a contributor's clone - INJECTED VIOLATIONS INTO THAT ONE, a tree
# the caller was not looking at, and on any other machine it did not run at all (backlog/80 T8).
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(ROOT)

def run():
    p = subprocess.run([sys.executable, "scripts/commandments.py"], capture_output=True, text=True)
    return ANSI.sub('', p.stdout + p.stderr)

# Exact bytes of every file this run has touched, captured the FIRST time it is written.
#
# Restore used to be `git checkout -- <paths>`, which reverts to HEAD - and therefore silently DESTROYS
# any uncommitted work in a file an injection happens to touch. It did exactly that: a run mid-session
# threw away the console-service changes to `task/mod.rs` and `syscall/dispatch.rs` along with four
# `COMMANDMENTS.baseline.toml` edits, and the only symptom was the summary line disagreeing with the
# baseline. A tool that mutates the tree to measure it must put back what WAS there, not what git thinks
# should be there.
_ORIGINAL = {}

def _snapshot(p):
    if p not in _ORIGINAL:
        _ORIGINAL[p] = io.open(p, encoding="utf-8", newline="").read()

def edit(path, old, new):
    p = os.path.join(ROOT, path)
    s = io.open(p, encoding="utf-8").read()
    assert old in s, "anchor not found in %s" % path
    _snapshot(p)
    io.open(p, "w", encoding="utf-8").write(s.replace(old, new, 1))

def strand_pin(key):
    """Move a plain [kernel] key below every sub-table, where TOML silently swallows it."""
    import re as _re
    p = os.path.join(ROOT, "COMMANDMENTS.baseline.toml")
    s = io.open(p, encoding="utf-8").read()
    m = _re.search(r"(?m)^" + key + r" = \[[^\]]*\]" + chr(10), s)
    assert m, "pin %s not found" % key
    _snapshot(p)
    s = s.replace(m.group(0), "")
    i = s.index(chr(10) + "# ---")
    io.open(p, "w", encoding="utf-8").write(s[:i] + chr(10) + m.group(0) + s[i:])


def create(path, text):
    io.open(os.path.join(ROOT, path), "w", encoding="utf-8").write(text)

def restore(paths, created=()):
    for c in created:
        f = os.path.join(ROOT, c)
        if os.path.exists(f):
            os.remove(f)
    # Restore from the SNAPSHOT, never from git - see `_ORIGINAL`. Anything this run did not touch is
    # left exactly alone, so a dirty working tree survives a red-team run intact.
    for path in paths:
        p = os.path.join(ROOT, path)
        if p in _ORIGINAL:
            io.open(p, "w", encoding="utf-8", newline="").write(_ORIGINAL.pop(p))


def violations(out):
    """The SET of violation detail lines. Counting alone misses a SWAP - one violation replaced by
    another leaves the count unchanged, and that is precisely what an unreadable exclusion set does:
    the escape it could no longer see disappears, and a 'cannot verify' takes its place."""
    return set(l.strip() for l in out.split(chr(10))
               if l.startswith("    ") and l.strip() and not l.strip().startswith("Commandment"))

CASES = [
    ("I-syscalls", "a 52nd syscall",
     lambda: edit("kernel/src/syscall/dispatch.rs",
                  "    FireIrq                = 51,",
                  "    FireIrq                = 51,\n    SecretBackdoor         = 52,"),
     ["kernel/src/syscall/dispatch.rs"], []),

    ("I-introspect", "a 23rd InspectKernel query",
     lambda: edit("kernel/src/syscall/dispatch.rs",
                  "        21 => match crate::arch::imp::com2_try_read_byte() { Some(b) => b as i64, None => -1 },",
                  "        21 => match crate::arch::imp::com2_try_read_byte() { Some(b) => b as i64, None => -1 },\n        22 => 0xdead,"),
     ["kernel/src/syscall/dispatch.rs"], []),

    ("I-authorities", "a 14th kernel authority",
     lambda: edit("kernel/src/capability/mod.rs",
                  "pub const SET_CLOCK_RESOURCE: ResourceId = ResourceId(13);",
                  "pub const SET_CLOCK_RESOURCE: ResourceId = ResourceId(13);\n"
                  "pub const BACKDOOR_RESOURCE: ResourceId = ResourceId(14);"),
     ["kernel/src/capability/mod.rs"], []),

    ("I-kernel-deps", "a new ring-0 crate",
     lambda: edit("kernel/Cargo.toml", "[dependencies]",
                  '[dependencies]\nserde = "1"'),
     ["kernel/Cargo.toml"], []),

    ("I-features", "a new kernel feature",
     lambda: edit("kernel/Cargo.toml", "arm-supervisor = []",
                  "arm-supervisor = []\nsneaky-mode = []"),
     ["kernel/Cargo.toml"], []),

    ("I-arch-drivers (undeclared)", "a new arch file with no declared role",
     lambda: create("kernel/src/arch/x86_64/nvme.rs", "// a storage driver\n"),
     [], ["kernel/src/arch/x86_64/nvme.rs"]),

    ("I-arch-drivers (declared)", "an arch file HONESTLY declared a storage driver",
     lambda: (create("kernel/src/arch/x86_64/nvme.rs", "// a storage driver\n"),
              edit("COMMANDMENTS.baseline.toml", "[kernel.arch_roles]",
                   '[kernel.arch_roles]\n"x86_64/nvme.rs" = "storage-driver"')),
     ["COMMANDMENTS.baseline.toml"], ["kernel/src/arch/x86_64/nvme.rs"]),

    # ANCHOR REPAIRED 2026-09-04. This probe was INERT from the step-D merge until now: it anchored
    # on the `nic-driver` catalogue entry, and step D deleted the kernel's per-service
    # catalogue (222 entries -> 1). A probe whose anchor is gone injects NOTHING, so the check it
    # exists to exercise was never run - and it reported CAUGHT for a violation never introduced.
    # v0.13.0 shipped with this guard testing nothing. Re-anchored on the ONE surviving entry,
    # `supervisor`, the kernel's single direct spawn (11.1). If that is ever renamed or removed
    # the probe reports ANCHOR? rather than silently passing - which is how this was caught.
    ("I-service-table", "a 2nd service config in the kernel",
     lambda: edit("kernel/src/task/mod.rs", '        "supervisor" => Some(("supervisor"',
                  '        "ghostsvc" => Some(("ghostsvc", ServiceConfig { }));\n'
                  '        "supervisor" => Some(("supervisor"'),
     ["kernel/src/task/mod.rs"], []),

    ("integrity-baseline", "a pin stranded below a sub-table (the bug that hit 4x)",
     # REALISTIC stranding: the key is appended at the END of [kernel], after every sub-table, which is
     # what actually happened four times. An earlier version of this case injected a DUPLICATE
     # [kernel.syscalls] header instead - TOML rejects that outright, so it exercised the malformed-file
     # path rather than the swallow, and reported MISSED for a check that works. The injection has to be
     # the thing that really goes wrong, or the test measures something nobody would ever do.
     lambda: strand_pin("introspect_queries"),
     ["COMMANDMENTS.baseline.toml"], []),

    # ANCHORED ON THE ATTRIBUTE LINE, not the bare text. `edit` replaces the FIRST occurrence, and since
    # the crate gained its `#![deny(unsafe_code)]` note the first `#[no_mangle]` in the file is inside a
    # comment - so both probes planted their code INSIDE THAT COMMENT, where no check looks, and reported
    # MISSED for checks that work. Found the first time this script's exit code was read (2026-10-10).
    ("V-no-panic", "a service that can halt the machine",
     lambda: edit("services/events/src/main.rs", "\n#[no_mangle]\npub extern",
                  "\nfn boom() { let x: Option<u32> = None; let _ = x.unwrap(); }\n#[no_mangle]\npub extern"),
     ["services/events/src/main.rs"], []),

    ("VI-static-mut", "unowned global mutable state in a service",
     lambda: edit("services/events/src/main.rs", "\n#[no_mangle]\npub extern",
                  "\nstatic mut SNEAK: u32 = 0;\n#[no_mangle]\npub extern"),
     ["services/events/src/main.rs"], []),
]

base = run()
n_base = base.count('  Commandment')
base_lines = set(l.strip() for l in base.split(chr(10)))
base_v = violations(base)
print("BASELINE (before any injection): %d violations reported\n" % base.count("  Commandment"))

rows = []
for name, what, inject, paths, created in CASES:
    try:
        inject()
    except AssertionError as e:
        rows.append((name, what, "ANCHOR?", str(e)))
        restore(paths, created)
        continue
    out = run()
    # Detect by COUNT, not message text: the enforce output prints the commandment TITLE and never the
    # check id, so matching on the id could never have fired. (Fourth time this session a test agreed
    # with me instead of failing.) A count is text-independent and cannot flatter itself.
    caught = violations(out) - base_v != set()
    new = [l.strip() for l in out.split(chr(10)) if l.strip() and l.strip() not in base_lines]
    msg = new[0] if new else ""
    rows.append((name, what, "CAUGHT" if caught else "MISSED", msg))
    restore(paths, created)

after = run()
print("=" * 100)
for name, what, verdict, msg in rows:
    print("%-30s %-46s %-7s %s" % (name, what, verdict, msg[:60]))
print("=" * 100)
print("\nRESTORED: %d violations reported (must equal the baseline above)\n"
      % after.count("  Commandment"))
for name, what, verdict, msg in rows:
    if verdict != "CAUGHT":
        print("!!! %s -> %s\n%s\n" % (name, verdict, msg))

# ---------------------------------------------------------------------------------------------------
# Commandment II. One check, DERIVED rather than configured - so the test has to cover both directions:
# that it catches a service escaping, AND that it stays silent for the apparatus it must permit. A check
# that flags everything is as useless as one that flags nothing, and only the negative cases show which.
CH = "services/chaos/src/main.rs"

CASES_II = [
    ("II escape", "a real service quietly added to is_transient()",
     lambda: edit(CH, 'name == "chaos" || name == "mem-pressure"',
                      'name == "chaos" || name == "shell" || name == "mem-pressure"'),
     [CH], [], True),

    ("II permission lapses", "chaos stops spawning mem-pressure but still excludes it",
     lambda: edit(CH, 'ctx.spawn("mem-pressure")', 'ctx.spawn("something-else")'),
     [CH], [], True),

    ("II unfindable", "is_transient renamed - the exclusion set cannot be read",
     lambda: edit(CH, "fn is_transient(", "fn is_transient_renamed("),
     [CH], [], True),

    ("II NEGATIVE apparatus", "chaos + mem-pressure alone must NOT be flagged",
     # The predicate no longer carries an `observe` clause, so the old edit matched nothing and this
     # NEGATIVE probe silently stopped running - a check that cannot fail is not a check (Commandment
     # II), and one that cannot even APPLY is worse, because it still prints a line. Rewritten as an
     # equivalent reordering of the predicate as it stands, so it exercises the same claim: this set,
     # alone, must not be flagged.
     lambda: edit(CH, 'name == "chaos" || name == "mem-pressure"',
                      'name == "mem-pressure" || name == "chaos"'),
     [CH], [], False),

    ("II NEGATIVE extra spawn", "chaos spawning MORE must not create a violation",
     lambda: edit(CH, 'ctx.spawn("mem-pressure")',
                      'ctx.spawn("mem-pressure"); let _ = ctx.spawn("probe-recv")'),
     [CH], [], False),
]

print()
print("=" * 100)
print("COMMANDMENT II - the derived check, both directions")
print("=" * 100)
WRONG_II = []
for name, what, inject, paths, created, should_catch in CASES_II:
    try:
        inject()
    except AssertionError as e:
        print("%-24s %-52s ANCHOR? %s" % (name, what, e)); restore(paths, created)
        WRONG_II.append(name + " (anchor not found)"); continue
    out = run()
    n = out.count("  Commandment")
    changed = violations(out) - base_v
    # A catch shows as a violation line absent from the baseline; a correct NEGATIVE case shows none.
    # (The baseline once carried one Commandment II violation, `observe*` escaping; it carries none now.)
    caught = changed != set()
    ok = caught if should_catch else not caught
    print("%-24s %-52s %-6s %s" % (name, what, "CAUGHT" if caught else "silent",
                                   "OK" if ok else "*** WRONG ***"))
    if not ok:
        WRONG_II.append(name)
    restore(paths, created)

print()
restored_ii = run().count("  Commandment")
print("RESTORED: %d violations (baseline %d)" % (restored_ii, n_base))

# THE VERDICT IS THE EXIT CODE. This printed its table and exited 0 whatever it said, so a MISSED row - a
# check that no longer fires on the real tree - passed every script that ran it (backlog/80 T8). It
# still gates no build (it mutates the tree, so it must run alone, on a committed tree), but a caller
# can now trust its status.
failures = []
failures += ["%s: %s" % (name, verdict) for name, _what, verdict, _msg in rows if verdict != "CAUGHT"]
failures += ["II %s: *** WRONG ***" % name for name in WRONG_II]
if after.count("  Commandment") != n_base or restored_ii != n_base:
    failures.append("the tree did not restore to its baseline (%d, then %d, against %d)"
                    % (after.count("  Commandment"), restored_ii, n_base))
if failures:
    print("\nRED TEAM: %d failure(s) - a check that does not fire on the real tree is not a check:" % len(failures))
    for f in failures:
        print("  " + f)
    sys.exit(1)
print("\nRED TEAM: every injected violation was caught, every negative case stayed silent, the tree restored")
