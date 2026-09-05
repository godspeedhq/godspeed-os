# GodspeedOS extensive self-check suite.
# Run it with:  gsh> selfcheck      (runs this embedded suite IN MEMORY - no disk write,
# so it is not capped by the on-disk file size; it just needs a flashed GSFS drive for
# the file-command tests). Passes iff the summary says "failed 0".
#
# Covers every shell utility's main functions + negative cases, EXCEPT:
#   - observe : the live full-screen view. `observe now` IS covered below, but only in its PIPED
#               form - see that section for what that does and does not reach.
#   - drives  : flashing/relabel/reset touch disks and prompt y/N - not scriptable.
# Re-runnable: everything is created under /sc, removed at the START of the run, and deleted again at
# the end. Cleaning only at the end is not re-runnable - it assumes the previous run REACHED its end.
# A run that was aborted, wedged, or killed leaves /sc populated, and the next run then fails setup
# assertions that only hold on a clean tree ("assert fails mkdir /sc", "assert fails read /sc/b.txt"),
# reporting phantom failures that look like product bugs. Observed three times before it was fixed.
#
# Rules this suite obeys (so it self-grades correctly):
#   - `assert ok|fails|fails-with <cmd>` is the RESULT form - only for NON-piped commands
#     (a line with '|' is a pipeline; the trailing `assert` is its sink instead).
#   - `<producer> | … | assert contains|lacks|empty <text>` is the CONTENT form.
#   - match/count/first/last are byte filters; where/select/sort/to/from work on records.
#   - exhaustive operator coverage runs on FREE producers (status, ls, json) to avoid
#     spawning a service per line; roster/greet/upper lines are kept lean.

# ##########################################################################
# #  gsh LANGUAGE TOUR                                                      #
# #  A guided, self-checking demo of EVERY gsh feature (Tier 1 + Tier 2).   #
# #  Each step asserts its own result or feeds a later assert, so the whole #
# #  tour must finish "failed 0". Each section ECHOES a banner first, so the #
# #  live transcript reads as a labelled, spaced-out walkthrough. `import`   #
# #  is a `run <file>`-time feature, shown (not run) near the bottom.       #
# ##########################################################################

# WHICH BUILD IS THIS? First line of the run, before any test, because a serial log that cannot
# identify its own image is worth less than it looks. A clean run proves nothing if nobody can say
# WHICH build produced it - and a diagnostic that only speaks on FAILURE cannot prove it was even
# present. That happened: a NIC fault was instrumented, the next hardware run came back clean, and
# the log was equally consistent with "the fix shipped and the bug is rare" and "the old image was
# still on the stick". `version` prints the git SHA stamped in at build time, so every log from here
# answers that in its own first line.
version
echo ''
echo '#################### gsh LANGUAGE TOUR ####################'
if ls /tour { delete /tour recursive }    # an aborted run leaves it behind; mkdir would then fail
mkdir /tour                              # a scratch directory for the tour's files

echo ''
echo '===== 1. VARIABLES - let (immutable), let mut (mutable), expansion ====='
#  `let` binds an IMMUTABLE variable; `let mut` a mutable one (reassign with `name = ...`).
#  "..." interpolates $vars; '...' is raw.
let name = Ada                           # immutable binding
let mut hits = 0                         # mutable counter (bumped in a loop below)
echo "hello, $name" | assert contains hello, Ada     # double quotes interpolate
echo 'raw text - $name stays literal'                # single quotes: no expansion

echo ''
echo '===== 2. ARITHMETIC - inline + - * / % with ( ) and precedence ====='
let total = 2 + 3 * 4                    # * binds tighter than + -> 14
echo $total | assert contains 14
let grouped = ( 2 + 3 ) * 4              # parentheses override precedence -> 20
echo $grouped | assert contains 20

echo ''
echo '===== 3. RESULT + IF / ELSE, comparisons, in ====='
#  Every command yields Ok/Err; `result` is the previous one's outcome.
write /tour/a.txt hi                     # a real command...
if result == Ok { echo wrote-ok | assert contains wrote-ok }   # ...check its result
if $total > 10 { echo big | assert contains big } else { fail "math broke" }
if $name in Ada Bob Cy { echo known-name | assert contains known-name }
# an else-if chain: the first true branch wins
if $total < 0 { fail "else-if: A wrong" } else if $total > 10 { echo elif-ok | assert contains elif-ok } else { fail "else-if: C wrong" }

echo ''
echo '===== 4. SWITCH - several values per arm, _ default, and `switch result` ====='
switch $name {
    Bob Cy   { fail "wrong arm" }        # an arm may list multiple values
    Ada      { echo matched-ada | assert contains matched-ada }
    _        { fail "default must not run" }
}
echo probe-for-switch-result             # a command -> result = Ok
switch result {                          # `switch result` matches the previous result's KIND
    Ok  { echo swr-ok | assert contains swr-ok }
    _   { fail "switch result: Ok not matched" }
}

echo ''
echo '===== 5. CAPTURE - $( ) puts a producer OR a function ($(fn)) output into a variable ====='
let phrase = $(echo hi there)            # -> "hi there"
echo got:$phrase | assert contains got:hi
fn greeting who { echo hello-$who }      # $(fn): capture a FUNCTION's output (bounded 4 KiB, no heap)
let g = $(greeting Ada)
echo capfn:$g | assert contains capfn:hello-Ada

echo ''
echo '===== 6. FOR LOOPS - words, range, mutable accumulation, and lines of a producer ====='
for fruit in apple pear plum {           # iterate a literal word list
    echo fruit-$fruit
}
for i in range 3 {                       # range N -> 0 1 2
    echo idx-$i
}
for i in range 1 5 {                     # range A B -> 1 2 3 4
    hits = $hits + 1                     # reassigned each pass: a fixed slot, no arena growth
}
echo hits-$hits | assert contains hits-4
let mut nlines = 0
# for line in (producer): iterate a producer's output lines.
#
# GUARDED ON THE WRITE, because `fail` STOPS THE RUN and this line needs storage. On a machine with
# no disk (a Pi 2 with no USB stick) the write failed, the loop saw nothing, and the whole suite
# aborted HERE - at section 6 of 31, reporting "ran 48, failed 4" as though that were the suite.
# Twenty-five sections that have nothing to do with storage never executed, and the count read like a
# small run rather than a truncated one.
#
# The guard is on the WRITE rather than on `nlines`, deliberately: skipping whenever the loop came
# back empty would also swallow a genuinely broken `for line` on a machine that HAS a disk, which is
# the bug this test exists to catch. Write succeeded -> run the real test, `fail` included.
if write /sc_fl.txt oneline {
    for line in (read /sc_fl.txt) { nlines = $nlines + 1 }
    if $nlines > 0 { echo forline-ok | assert contains forline-ok } else { fail "for line: empty" }
} else {
    echo 'SKIP for-line: no writable storage on this machine'
}
delete /sc_fl.txt

echo ''
echo '===== 7. UNBOUNDED loop + break / continue ====='
let mut k = 0
loop {                                   # runs until `break` (100k-iteration backstop)
    k = $k + 1
    if $k == 2 { continue }              # skip the rest of THIS pass
    if $k > 4  { break }                 # leave the loop entirely
    echo pass-$k                         # prints pass-1, pass-3, pass-4
}

echo ''
echo '===== 8. FUNCTIONS - named params, return, recursion, and as an `if` condition ====='
fn sayhi who {                           # `who` is a parameter (named, positional)
    echo "hi, $who"                      # a function sees its params + immutable globals
}
sayhi $name                              # call it like a command -> "hi, Ada"
if result == Ok { echo sayhi-ok | assert contains sayhi-ok }   # a function's result is checkable
fn clamp n {                             # `return` ends a function early
    if $n > 100 { echo clamped ; return }
    echo n-is-$n
}
clamp 50                                 # -> n-is-50
clamp 250                                # -> clamped (early return; "n-is-250" never prints)
fn countdown n {                         # recursion via an explicit call stack (no native recursion)
    if $n <= 0 { echo liftoff } else { echo t-$n ; let m = $n - 1 ; countdown $m }
}
countdown 3                              # -> t-3, t-2, t-1, liftoff
fn is_ready { echo yes }                 # a function used AS an `if` condition: if myfn (branch on result)
if is_ready { echo iffn-then | assert contains iffn-then } else { fail "if myfn: Ok must take then" }
if !is_ready { fail "if !myfn must take else" } else { echo iffn-neg | assert contains iffn-neg }
# audit U1: multi-`!` negation is ITERATIVE (no native recursion / stack overflow). Parity must hold.
if !!5 > 3 { echo neg-even | assert contains neg-even } else { fail "!! should be identity (true)" }
if !!!5 > 3 { fail "!!! of true should be false" } else { echo neg-odd | assert contains neg-odd }
# audit U2: reserved parameter words cannot be bound - as a let, a loop var, or a fn param.
assert fails let self = x
assert fails let args = x
assert fails let arg1 = x

echo ''
echo '===== 9. DEFER - cleanup on scope exit, LIFO, even on fail ====='
fn build_thing {
    mkdir /tour/work
    defer delete /tour/work recursive    # runs when this function returns, however we leave it
    write /tour/work/out done
    read /tour/work/out | assert contains done
}                                        # <-- the deferred delete fires HERE, on return
build_thing
ls /tour | assert lacks work             # proof the defer ran: /tour/work is gone

echo ''
echo '===== 10. RECORD AGGREGATORS - count / sum / min / max / avg ====='
#  Pipes carry TYPED records, so a pipeline can REDUCE - impossible for byte pipes.
write /tour/inv.json '[{"item":"a","qty":10},{"item":"b","qty":20},{"item":"c","qty":30}]'
read /tour/inv.json | from json | count   | assert contains 3    # row count (dual: rows|lines)
read /tour/inv.json | from json | sum qty | assert contains 60   # 10 + 20 + 30
read /tour/inv.json | from json | min qty | assert contains 10
read /tour/inv.json | from json | max qty | assert contains 30
read /tour/inv.json | from json | avg qty | assert contains 20

echo ''
echo '===== IMPORT - shown, not run (libraries load at run <file> time) ====='
echo '  from /lib/assert.gsh import ok fails as denied   (selective, with as-rename)'
echo '  import /lib/math.gsh                             (all of a libs functions)'
#  Names collide loudly (resolve with `as`); the run's pre-scan then indexes them.
#  Exercised end-to-end by `osdev test files`.

echo ''
echo '===== tour cleanup - leave nothing behind ====='
delete /tour recursive
assert fails ls /tour                    # the tour dir is gone

echo ''
echo '#################### gsh LANGUAGE TOUR complete ####################'
echo ''

# ===== meta: the result model + the assert forms themselves =====
echo ''
echo '===== meta: the result model + the assert forms themselves ====='
assert ok echo hello
assert fails totallybogus
assert fails-with Unknown totallybogus
assert ok result
echo one two three | assert contains two
echo keep this | assert lacks secret
echo "spaced words stay" | assert contains spaced words stay
echo nothing | match zzz | assert empty

# ===== self-documentation: <util> help / <util> version =====
echo ''
echo '===== self-documentation: <util> help / <util> version ====='
assert ok help
assert ok status help
assert ok read help
assert ok assert help
assert ok mem help
assert ok ls help
assert ok run help
assert ok roster help
assert ok find version
assert ok read version
assert ok version help
assert ok version version
assert ok clear help

# ===== system info - now PIPE PRODUCERS (text emitters captured via Out), bare + piped =====
echo ''
echo '===== system info - now PIPE PRODUCERS (text emitters captured via Out), bare + piped ====='
assert ok about
assert ok version
assert ok cores
assert ok mem
assert ok date
assert ok date epoch
# wait: the q-abortable pacing pause (the library watch loop is built on it)
assert ok wait 1
assert fails wait
assert fails wait 0
assert fails wait 99999
assert ok wait help
assert ok wait version
# whatis: a name's kind + origin (the honest which - no $PATH here, so kind IS the answer)
assert ok whatis ls
whatis ls | assert contains built-in
whatis fs | assert contains service
whatis where | assert contains pipe
assert fails whatis banana
assert fails whatis
assert ok whatis help
about | assert contains GodspeedOS
version | assert contains GodspeedOS
cores | assert contains cores
mem | assert contains used
# The clock: PASS if it can be set, SKIP if this machine has no way to know the time.
#
# This board has no RTC, so `date` prints a time only once SNTP has set the clock. Three states,
# confirmed on hardware: boot WITH ethernet -> set at boot+5s, passes; unplug afterwards -> stays set,
# passes; boot WITHOUT ethernet -> never set, and the old assertion failed for a missing cable rather
# than for anything wrong with GodspeedOS.
#
# The probe READS, it does not repair. An earlier version ran `date sync` here, and the user was right
# to reject it: a check must not perform a network operation or set the machine's clock as a side
# effect. That is a repair wearing a probe's face, it costs time on every run, and a test that changes
# the system has stopped measuring it.
#
# `date epoch` yields 0 when the clock is unset, so iterating its output (`for line in (producer)`,
# section 6 above) gives a testable value without touching anything. `assert contains` cannot serve as
# the probe because it FAILS the suite rather than returning a boolean, and there is no bare
# `contains` - that dead end is what made an earlier attempt invent syntax.
#
# When the clock IS set the format is still asserted properly, so a broken `date` still fails. Only
# "this machine cannot know the time" is skipped, and it is skipped OUT LOUD, because a silent skip is
# a test that has quietly stopped testing.
#
# (An earlier version of this hung the whole suite: it used `$var = ...`, which is not an assignment,
# so a counter never incremented and a `wait 1` loop ran forever. Real grammar is in section 7 above:
# `let mut` to declare, `name = $name + 1` to assign. Test script changes on hardware before shipping.)
let mut clockset = 0
for line in (date epoch) { if $line > 0 { clockset = 1 } }
if $clockset > 0 {
    date | assert contains :
} else {
    echo 'SKIP  date - the clock is not set on this machine (no RTC, no network); not a failure'
}
help | assert contains status
# uptime - a record producer (wall-clock RTC delta): bare grid + json + column projection.
assert ok uptime
uptime | assert contains seconds
uptime | to json | assert contains seconds
uptime | select seconds | to json | assert lacks uptime

# ===== introspection producers: status / caps (+ every where operator, no spawn) =====
echo ''
echo '===== introspection producers: status / caps (+ every where operator, no spawn) ====='
assert ok status
status | assert contains shell
status | where name=shell | assert contains shell
status | where name!=shell | assert lacks shell
# These two exercise the NUMERIC where-operators (= and <), and they used the shell's core as a
# convenient value. That stopped being a fact: the shell moved to core 1 so the serial writer is not
# sharing a core with the microframe-timed USB driver, and both lines failed - a test asserting a
# placement DECISION while claiming to test an operator.
#
# The supervisor's core IS fixed, by the constitution rather than by choice: the kernel spawns it on
# core 0 and that is its one direct spawn (§11). So the operators are now tested against the one
# value in the system that cannot be re-placed.
status | where core=0 | assert contains supervisor
status | where state=Running | assert contains shell
status | where slot>=0 | assert contains shell
status | where core<1 | assert contains supervisor
status | where name contains super | assert contains supervisor
status | select name state | assert contains shell
status | sort name | assert contains supervisor
status | sort reverse slot | assert contains shell
assert ok caps
caps | assert contains introspect
caps shell | assert contains introspect
caps shell | where resource=spawn | assert contains spawn
caps shell | select resource | assert contains introspect
assert fails caps nosuchservice
assert fails-with FileNotFound caps nosuchservice

# ===== observe now: the metrics snapshot =====
echo ''
echo '===== observe now: the one-shot metrics frame (piped record form) ====='
# WHAT THIS REACHES, stated plainly because it is less than the command name suggests. `observe now`
# has TWO renderers, and piping picks the other one:
#   unpiped -> the `observe-now` SERVICE prints a formatted, column-aligned frame to the console
#   piped   -> the SHELL builds a record table (`build_observe_table`) with an extra `ticks` column
# `assert` needs a pipe, so everything here exercises the SHELL's record path. The service's
# console frame cannot be captured from inside the shell at all; it is asserted by the host harness
# in `osdev/src/shell_test.rs`, which reads raw serial and so can see it - including the column
# alignment, which is where a long service name broke the table and no test noticed.
#
# That split is worth knowing rather than glossing: a green line here does NOT mean the frame a
# person looks at is right. It means the introspection path behind it works.
#
# CONSEQUENCE, recorded because it is a real hole and not a technicality (§26.7): the harness that
# checks the frame runs only under QEMU, so ON HARDWARE nothing checks the table's alignment at all.
# A board-specific rendering problem would pass this suite. It cannot be closed by writing a better
# test here - `assert` needs a pipe and a pipe changes the renderer - so it is written down instead.
assert ok observe now
# The gated introspection path answers: the table is built from `task_stat`, so a service that is
# actually running has to appear in it.
observe now | assert contains supervisor
observe now | assert contains shell
# `ticks` is the column that distinguishes this from `status` (cumulative cpu-time). If it is absent
# the record form has silently degraded into a second `status`.
observe now | assert contains ticks
# The record verbs compose over it like any other producer (docs/records.md).
observe now | where name contains shell | assert contains shell
observe now | select name state | assert lacks ticks
observe now | to json | assert contains name

# ===== hw-enumerator: hardware discovery in USERSPACE (step D2) =====
echo ''
echo '===== hw-enumerator: userspace PCI discovery + its narrow authority ====='
# NOT EVERY MACHINE HAS PCI. This service exists on x86 and on the Pi 4; the Pi 2's peripherals hang
# off a memory-mapped bus with no PCI at all, so there is nothing here to enumerate and no service to
# ask. Probe for it and SKIP OUT LOUD, the same way the clock check does above - a silent skip is a
# test that has quietly stopped testing, and asserting it unconditionally would fail the Pi 2 for
# lacking hardware rather than for anything being wrong.
# The probe has to survive BOTH machines, and getting it wrong is quiet rather than loud - which is
# why it is written this way and not the obvious way.
#
# `for line in (status | where ...)` is the shape that reads best and it does NOT work: gsh refuses to
# capture a PIPELINE (bounded stack - it says so). The refusal made the loop body never run, `hwe`
# stayed 0, and the suite cheerfully printed SKIP on a machine that HAS the service. A probe that
# fails safe-looking is worse than one that fails loudly, so it is staged through a file the way the
# error message and every other capture in this suite do it.
#
# `count` is what makes the answer unambiguous: it counts DATA rows, not the header, so a match is 1
# and no match is 0. Reading the raw table instead would see a header row either way and always say
# "present". Root is used for the staging file because `/sc` is not created until much later in this
# script, and writing into a missing parent fails.
status | where name contains hw-enumerator | count | write /hwe.txt
let mut hwe = 0
for line in (read /hwe.txt) { if $line > 0 { hwe = 1 } }
delete /hwe.txt
if $hwe > 0 {
    # It is alive, and it is where the supervisor put it.
    status | where name contains hw-enumerator | assert contains hw-enumerator
    # It survived to serve: a service that logged its scan and then died would still be "in the
    # table" for a moment, so assert the state the supervisor keeps it in.
    status | where name contains hw-enumerator | assert lacks Dead
    # The kernel directory resolves it by name - the property that makes it reacquirable (§14.3).
    trace endpoints | assert contains hw-enumerator

    # THE AUTHORITY, which is the part actually worth pinning. `pci_cfg` is a hardware capability
    # granted to a userspace service, so its SHAPE is a security claim and not an implementation
    # detail: one configuration READ and nothing else. `caps` names the resource rather than printing
    # it as an anonymous id, so the claim is readable here at all.
    caps hw-enumerator | assert contains pci_cfg
    caps hw-enumerator | where resource=pci_cfg | assert contains read
    # THE REGRESSION GUARD. Config space holds every BAR and every command register, so write
    # authority over it is write authority over every device on the bus - there is no narrower form,
    # because the target is chosen by data rather than by the interface. It was minted READ|WRITE
    # once, before the write operation was removed. If anyone re-adds that right, this line fails and
    # says why, which is the whole point of writing it down as a test rather than as a comment.
    caps hw-enumerator | where resource=pci_cfg | assert lacks write
    # And it holds no authority it has no business holding: discovery does not spawn, kill, or reboot.
    caps hw-enumerator | assert lacks service_control
    caps hw-enumerator | assert lacks reboot
    caps hw-enumerator | assert lacks image_spawn
} else {
    echo 'SKIP  hw-enumerator - this machine has no PCI to enumerate (Pi 2); not a failure'
}

# `caps` must NAME a well-known resource, never print it as an anonymous number.
#
# THIS ASSERTS A PIPED `caps`, and for a while that was not the same thing as the `caps` a person
# reads. There were TWO copies of the naming table - one in the record producer, one in the console
# renderer - so naming all sixteen resources fixed the piped view while the console view kept
# printing `endpoint#8` for `reboot`. The piped test passed the whole time. The console output cannot
# be captured (piping is what switches renderers), so no assertion can ever guard it directly; the
# only real fix was to delete the duplicate so both views read from ONE table. This line therefore
# guards the naming for both, and that is only true while that remains a single table (§26.4). Ten of the sixteen
# used to fall through to an `endpoint#N` fallback, which reported (for instance) the shell's authority
# to reboot the machine as "endpoint#8" - a label naming the wrong KIND of thing, so a reader could not
# tell real authority from an ordinary IPC endpoint. Authority has to be readable (§26.9). The shell
# holds `reboot`, so it is the honest witness for this on every machine.
caps shell | assert contains reboot
caps shell | assert lacks endpoint#8

# ===== lifecycle guardrails + supervisor recovery (safe, deterministic) =====
echo ''
echo '===== lifecycle guardrails + supervisor recovery (safe, deterministic) ====='
# The shell COMMAND refuses spawn/restart of the supervisor (the recovery authority) - a command-layer
# hygiene check (no duplicate or self-restart of the restart authority), NOT "can't recover". `kill
# supervisor` is NOT refused: the supervisor is kernel-restartable (Phase 6), so a kill SUCCEEDS and it
# revives - asserted positively just below. `kill shell` is NOT tested: the shell is restartable now, so
# it would kill this run.
assert fails spawn supervisor
assert fails-with Denied spawn supervisor
# POSITIVE (Test 15 / Commandment V): kill the supervisor and assert it comes back to LIFE. `chaos
# kill-storm supervisor 1` captures its generation, kills it, waits on the generation BUMP (the truth,
# bounded - never a fixed sleep), and returns Ok only if a HIGHER generation appears; it prints
# "killed gen N -> recovered gen M". A no-show returns Err, so `assert ok` fails LOUDLY.
assert ok chaos kill-storm supervisor 1
assert fails spawn nosuchservice
assert fails-with Unknown spawn nosuchservice
assert fails kill nosuchservice
assert fails restart supervisor
assert fails restart nosuchservice

# ===== trace: the IPC observability views =====
echo ''
echo '===== trace: blocked / chain / deps / endpoints / ipc / status ====='
# Every view must ANSWER. A healthy machine has nothing blocked, and saying so is the correct answer -
# an empty result would not be (idle on your own endpoint is not stuck).
# `blocked`, `chain` and `status` print a report rather than records, so they are not pipe producers -
# `assert ok` is the form for those, and it is not a weaker check here: each returns Err when it cannot
# answer, which is exactly the failure being guarded against.
assert ok trace blocked
assert ok trace chain shell
# `deps` reads the LIVE capability table, so the shell must show the peers it actually holds.
trace deps shell | assert contains fs
trace deps fs | assert contains block-driver
# The tree and the record stream are the same data - the grid header names every filterable column.
trace deps shell | to grid | assert contains parent
trace deps shell | where peer contains fs | assert contains fs
# The endpoint inventory, and the inverse lookup it exists to feed.
trace endpoints | assert contains events
trace endpoints | where name contains fs | assert contains fs
# The ring itself answers, and reports its drop count (a silent loss is the bug - invariant 12).
assert ok trace status
# The shell has been calling `fs` throughout this suite, so the ring holds real traffic.
trace ipc | assert contains outcome
# ===== events: the METRIC table, in the same service that holds the ring =====
# This is the half of `events` that is not the trace ring, and the reason the service is no longer
# called `logger`: it holds published samples as well as events, and never held a log line in its life
# (`ctx.log()` is syscall 5, straight to the kernel ring and serial - CLAUDE.md 11.4).
assert ok trace metrics
# THE SINK PUBLISHES ITS OWN NUMBERS BY LOCAL WRITE, NEVER BY SENDING ITSELF A MESSAGE. A send is
# itself a reportable event, so a self-emit over IPC would feed the ring from the ring and fill it with
# its own reporting. These rows existing is that local-write path working, and it is the executable
# form of the rule in docs/observability.md 9.
trace metrics | assert contains ring.recorded
trace metrics | assert contains metrics.held
# ...and an ORDINARY service publishes over IPC, which is the path any new service would use. `fs` has
# served this entire suite, so it is far past its 32-request publish interval.
trace metrics | assert contains blk.outages
# EVERY internal service is registered, not just the two that started with the cap. `msgs.received` is
# counted in the SDK's receive paths, so a service gets it by existing rather than by remembering to
# add it - which is the same reason trace emission lives there. It publishes on the FIRST message as
# well as every 64th: a service under the interval would otherwise have NO ROW, and no row is
# indistinguishable from dead, which is the one question this metric exists to answer.
trace metrics | assert contains msgs.received
# Named services, on every port: the terminal, storage, and the clock. Attribution is the point - an
# undeclared service publishes under a BLANK owner, and since the key is (owner, name) every such
# service collides into ONE row with the counters interleaving. Caught exactly that way: a single
# `msgs.received 1920` belonging to nobody, which was `console` plus nine others.
trace metrics | where owner contains console | assert contains msgs.received
trace metrics | where owner contains block-driver | assert contains msgs.received
trace metrics | where owner contains time | assert contains msgs.received
# It is a record source like `trace ipc`, so it filters like one.
trace metrics | where owner contains events | assert contains ring.recorded
# An unknown view is refused loudly here too.
assert fails trace metricz

# ===== events: the reader named after the service =====
# `events` IS the reader; `trace` is the older name for the same views and both must keep working.
# The service holds three streams now - logs, IPC traces and metrics - and only one of them is a
# trace, so a reader named after the service is the discoverable one.
assert ok events status
assert ok events metrics
# PIPED, NOT BARE. `events ipc` unpiped is the INTERACTIVE PAGED view and waits for a keypress - the
# shell test drives it by sending `q`. A script has no one to press it, so a bare `assert ok events ipc`
# hangs the whole suite until the harness times out, which is exactly what it did.
events ipc | assert contains outcome
# `events trace` reads naturally once the command is `events`, and resolves to the same view as `ipc`.
events trace | assert contains outcome
# The alias is a real record source, not just a printer: it filters exactly as `trace` does.
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
mkdir /sc
events log | write /sc/evt.log
read /sc/evt.log | assert contains owner
events log | where owner=block-driver | write append /sc/evt.log
assert ok read /sc/evt.log

# ===== events persist: capture to disk, via a service that is NOT `events` =====
# `recorder` drains `events` and writes the file. It is spawned ON DEMAND by the line below and is
# absent from the kernel managed-service lists, so this whole feature costs the kernel nothing.
#
# Why it is a separate service at all: a file write BLOCKS on a reply, and a blocked `events` stops
# draining its endpoint and drops the very events worth capturing. Here the blocking is harmless -
# nothing depends on `recorder`, so a stalled disk stalls it alone and the volatile window survives.
events persist status | assert contains not running
assert ok events persist start /sc/cap.log
events persist status | assert contains recording
ls /sc | assert contains cap.log
assert ok events persist stop
# The footer is what makes a capture readable as COMPLETE. A file with a header and no footer died.
events persist status | assert contains idle

# NOT asserted here: filtering for a SPECIFIC owner. Which services have logged inside the 8 KiB
# window varies by machine and by how far it has wrapped, so any such assertion is a coin flip on
# hardware. Filtering by owner is already pinned above, on the metrics view, where the rows are stable.
#
# One owner can NEVER appear, and it is worth knowing why: `events` itself. It holds no send cap to
# itself, so its `ctx.log()` copy resolves to `u32::MAX` and goes nowhere - the same cut that stops the
# sink tracing its own sends. Its lines reach serial by syscall like everyone's; only the queryable
# copy is absent. `events log | where owner=events` was asserted here at first and failed, which is the
# self-observation rule in section 9 of docs/observability.md doing its job on the test that forgot it.
# An unknown view under the new name is refused as loudly as under the old one.
assert fails events nosuchview

# Every view refuses an unknown subject LOUDLY rather than answering with nothing.
assert fails trace chain nosuchsvc
assert fails trace deps nosuchsvc
assert fails trace endpoint notanumber
assert fails trace nosuchview

# THE SINK IS RESTARTABLE, AND THE READER MUST SURVIVE IT. `events` holds the trace ring; killing it
# invalidates every cached capability to it. Without a reacquire the shell keeps a stale generation
# forever, so `trace ipc` reports a live service as unreachable and every emission logs a kernel
# gen-mismatch - which is exactly what a chaos storm produced on hardware (cap 985 vs record 1025)
# before this was fixed. Kill it, then prove the views still work (14.3: reacquire by name, retry).
assert ok chaos kill-storm events 1
assert ok trace status
trace ipc | assert contains outcome
trace endpoints | assert contains events
# THE METRIC TABLE IS VOLATILE AND DIES WITH THE SINK. That is correct, not a gap: a restart is a
# re-init and not a resume (14.2), and it is exactly why `events` must never acquire a durable-storage
# dependency - a service that reports a storage failure must not be downstream of storage. What has to
# survive the kill is the VIEW, which answers again and refills as services publish. The one thing it
# can never report is its OWN death; the supervisor's death notification and the kernel's unconditional
# serial write do that, and both sit beneath it.
assert ok trace metrics

# ===== files: create / read / overwrite / append / empty / quoted =====
echo ''
echo '===== files: create / read / overwrite / append / empty / quoted ====='
# Start from a known-empty tree, whatever the previous run did or did not finish.
#
# As an `if` CONDITION, not a bare statement. A bare `delete /sc recursive` fails on a clean tree
# (there is nothing to delete) and the runner counts every failing statement - so the line added to
# make the suite re-runnable was itself the one failure in every otherwise-perfect run: 350/1 four
# times over, caused by the cleanup rather than anything under test. A condition is evaluated for its
# truth and never tallied, which is exactly the semantics wanted here: delete it IF it is there.
if ls /sc { delete /sc recursive }
mkdir /sc
assert ok ls /sc
assert fails mkdir /sc
write /sc/a.txt hello
read /sc/a.txt | assert contains hello
write /sc/a.txt world
read /sc/a.txt | assert contains world
read /sc/a.txt | assert lacks hello
write append /sc/a.txt MORE
read /sc/a.txt | assert contains worldMORE
write append /sc/fresh.txt born
read /sc/fresh.txt | assert contains born
# prepend (standalone): adds to the FRONT; append + prepend compose to TOP-MID-END
write /sc/pp.txt MID
write append /sc/pp.txt -END
write prepend /sc/pp.txt TOP-
read /sc/pp.txt | assert contains TOP-MID-END
# append/prepend as PIPE SINKS (capture then add): header lands before footer
echo footer | write append /sc/ap.txt
read /sc/ap.txt | assert contains footer
echo header | write prepend /sc/ap.txt
read /sc/ap.txt | assert contains header
read /sc/ap.txt | assert contains footer
# pipe producer → file, then read back (capture-to-disk of a text producer + help)
about | write /sc/about.txt
read /sc/about.txt | assert contains GodspeedOS
help | write /sc/help.txt
read /sc/help.txt | assert contains Storage
write /sc/empty.txt
read /sc/empty.txt | assert empty
write /sc/q.txt "two words"
read /sc/q.txt | assert contains two words
assert fails read /sc/missing.txt
assert fails-with FileNotFound read /sc/missing.txt

# ===== directories: mkdir (parents) + delete guard =====
echo ''
echo '===== directories: mkdir (parents) + delete guard ====='
assert fails mkdir /sc/x/y/z
mkdir /sc/x/y/z parents
assert ok ls /sc/x/y/z
mkdir /sc/x/y2 parents
assert ok ls /sc/x/y2
mkdir /sc/d1
write /sc/d1/f.txt data
assert fails delete /sc/d1
assert ok read /sc/d1/f.txt

# ===== copy / move / rename (positive + negative) =====
echo ''
echo '===== copy / move / rename (positive + negative) ====='
copy /sc/a.txt /sc/b.txt
read /sc/b.txt | assert contains worldMORE
assert ok read /sc/a.txt
assert fails copy /sc/missing.txt /sc/z.txt
copy /sc/d1 /sc/d2 recursive
assert ok read /sc/d2/f.txt
move /sc/b.txt /sc/c.txt
assert ok read /sc/c.txt
assert fails read /sc/b.txt
assert fails move /sc/missing.txt /sc/q2.txt
rename /sc/c.txt renamed.txt
assert ok read /sc/renamed.txt
write /sc/keep.txt x
assert fails rename /sc/renamed.txt keep.txt

# ===== cd: absolute / relative / parent / negative =====
echo ''
echo '===== cd: absolute / relative / parent / negative ====='
cd /sc
assert ok read a.txt
ls | assert contains a.txt
cd /sc/d1
cd ..
ls | assert contains a.txt
cd -
assert ok read /sc/a.txt
assert fails cd /sc/a.txt
cd /

# ===== ls / find / tree as record producers (still referencing d1/d2) =====
echo ''
echo '===== ls / find / tree as record producers (still referencing d1/d2) ====='
ls /sc | where type=file | assert contains a.txt
ls /sc | where type=dir | assert contains d1
ls /sc | where type=file | assert lacks d1
ls /sc | select name | assert contains a.txt
ls / | where type=dir | assert contains sc
find a.txt /sc | assert contains /sc/a.txt
find f.txt /sc | where type=file | assert contains /sc/d1/f.txt
find fresh.txt | assert contains /sc/fresh.txt
find *.txt /sc | assert contains fresh.txt
assert ok find nomatchxyz /sc
tree /sc | assert contains d1
tree /sc | assert contains d2
tree /sc | assert contains x

# ===== directory move / rename (after the d1/d2 checks above) =====
echo ''
echo '===== directory move / rename (after the d1/d2 checks above) ====='
move /sc/d2 /sc/d3
assert ok read /sc/d3/f.txt
assert fails read /sc/d2/f.txt
rename /sc/d1 dd1
assert ok read /sc/dd1/f.txt
assert fails read /sc/d1/f.txt

# ===== byte pipes: producers + filters (each line spawns a service; kept lean) =====
echo ''
echo '===== byte pipes: producers + filters (each line spawns a service; kept lean) ====='
greet | assert contains hello
greet | match capability | assert contains capability
greet | count | assert contains 3 lines
greet | sort | first 1 | assert contains capability
greet | sort | last 1 | assert contains ambient
echo lower CASE | upper | assert contains LOWER CASE
echo alpha beta gamma | match beta | assert contains beta

# ===== record service over the binary wire codec (roster) - lean operator sample =====
echo ''
echo '===== record service over the binary wire codec (roster) - lean operator sample ====='
assert ok roster
roster | where role=core | assert contains Matthew
roster | where role!=core | assert lacks Matthew
roster | where seat>1 | assert lacks Matthew
roster | where seat=1 | assert contains Matthew
roster | where name contains ar | assert contains Mark
roster | sort reverse seat | assert contains John
roster | to json | assert contains role
roster | to json | from json | where role=core | assert contains Matthew
roster | select name seat | to json | assert contains Luke

# ===== json <-> records bridge (exhaustive where/select/sort - no service spawn) =====
echo ''
echo '===== json <-> records bridge (exhaustive where/select/sort - no service spawn) ====='
write /sc/data.json '[{"name":"x","n":1},{"name":"y","n":2},{"name":"z","n":3}]'
read /sc/data.json | from json | assert contains y
read /sc/data.json | from json | where n>1 | assert contains z
read /sc/data.json | from json | where n>1 | assert lacks x
read /sc/data.json | from json | where n<2 | assert contains x
read /sc/data.json | from json | where n=2 | assert contains y
read /sc/data.json | from json | where n!=2 | assert lacks y
read /sc/data.json | from json | where n>=2 | assert lacks x
read /sc/data.json | from json | where n<=1 | assert contains x
read /sc/data.json | from json | where name contains y | assert contains y
read /sc/data.json | from json | select name | assert contains z
read /sc/data.json | from json | select name n | to yaml | assert contains name
read /sc/data.json | from json | sort n | assert contains x
read /sc/data.json | from json | sort reverse n | assert contains z

# ===== fsck: drives check rebuilds bitmap/free from the populated tree, finds no corruption =====
echo ''
echo '===== fsck: drives check rebuilds bitmap/free from the populated tree, finds no corruption ====='
assert ok drives check

# ===== scrub: read-only CRC integrity sweep over the populated tree finds no bit-rot =====
echo ''
echo '===== scrub: read-only CRC integrity sweep over the populated tree finds no bit-rot ====='
assert ok drives scrub

# ===== file-as-capability (§7.10, P2): open a file as a REAL kernel cap and exercise every
# property - read/write THROUGH the cap, non-escalation (a read-only cap can't write, at both
# the kernel and fs layers), unforgeable handle, revoke-on-close. `fcap` is Ok only if all hold.
# It is self-contained: it creates and deletes its own throwaway file, so it takes no argument. =====
assert ok fcap

# ===== fmt: format a .gsh script to the canonical layout, then verify it (fmt check) =====
echo ''
echo '===== fmt: format a .gsh script to canonical layout, then fmt check ====='
write /sc_fmt.gsh "echo aaa ; echo bbb"  # a ;-joined one-liner - NOT canonical
fmt /sc_fmt.gsh                          # format IN PLACE -> one statement per line
fmt check /sc_fmt.gsh                    # now canonical -> Ok (silent)
if result == Ok { echo fmt-ok | assert contains fmt-ok } else { fail "fmt: not canonical after format" }
read /sc_fmt.gsh | assert contains bbb   # semantics-preserving: the content survived the format
delete /sc_fmt.gsh

# ===== cleanup: proves delete + delete recursive =====
echo ''
echo '===== cleanup: proves delete + delete recursive ====='
delete /sc/a.txt
assert fails read /sc/a.txt
delete /sc recursive
assert fails ls /sc

# ---- network: RECEIVE must work, checked without sending anything ----------------------------
#
# A pure READ of state: it asks what already happened, it does not make anything happen.
#
# What it catches: a driver change stopped the receive channel being armed, so the host transmitted
# normally and received nothing. DHCP got no offer, net-stack fell back to an address the network does
# not route - and all 351 other tests still passed, because none of them touched the network. A stack
# that cannot receive cannot get a lease. That is the whole check.
#
# WAITS ON THE ANSWER, NOT ON A MOMENT. The first version asked once and failed on a machine whose PHY
# negotiated link 50 seconds into the boot: net-stack was mid-DHCP (it blocks its serve loop for the
# length of a dance), `net` timed out, and the suite reported a network fault that did not exist. So it
# retries while there is no answer, bounded, and fails only if none ever comes - which is Commandment
# VIII's distinction exactly: wait for the truth, with a bound, never for a fixed interval.
#
# `net lease` prints ONE word so the retry can test it with the grammar this language has, and prints
# NOTHING while net-stack is busy - so silence retries rather than counting as a verdict.
let mut leaseok = 0
for i in range 20 {
    if $leaseok < 1 {
        for line in (net lease) { if $line in ok { leaseok = 1 } }
        if $leaseok < 1 { wait 1 }
    }
}
if $leaseok > 0 {
    echo 'PASS  net - the stack holds a lease (or has no link to need one)'
} else {
    fail 'net: no lease after 20s - the receive path or DHCP is broken'
}

# ---- network: NAME RESOLUTION, asserted only where it can be OUR fault -----------------------
#
# What it catches: DNS is a different code path from everything above it. ICMP can be perfect while
# UDP request/reply is broken - and it was, twice in one session. `nic-driver` used to hand a received
# frame back as the answer to a TRANSMIT, and `udp_roundtrip` collected its first frame from that
# reply and its next from an ARP-reply's; decoupling that touched DNS and nothing else. Ping stayed
# green throughout. The suite would not have noticed either way, because nothing here resolved a name.
#
# GATED ON THE INTERNET BEING DEMONSTRABLY REACHABLE, and that is the whole design. A machine with no
# cable, no DHCP server, no route, or no WAN cannot resolve a name, and none of that is a defect in
# this system - failing there would be a suite that cries wolf on a laptop at a coffee shop. So the
# assertion runs ONLY after ICMP to the internet has just succeeded. Once that holds, the network is
# proven end to end, and a name that will not resolve is ours: same cable, same lease, same gateway,
# same driver, different code path. The skip is not a weakened check, it is the check declining to
# make a claim it cannot support.
#
# ONE RETRY, bounded, for the same reason the lease gate retries: a single lost UDP datagram is not a
# broken resolver, and DNS has no retransmit of its own here.
let mut dnsok = 0
if ping count 2 8.8.8.8 {
    for i in range 2 {
        if $dnsok < 1 {
            if net dns example.com { dnsok = 1 } else { wait 1 }
        }
    }
    if $dnsok > 0 {
        echo 'PASS  dns - names resolve over a network that is proven reachable'
    } else {
        fail 'dns: ICMP to 8.8.8.8 works but no name resolves - the UDP request/reply path is broken'
    }
} else {
    echo 'PASS  dns - skipped, no internet to resolve through (not a fault of this system)'
}
