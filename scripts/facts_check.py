#!/usr/bin/env python3
"""Fail when a doc states a number the CODE owns, and the two disagree.

WHY THIS EXISTS. `doc_refs.py` catches a doc pointing at a file that does not exist. This catches the
other half: a doc stating a FACT that used to be true. Both are the same disease - a second copy of
something whose truth lives elsewhere - and the second copy is the one that goes quietly stale,
because nothing breaks when it does.

Found by hand in one sitting, which is the argument for mechanising it:
  - CLAUDE.md's own test-category diagram said "Tests 1-11" while its prose said "Tests 1-15".
  - An osdev comment said "all 22 services" when there were 19 crates.
  - Three doc STATUS lines named branches that had been deleted.

THE RULE THIS ENFORCES. A number that comes from code is owned by the code. A doc may restate it -
prose is allowed to be readable - but the restatement is CHECKED. Where a doc does not need the
number at all, the better fix is to delete it rather than pin it, and that is not this script's
call to make.

WHAT IS DELIBERATELY NOT CHECKED. Dated evidence. `docs/ahci.md` recording "identity 23/23" is a true
statement about a run in the past, and rewriting it would falsify the record - the same exemption
`doc_refs.py` grants `audits/`. History states what WAS; only present-tense claims are checked.

Exit 0 when every checked fact agrees with its source, 1 otherwise.
"""

import io
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def read(rel):
    try:
        return io.open(os.path.join(ROOT, rel), encoding="utf-8", errors="ignore").read()
    except OSError:
        return ""


def const(rel, name):
    """The value of a `const NAME: ty = N;` in one file - the authoritative number."""
    m = re.search(r"const\s+%s\s*:\s*\w+\s*=\s*([0-9_]+)" % re.escape(name), read(rel))
    return int(m.group(1).replace("_", "")) if m else None


def enum_value(rel, name):
    """The value of `Name = N,` in a Rust enum."""
    m = re.search(r"^\s*%s\s*=\s*([0-9]+)\s*," % re.escape(name), read(rel), re.M)
    return int(m.group(1)) if m else None


def count_files(pattern_dir, pattern):
    import glob
    return len(glob.glob(os.path.join(ROOT, pattern_dir, pattern)))


# Each fact: a human name, the authoritative value, and the patterns that ASSERT it in prose.
# A pattern must capture the number, so a wrong number is what fails - not a missing mention.
def shared_surface():
    """(total, neutral-kernel) from the ratchet's baseline, or (None, None).

    CLAUDE.md 4.1 used to state this debt as a HAND COUNT - "neutral code still names `arm` in 8
    places, `aarch64` in 4 and `x86_64` in 3" - which had drifted to 6, 4 and 2 before anybody
    re-measured. The 2026-09-13 amendment replaces it with a measured figure, and a measured figure
    restated in prose is exactly what this script exists to keep honest: without this entry the new
    number would go stale the same way the old one did, only with more confidence behind it.
    """
    text = read("SHARED-SURFACE.baseline.txt")
    if not text:
        return None, None
    total = 0
    kern = 0
    for line in text.split("\n"):
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split(None, 1)
        if len(parts) != 2 or not parts[0].isdigit():
            continue
        n = int(parts[0])
        total += n
        if parts[1].strip().startswith("kernel/src"):
            kern += n
    return (total, kern) if total else (None, None)


def porting_tree_problems():
    """`docs/porting.md`'s tree annotates each file with its arch-conditional count. Check every one
    against `SHARED-SURFACE.baseline.txt`, which is the ratchet's own file and the only source.

    A tree of per-file numbers is exactly the shape that rots: CLAUDE.md 4.1 carried a hand count
    ("neutral code still names `arm` in 8 places, `aarch64` in 4 and `x86_64` in 3") that had drifted
    to 6, 4 and 2 before anybody re-measured, with nothing watching. The tree is more useful than that
    paragraph and would rot the same way, so it is checked line by line rather than trusted.
    """
    import re as _re
    base = read("SHARED-SURFACE.baseline.txt")
    doc = read("docs/porting.md")
    if not base or not doc:
        return []
    truth = {}
    for line in base.split("\n"):
        line = line.split("#", 1)[0].strip()
        parts = line.split(None, 1)
        if len(parts) == 2 and parts[0].isdigit():
            truth[parts[1].strip()] = int(parts[0])

    problems = []
    seen = set()
    # Tree rows look like:  `│   ├── task/scheduler.rs               [ 2 ]   - NOT yours...`
    for n, line in enumerate(doc.split("\n"), 1):
        m = _re.search(r'([A-Za-z0-9_./-]+\.rs|[A-Za-z0-9_./-]*build\.rs)\s+\[\s*(\d+)\s*\]', line)
        if not m:
            continue
        leaf, claimed = m.group(1), int(m.group(2))
        # The tree shows leaves; the baseline holds full paths. Match on suffix, which is
        # unambiguous here because no two baseline paths end the same way.
        hits = [k for k in truth if k.endswith(leaf)]
        if len(hits) != 1:
            if claimed == 0:
                continue  # "every other service [ 0 ]" and friends - a claim of absence, not a file
            problems.append(f"docs/porting.md:{n}: `{leaf}` matches {len(hits)} baseline entries")
            continue
        seen.add(hits[0])
        if truth[hits[0]] != claimed:
            problems.append(f"docs/porting.md:{n}: `{hits[0]}` says {claimed}, baseline says {truth[hits[0]]}")

    for path, n in sorted(truth.items()):
        if path not in seen:
            problems.append(f"docs/porting.md: `{path}` ({n} site(s)) is in the ratchet but ABSENT "
                            f"from the tree - a porter would not know to look at it")
    return problems


def wire_format_problems():
    """`fs` and the shell must agree on the LIST_DIR reply header size.

    Another CODE-versus-CODE check across a crate boundary, for the same reason as the budget
    ordering above: neither number looks wrong on its own, and nothing else compares them.

    THE RULE. A `LIST_DIR` reply opens `[FS_OK, count, more, next:u32]` and the entries follow. `fs`
    writes them at its `DIR_HDR`; the shell reads them at its own. If the two drift, every caller
    parses a name length out of the middle of the cursor and renders garbage - or worse, a plausible
    wrong name, since the bytes are real data.

    WHY IT IS CHECKED RATHER THAN COMMENTED. This offset was a bare `3` written out at NINE call
    sites in the shell and once in `fs`. Growing the header by four bytes meant changing it in ten
    places, and the ninth and tenth were found by a test failing, not by a checker. Both sides name
    the constant now, and this compares them so the next change to the wire format cannot be applied
    to only one half of it.
    """
    problems = []
    fs_hdr    = const("services/fs/src/main.rs", "DIR_HDR")
    shell_hdr = const("services/shell/src/main.rs", "DIR_HDR")
    if fs_hdr is None or shell_hdr is None:
        problems.append("wire format: a constant is missing - fs DIR_HDR=%s, shell DIR_HDR=%s "
                        "(renamed or deleted? this check cannot pass vacuously)"
                        % (fs_hdr, shell_hdr))
        return problems
    if fs_hdr != shell_hdr:
        problems.append(
            "wire format: LIST_DIR header size disagrees across the crates - fs DIR_HDR=%d writes "
            "entries at that offset, shell DIR_HDR=%d reads them from there. Every listing would "
            "parse a name length out of the wrong byte. Change both, or neither."
            % (fs_hdr, shell_hdr))
    return problems


def help_philosophy_problems():
    """`help`'s Philosophy section must name all six kernel responsibilities.

    It restates MISCIS, which is `CLAUDE.md` §4.3 - and a restatement is a copy that rots. This
    derives the six from the CONSTITUTION (the same list `scripts/commandments.py` parses, so there
    is one source) and fails if the help text stops naming one.

    It is what makes the copy affordable. The alternative was to leave the architecture out of
    `help` entirely, or to draw it by hand and hope - and nine commands had just fallen out of this
    very table with nothing watching, which is not a hope worth repeating.
    """
    import re
    problems = []
    claude = read("CLAUDE.md")
    # SLICED EXACTLY AS `commandments.py` SLICES IT, because there must be one way to read this list.
    # The first version here used its own regex and the `- **MISCIS**` suffix on the heading broke it
    # immediately: two parsers for one fact is the same mistake as two copies of the fact.
    try:
        scope = claude[claude.index("### 4.3 Kernel Scope"):claude.index("### 4.4 Kernel Anti-Scope")]
    except ValueError:
        problems.append("help philosophy: cannot find CLAUDE.md 4.3 - the six responsibilities "
                        "cannot be read from the constitution, and an unverifiable law is a "
                        "failure, never a pass")
        return problems
    wanted = []
    for ln in scope.split("\n"):
        ln = ln.strip()
        if ln.startswith("- "):
            wanted.append(ln[2:].split("(")[0].strip().lower().split()[0])
    src = read("services/shell/src/main.rs")
    # The MANUAL, not `help` - they were split so each has one job (see `DOCS` in the shell).
    # THE MANUAL, not `help`. They were split so each has one job: `help` is a reference consulted
    # mid-task, `docs` is a document read once, and a philosophy section inside `help` made `help`
    # open on prose when somebody wanted a command word.
    ph = re.search(r"static DOCS: &\[HelpRow\] = &\[(.*?)\n\];", src, re.S)
    if not ph:
        problems.append("help philosophy: no DOCS document in the shell (renamed? this check "
                        "cannot pass vacuously)")
        return problems
    text = ph.group(1).lower()
    missing = [w for w in wanted if w not in text]
    if missing:
        problems.append(
            "help philosophy: the section does not name %d of the six kernel responsibilities "
            "from CLAUDE.md 4.3 - %s. MISCIS is the whole of what the kernel does; a help text "
            "that drops one teaches a different system."
            % (len(missing), ", ".join(missing)))
    return problems


def help_coverage_problems():
    """Every utility the shell answers must appear somewhere in `help`.

    A third CODE-versus-CODE check, and the one with the most obvious failure mode: `help` is the
    front door, it is edited by hand, and nothing compared it to the command set. Nine utilities had
    fallen out of it without a single check noticing - `avg`, `churn`, `from`, `input`, `max`,
    `min`, `seal`, `sock`, `sum` - including two whole features with their own specs.

    A command the shell accepts and `help` never mentions is not a small gap: `help` is how somebody
    finds out the command exists at all, so an omission makes the feature effectively unshipped for
    anyone who did not watch it being built.

    Deliberately loose about WHERE: a mention anywhere in the help text counts, because several
    utilities are documented in a piped form (`status | where mem>0`) rather than on a row of their
    own, and demanding a particular shape would make this fight the author instead of helping them.
    """
    import re
    problems = []
    src = read("services/shell/src/main.rs")
    m = re.search(r"static HELP:[^=]*=\s*&\[(.*?)\n\];", src, re.S)
    u = re.search(r"const UTILS: &\[&str\] = &\[(.*?)\n\];", src, re.S)
    if not m or not u:
        problems.append("help coverage: could not find HELP or UTILS in services/shell/src/main.rs "
                        "(renamed? this check cannot pass vacuously)")
        return problems
    help_txt, utils = m.group(1), sorted(set(re.findall(r'"([a-z0-9_-]+)"', u.group(1))))
    missing = [x for x in utils if not re.search(r"\b" + re.escape(x) + r"\b", help_txt)]
    if missing:
        problems.append(
            "help coverage: %d utilit(y/ies) the shell answers are mentioned NOWHERE in `help` - %s. "
            "`help` is how somebody learns the command exists; an omission ships the feature to "
            "nobody who was not watching it being built."
            % (len(missing), ", ".join(missing)))
    return problems


def util_help_coverage_problems():
    """Every utility in UTILS must have a `util_help` BLOCK, not just a mention in `help`.

    `help_coverage_problems` above asks whether a utility is FINDABLE - named somewhere in the
    `help` list. This asks the other half of conventions rule 1: does `<util> help` actually print
    anything. They are different failures and the second is worse, because the command answers.

    `docs` shipped failing it. It was added to UTILS in the change that added `events` and `trace`,
    on the note that those two "already HAD help blocks" - `docs` did not, so `docs help` matched
    the intercept, fell through `util_help`'s `_ => return false`, printed NOTHING, and returned
    Ok. A command that answers a question with a blank line and success is worse than one that
    does not know the word.
    """
    import re
    problems = []
    src = read("services/shell/src/main.rs")
    u = re.search(r"const UTILS: &\[&str\] = &\[(.*?)\n\];", src, re.S)
    h = re.search(r"fn util_help\(.*?\n\}\n", src, re.S)
    if not u or not h:
        problems.append("util help coverage: could not find UTILS or fn util_help in "
                        "services/shell/src/main.rs (renamed? this check cannot pass vacuously)")
        return problems
    utils = sorted(set(re.findall(r'"([a-z0-9_-]+)"', u.group(1))))
    # An arm is `"name" =>` or a grouped `c @ ("sum" | "min" | ...) =>`; either way the name is a
    # quoted word inside the match, so collecting every quoted word is enough to answer "is there
    # an arm for this utility" without parsing Rust.
    armed = set(re.findall(r'"([a-z0-9_-]+)"', h.group(0)))
    missing = [x for x in utils if x not in armed]
    if missing:
        problems.append(
            "util help coverage: %d utilit(y/ies) in UTILS have no `util_help` block - %s. "
            "`<util> help` matches the intercept, finds no arm, and prints NOTHING while "
            "reporting success (conventions rule 1)."
            % (len(missing), ", ".join(missing)))
    return problems


def budget_ordering_problems():
    """net-stack must answer a request BEFORE its client stops waiting.

    Not a doc-versus-code check like the rest of this file - a CODE-versus-CODE one, across a crate
    boundary, which is why it lives in a function rather than the facts table.

    THE RULE. When a service can spend longer on a request than its caller will wait, the caller
    always gives up first and the answer lands afterwards as a STALE reply. That does not merely waste
    the work: the next request receives the previous one's answer, the correlation tag rejects it, and
    the reply stream never catches up. One slow lookup desyncs the channel indefinitely.

    WHY IT IS CHECKED RATHER THAN COMMENTED. It was a comment. net-stack bounded DNS by a COUNT
    (`DNS_RX_TRIES` polls of `DANCE_SECS` each = 24 s) while the shell waited 8 - and nothing compared
    them, because they are in different crates and neither number looks wrong on its own. A Dell Wyse
    found it (2026-09-17): every `net resolve` timed out, and the log showed the shell discarding a
    reply for tag 4 while awaiting tag 6.

    The same ordering `backlog/28` established for the stash budgets, extended across the wire.
    """
    problems = []
    client = const("services/shell/src/main.rs", "NET_RESOLVE_SECS")
    floor  = const("services/net-stack/src/main.rs", "CLIENT_MIN_DEADLINE_SECS")
    budget = const("services/net-stack/src/main.rs", "DNS_BUDGET_SECS")
    if client is None or floor is None or budget is None:
        problems.append("budget ordering: a constant is missing - shell NET_RESOLVE_SECS=%s, "
                        "net-stack CLIENT_MIN_DEADLINE_SECS=%s DNS_BUDGET_SECS=%s "
                        "(renamed or deleted? this check cannot pass vacuously)"
                        % (client, floor, budget))
        return problems
    if floor != client:
        problems.append(
            "budget ordering: net-stack believes its shortest client deadline is %ds, but the shell's "
            "`net resolve` waits %ds. net-stack sizes its DNS budget against that belief, so the two "
            "must agree." % (floor, client))
    if budget >= client:
        problems.append(
            "budget ordering: net-stack may spend %ds on a DNS resolve while its client waits only "
            "%ds. The client will give up first and the late answer will desync the reply stream - "
            "see backlog/28 and the Wyse capture of 2026-09-17." % (budget, client))
    return problems


def facts():
    out = []

    # The SEAM's own size, imported from the checker that DISCOVERS it. `docs/porting.md` tells a
    # porter how many members they owe, and that is precisely a number that moves every time one is
    # added - which this branch did seven times in a day.
    try:
        sys.path.insert(0, os.path.join(ROOT, "scripts"))
        import arch_seam_check
        # `wanted()` returns (top-level names, {module: names}); the seam SIZE is both, which is
        # what the checker's own "all N members" line counts. `len()` of the pair is 2 - a reading
        # that looked plausible and was wrong, caught because this fact was checked rather than
        # written down.
        _top, _moded = arch_seam_check.wanted()
        seam = len(_top) + sum(len(v) for v in _moded.values())
    except Exception:
        seam = 0
    if seam:
        out.append(("arch::imp seam members", seam,
                    "scripts/arch_seam_check.py wanted() - discovered from neutral-kernel usage",
                    [r"\*\*([0-9]+) members\*\* that the neutral kernel calls",
                     r"every one of the ([0-9]+) `arch::imp` members"]))

    ss_total, ss_kern = shared_surface()
    if ss_total:
        out.append(("shared surface outside arch/", ss_total,
                    "SHARED-SURFACE.baseline.txt (scripts/shared_surface_check.py)",
                    [r"([0-9]+) arch-conditional sites outside",
                     # `| arch-conditional sites outside `arch/` | **143** | **46** |` - the AFTER
                     # column. A release note states its numbers as a table, not as a sentence.
                     r"arch-conditional sites outside[^|]*\|[^|]*\|\s*\*\*([0-9]+)\*\*"]))
        out.append(("shared surface, neutral kernel", ss_kern,
                    "SHARED-SURFACE.baseline.txt (scripts/shared_surface_check.py)",
                    [r"([0-9]+) in the neutral kernel",
                     r"in the NEUTRAL KERNEL[^|]*\|[^|]*\|\s*\*\*([0-9]+)\*\*"]))

    # HOW MANY FILES ARE IN A DIRECTORY, because §5's map of the repository states three such
    # counts and all three had rotted: utilities said 48 and held 52, examples said 12 and held 14,
    # docs said "~30" and held 41.
    #
    # This is the failure §4.1's own amendment names - "a hand count is right on the day it is taken
    # and silently wrong afterwards" - committed by the document that names it, in the section right
    # after. That amendment installed a ratchet for arch-conditional sites; the counts one paragraph
    # further down had no gate at all, which is why they drifted for months without failing anything.
    #
    # A count nobody can be bothered to re-take is a count that should be derived. These are.
    util = count_files("utilities", "*.md") - 1          # 0_conventions.md is the rules, not a utility
    if util > 0:
        out.append(("utility specs", util, "utilities/*.md minus 0_conventions.md",
                    [r"one file each \(([0-9]+) \+ 0_conventions\)"]))
    # Dot-directories are configuration, not examples: this counted `.cargo` on its first run and
    # reported the doc wrong when the doc was right. An instrument that miscounts is worse than none,
    # because the first thing it does is send you to "fix" something correct.
    ex = len([d for d in os.listdir(os.path.join(ROOT, "examples"))
              if not d.startswith(".") and os.path.isdir(os.path.join(ROOT, "examples", d))])
    if ex > 0:
        out.append(("worked examples", ex, "directories under examples/",
                    [r"# ([0-9]+) worked services"]))
    dcount = count_files("docs", "*.md")
    if dcount > 0:
        out.append(("design notes", dcount, "docs/*.md",
                    [r"\(([0-9]+) files; the index lists them all\)"]))

    qd = const("kernel/src/ipc/queue.rs", "QUEUE_DEPTH")
    if qd:
        out.append(("IPC queue depth", qd, "kernel/src/ipc/queue.rs QUEUE_DEPTH",
                    [r"([0-9]+)-deep queue", r"queue depth (?:of|is|=)\s+([0-9]+)",
                     r"([0-9]+) messages per endpoint", r"queue depth, 0-([0-9]+)"]))

    mm = const("kernel/src/ipc/message.rs", "MAX_MESSAGE_SIZE")
    mp = const("sdk/rust/src/ipc.rs", "MAX_PAYLOAD")
    if mm and mp and mm != mp:
        out.append(("SDK MAX_PAYLOAD vs kernel MAX_MESSAGE_SIZE", mm,
                    "these two MUST be equal - a smaller SDK buffer is a stack smash", []))

    ring = const("services/events/src/main.rs", "RING")
    if ring:
        out.append(("events trace ring", ring, "services/events/src/main.rs RING",
                    [r"([0-9]+)-event (?:trace )?ring", r"ring of ([0-9]+) events"]))

    logb = const("services/events/src/main.rs", "LOG_BYTES")
    if logb:
        out.append(("events log window (KiB)", logb // 1024, "services/events/src/main.rs LOG_BYTES",
                    [r"([0-9]+) KiB log window"]))

    pieces = const("services/recorder/src/main.rs", "PIECES")
    if pieces:
        out.append(("recorder rotation pieces", pieces, "services/recorder/src/main.rs PIECES",
                    [r"`?PIECES`? files rotate \(([0-9]+) today", r"rotates? between ([0-9]+) pieces"]))

    for nm in ("Log", "Call", "CallDeadline", "AcquireSendCap", "InspectKernel", "Kill"):
        v = enum_value("kernel/src/syscall/dispatch.rs", nm)
        if v:
            out.append(("syscall %s" % nm, v, "kernel/src/syscall/dispatch.rs enum",
                        [r"syscall\s+([0-9]+)\s*[-(]?\s*%s" % nm, r"%s\s*\(syscall\s+([0-9]+)\)" % nm]))

    # The TCP connection table. Added 2026-09-15 because this one had ALREADY drifted: `MAX_CONNS`
    # was cut from 4 to 2 when net-stack's `service_main` frame reached 37% of the arm32 user stack,
    # and `utilities/48_tcp.md` went on saying "at most four connections exist at once" until a
    # reader happened to check it against the source. A number a doc restates is exactly what this
    # script is for, so it is pinned rather than merely corrected.
    mc = const("services/net-stack/src/tcp.rs", "MAX_CONNS")
    if mc:
        out.append(("TCP connection table", mc, "services/net-stack/src/tcp.rs MAX_CONNS",
                    [r"at most \*{0,2}([0-9]+) connections",
                     r"([0-9]+) simultaneous TCP connections"]))

    n_util = count_files("utilities", "[0-9]*.md")
    out.append(("utility specs", n_util, "utilities/*.md on disk",
                [r"([0-9]+) utility specs", r"([0-9]+) utilities\b"]))

    # HOW MANY SUITES `fs-all` RUNS, and how many operations `fs-tear` cuts. Both are counts a
    # reader is told in prose and neither had a gate, so both were wrong when this was written:
    # the storage sweep was documented as 16, 19 and 25 suites in three different files while the
    # list held 30, and `fs-tear` announced "3 operations, 54 tear points" twice on the same page
    # as the table that lists four of them.
    #
    # This is the same failure the utility/example/doc counts above were added for, and it is worth
    # being blunt about why it keeps happening HERE: the storage suites grow one at a time, every
    # addition is correct in isolation, and the number that goes stale is in a different file from
    # the change that invalidates it. Nobody is ever wrong on the day; the count is just never
    # re-taken. So derive it.
    #
    # The tear POINT count (75) is deliberately not gated: it is discovered at runtime by recording
    # what the driver actually writes, so no source owns it and a gate here would be a number
    # checking itself.
    m = re.search(r"const\s+SUITES\s*:\s*&\[&str\]\s*=\s*&\[(.*?)\];",
                  read("osdev/src/main.rs"), re.S)
    if m:
        # STRIP THE COMMENTS FIRST. That list is more comment than data - each entry says why it is
        # there - and one of those comments contains the phrase "every fs suite" in quotes, which a
        # naive count of quoted strings reads as a 31st suite. Caught on this fact's first run,
        # reporting a correct document wrong, which is the exact trap the examples count above
        # records hitting with `.cargo`.
        body = re.sub(r"//[^\n]*", "", m.group(1))
        n_suites = len(re.findall(r'"[^"]+"', body))
        if n_suites:
            out.append(("fs-all suites", n_suites, "osdev/src/main.rs SUITES",
                        [r"`osdev test fs-all`[^|]*?\(([0-9]+) suites",
                         r"runs \*{0,2}([0-9]+) suites",
                         r"runs ([0-9]+) suites",
                         r"#\s*([0-9]+) suites,",
                         r"fs-all: ([0-9]+) suites"]))

    # `struct TearCase {` is the DECLARATION, not a case, and counting it made four operations
    # read as five. Only a construction inside the table counts, so the declaration is excluded
    # rather than the pattern being loosened.
    n_tear = len(re.findall(r"(?<!struct )\bTearCase\s*\{", read("osdev/src/shell_test.rs")))
    if n_tear:
        out.append(("fs-tear operations", n_tear, "osdev/src/shell_test.rs TearCase entries",
                    [r"([0-9]+) operations, [0-9]+ tear points"]))

    return [f for f in out if f[3]]


# Present-tense docs only. `audits/` and `milestones/` record what WAS true and are exempt, as are
# CLAUDE.md's dated amendment blocks (handled by the historical-line filter below).
DOCS = ["CLAUDE.md", "README.md", "CONTRIBUTING.md", "GETTING_STARTED.md"]
DOC_GLOBS = ["docs/*.md", "utilities/*.md", "services/*/CLAUDE.md", "kernel/src/**/CLAUDE.md",
             "sdk/rust/CLAUDE.md", "osdev/CLAUDE.md", "tests/**/CLAUDE.md", "backlog/*.md",
             # The PUBLISHED site. Most of its pages are `{{#include}}` views of the files above and
             # cannot drift by construction - but four are written for the site and have no source to
             # be a view OF, so they are exactly where a restated number goes stale unwatched.
             "website/src/*.md"]

# NOT SCANNED: `milestones/RELEASE-*.md`. Those files existed briefly as prepared tag messages and
# were removed at v0.17.0 - release notes are written on the GitHub release now, and the tag body is
# the immutable copy. A glob aimed at a convention the project has dropped is a check that can only
# ever report a pass it did not earn, so it goes with the convention (26.2).

# A SECTION REFERENCE IS NOT A VALUE. On this script's first run `queue depth (§8.5)` captured "8"
# and a `0-16` range captured "0" - two false alarms out of two findings. A checker that cries wolf
# gets disabled, which is worse than never having written it, so references are stripped before any
# pattern sees the line rather than being fought inside each pattern.
SECTION_REF = re.compile(r"\(?§\s*[0-9]+(\.[0-9]+)*\)?|\bsection\s+[0-9]+(\.[0-9]+)*", re.I)

# A line that reports a past run or names a date is evidence, not a claim about now.
HISTORICAL = re.compile(r"20[0-9]{2}-[0-9]{2}-[0-9]{2}|Verified:|verified on|Amendment|was\b.*\bnow\b")

# ...EXCEPT a line that declares the CURRENT STATE and happens to date it. That is not evidence about
# the past, it is a claim about now wearing a date, and it is the single construct most likely to go
# stale - which made it the worst possible thing to exempt.
#
# Found by drift this gate was written to catch and did not: `docs/gsfs-carnage.md` opened with
#
#     **Status, as of 2026-09-21.** `osdev test fs-all` runs **30 suites** and all 30 pass together
#
# while SUITES held 33. The suite-count fact has a pattern that matches `runs **30 suites**`
# perfectly; it never ran, because the date earlier in the same line sent the whole line to
# HISTORICAL. A checker that is silent because it declined to look is worse than no checker: it is
# the reassurance without the check, which is what `backlog/32` exists to prevent.
#
# So a status line is read like any other. The exemption keeps its real job - amendments, dated
# verification notes, "was X, now Y" - and loses the one case where a date meant the opposite of
# historical.
STATUS_CLAIM = re.compile(r"\*\*\s*Status\b|^\s*Status[ ,:]|\bStatus,? as of\b", re.I)


def main():
    import glob
    docs = [d for d in DOCS]
    for g in DOC_GLOBS:
        docs += [os.path.relpath(p, ROOT).replace(os.sep, "/")
                 for p in glob.glob(os.path.join(ROOT, g), recursive=True)]

    checked = 0
    bad = []

    tree = (porting_tree_problems() + budget_ordering_problems() + wire_format_problems()
            + help_coverage_problems() + util_help_coverage_problems()
            + help_philosophy_problems())
    for name, truth, source, pats in facts():
        if not pats:
            bad.append((name, source, "", "", ""))
            continue
        for rel in sorted(set(docs)):
            text = read(rel)
            if not text:
                continue
            for line_no, line in enumerate(text.split("\n"), 1):
                if HISTORICAL.search(line) and not STATUS_CLAIM.search(line):
                    continue
                line = SECTION_REF.sub(" ", line)
                for pat in pats:
                    for m in re.finditer(pat, line, re.I):
                        checked += 1
                        got = int(m.group(1))
                        if got != truth:
                            bad.append((name, source, rel, line_no, "says %d, code says %d" % (got, truth)))

    if tree:
        # NOT a porting.md header. This list collects six unrelated code-versus-code checks, and
        # printing one check's name over another's failure sends the reader to the wrong file -
        # `util help coverage` arrived under "docs/porting.md's TREE disagrees", which is a checker
        # lying about what it found.
        print("code-versus-code checks disagree:")
        print()
        for t in tree:
            print("  %s" % t)
        print()
        print("SHARED-SURFACE.baseline.txt is the only source for those counts. A tree of per-file")
        print("numbers rots exactly like the hand count CLAUDE.md 4.1 used to carry, so it is checked.")
        return 1

    if not bad:
        print("facts: %d doc statement(s) agree with the code they restate, and the porting tree "
              "matches the ratchet" % checked)
        return 0

    print("facts: %d doc statement(s) disagree with the code\n" % len(bad))
    for name, source, rel, line_no, what in bad:
        print("  %s" % name)
        print("      source of truth: %s" % source)
        if rel:
            print("      %s:%s  %s" % (rel, line_no, what))
    print("\nThe code owns the number. Update the doc, or - better - delete the number from the\n"
          "doc if the sentence reads fine without it. A fact restated in two places drifts.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
