"""Inject a real violation for each currently-GREEN check, run the real checker, restore.

The self-test corpus proves the check FUNCTIONS fire on synthetic input. This proves they fire against
the real tree, through the real file-reading path - a different claim, and the gap between them has
already hidden a bug once (a pin vanished into a TOML sub-table while every probe kept passing, because
the probes handed in explicit pins and never read the file).

Only the green checks are exercised. Three checks are already failing against real code right now
(I-kernel-spawns, I-responsibilities, II-chaos-exclusions), so they are validated by reality.
"""
import io, os, re, subprocess, sys

ANSI = re.compile(chr(27) + r'\[[0-9;]*m')

ROOT = r"C:\Downloads\Bankole\GodspeedOS\github\godspeed"
os.chdir(ROOT)

def run():
    p = subprocess.run([sys.executable, "scripts/commandments.py"], capture_output=True, text=True)
    return ANSI.sub('', p.stdout + p.stderr)

def edit(path, old, new):
    p = os.path.join(ROOT, path)
    s = io.open(p, encoding="utf-8").read()
    assert old in s, "anchor not found in %s" % path
    io.open(p, "w", encoding="utf-8").write(s.replace(old, new, 1))

def create(path, text):
    io.open(os.path.join(ROOT, path), "w", encoding="utf-8").write(text)

def restore(paths, created=()):
    for c in created:
        f = os.path.join(ROOT, c)
        if os.path.exists(f):
            os.remove(f)
    if paths:
        subprocess.run(["git", "checkout", "--"] + list(paths), capture_output=True)

CASES = [
    ("I-syscalls", "a 50th syscall",
     lambda: edit("kernel/src/syscall/dispatch.rs",
                  "    SetClock               = 50,",
                  "    SetClock               = 50,\n    SecretBackdoor         = 51,"),
     ["kernel/src/syscall/dispatch.rs"], []),

    ("I-introspect", "a 24th InspectKernel query",
     lambda: edit("kernel/src/syscall/dispatch.rs",
                  "        22 => crate::wallclock::floor(),",
                  "        22 => crate::wallclock::floor(),\n        23 => 0xdead,"),
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

    ("I-service-table", "a 219th service config in the kernel",
     lambda: edit("kernel/src/task/mod.rs", '        "nic-driver" => Some(("nic-driver"',
                  '        "ghostsvc" => Some(("ghostsvc", ServiceConfig { }));\n'
                  '        "nic-driver" => Some(("nic-driver"'),
     ["kernel/src/task/mod.rs"], []),

    ("integrity-baseline", "a pin stranded under a sub-table (the bug that hit 4x)",
     lambda: edit("COMMANDMENTS.baseline.toml", "introspect_queries = [",
                  "[kernel.syscalls]\nintrospect_queries = ["),
     ["COMMANDMENTS.baseline.toml"], []),

    ("V-no-panic", "a service that can halt the machine",
     lambda: edit("services/logger/src/main.rs", "#[no_mangle]",
                  "fn boom() { let x: Option<u32> = None; let _ = x.unwrap(); }\n#[no_mangle]"),
     ["services/logger/src/main.rs"], []),

    ("VI-static-mut", "unowned global mutable state in a service",
     lambda: edit("services/logger/src/main.rs", "#[no_mangle]",
                  "static mut SNEAK: u32 = 0;\n#[no_mangle]"),
     ["services/logger/src/main.rs"], []),
]

base = run()
n_base = base.count('  Commandment')
base_lines = set(l.strip() for l in base.split(chr(10)))
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
    caught = out.count("  Commandment") > n_base
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
