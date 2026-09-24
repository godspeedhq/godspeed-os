# GodspeedOS self-check, part 4 of 9: the events metric table and the observability reader
#
# One of the parts `selfcheck` runs, in the order they are listed in `SELFCHECK_PARTS`
# (`services/shell/src/main.rs`). `selfcheck events` runs THIS part alone; `selfcheck` runs them all
# and prints one tally for the whole suite.
#
# They are separate files because a baked script is indexed with u16 offsets and the single file had
# reached 65,139 bytes of a 65,536 ceiling (`backlog/47`). Each part now carries its own budget - and
# its own 256-statement detail budget, so the report no longer stops naming statements part way.
#
# No part may declare a `fn` or a `let` another part uses: each is interpreted on its own, with a
# fresh variable table. What DOES carry across is the working directory and the disk.

# ===== events: the METRIC table, in the same service that holds the ring =====
# The half of `events` that is not the trace ring: it holds published samples, and it holds no log
# lines at all - `ctx.log()` is syscall 5, straight to the kernel ring and serial (CLAUDE.md 11.4).
assert ok events metrics
# THE SINK PUBLISHES ITS OWN NUMBERS BY LOCAL WRITE, NEVER BY SENDING ITSELF A MESSAGE. A send is
# itself a reportable event, so a self-emit over IPC would feed the ring from the ring and fill it with
# its own reporting. These rows existing is that local-write path working, and it is the executable
# form of the rule in docs/observability.md 9.
events metrics | assert contains ring.recorded
events metrics | assert contains metrics.held
# ...and an ORDINARY service publishes over IPC, which is the path any new service would use. That
# assertion is made LATER, after the file sections, and the move is the whole point: `fs` publishes
# every 32 requests and this section runs BEFORE any file work, so at this line `fs` may not have
# served 32 requests since the last sink restart - and after a chaos storm it has not. The comment
# here used to claim "`fs` has served this entire suite", which is simply false this early in the file.
# EVERY internal service is registered, not just the two that started with the cap. `msgs.received` is
# counted in the SDK's receive paths, so a service gets it by existing rather than by remembering to
# add it - which is the same reason trace emission lives there. It publishes on the FIRST message as
# well as every 64th: a service under the interval would otherwise have NO ROW, and no row is
# indistinguishable from dead, which is the one question this metric exists to answer.
events metrics | assert contains msgs.received
# Named services, on every port: the terminal, storage, and the clock. Attribution is the point - an
# undeclared service publishes under a BLANK owner, and since the key is (owner, name) every such
# service collides into ONE row with the counters interleaving. Caught exactly that way: a single
# `msgs.received 1920` belonging to nobody, which was `console` plus nine others.
# BUSY services only, and that is not laziness. The table is VOLATILE: when chaos kills `events` its
# rows go with it, and a service republishes only when it next RECEIVES something. A quiet service like
# `time` is therefore legitimately absent for a while after a sink restart - which is the design, not a
# fault, and it failed here after 61 restarts in a chaos run. Assert on services that are certainly
# doing work while the suite runs.
events metrics | where owner contains console | assert contains msgs.received
# It is a record source like `events ipc`, so it filters like one.
events metrics | where owner contains events | assert contains ring.recorded
# An unknown view is refused loudly here too.
assert fails events metricz

# ===== events: the observability reader =====
assert ok events status
assert ok events metrics
# PIPED, NOT BARE. `events ipc` unpiped is the INTERACTIVE PAGED view and waits for a keypress - the
# shell test drives it by sending `q`. A script has no one to press it, so a bare `assert ok events ipc`
# hangs the whole suite until the harness times out, which is exactly what it did.
events ipc | assert contains outcome
# A real record source, not just a printer: it filters like every other view.
events metrics | assert contains msgs.received
events metrics | where owner contains events | assert contains ring.recorded
# THE LOG. A queryable copy of what services printed - never the authoritative record, which went to
# serial and the kernel ring by syscall before this service saw it. That ordering is the whole design:
# a dead `events` loses scrollback and no log output.
# BOUNDED: a screenful, not the whole window. The default used to be everything the sink held
# - about 3 KB on a booted machine - which is more than anyone reads and enough console
# traffic to slow a capture harness. `events log <n>` asks for more.
assert ok events log 5
# EVERY STREAM THE SINK SERVES IS RECORDS, not free text. That is the rule, and the log was the one
# view breaking it: it printed lines, so filtering it needed a bespoke per-service argument in the
# shell - duplicated machinery that `where` already provides, and wrong on its first outing. As
# records the answer is the same `where` every other view uses, and `to json` / `to yaml` come free.
# Asserted on the COLUMN rather than on any service's line: which services logged recently varies by
# machine and by how far the 8 KiB window has wrapped, but the shape never does.
events log | assert contains owner
events log | to json | assert contains owner
events log | to yaml | assert contains owner
# PERSISTING TO DISK NEEDS NO NEW MECHANISM, and this is where that is proved. `events` must never
# gain an `fs` peer (docs/logging.md: a service that reports a storage failure must not be downstream
# of storage), so the drain happens on the READER side: `events` is a record source, `write` is a
# generic pipe sink, and the shell already holds both caps. The dependency points the right way -
# the drainer needs `events` and `fs`; neither of them needs the drainer.
# GUARDED, like the file section below. The disk PERSISTS ACROSS BOOTS, so `/sc` is usually already
# there on hardware and a bare `mkdir` fails - which is the single failure this suite reported on an
# otherwise clean Pi 4 run. QEMU never showed it, because its test disk is formatted fresh every time:
# a suite that is only ever run against a new disk cannot see the state a real machine keeps.
if dir /sc { delete /sc recursive }
mkdir /sc
events log | write /sc/evt.log
read /sc/evt.log | assert contains owner
events log | where owner=block-driver | write append /sc/evt.log
assert ok read /sc/evt.log
