# GodspeedOS self-check, part 1 of 9: the gsh language itself: bindings, arithmetic, control flow, functions, capture
#
# One of the parts `selfcheck` runs, in the order they are listed in `SELFCHECK_PARTS`
# (`services/shell/src/main.rs`). `selfcheck language` runs THIS part alone; `selfcheck` runs them all
# and prints one tally for the whole suite.
#
# They are separate files because a baked script is indexed with u16 offsets and the single file had
# reached 65,139 bytes of a 65,536 ceiling (`backlog/47`). Each part now carries its own budget - and
# its own 256-statement detail budget, so the report no longer stops naming statements part way.
#
# No part may declare a `fn` or a `let` another part uses: each is interpreted on its own, with a
# fresh variable table. What DOES carry across is the working directory and the disk.

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
#   - exhaustive operator coverage runs on FREE producers (status, dir, json) to avoid
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
if dir /tour { delete /tour recursive }    # an aborted run leaves it behind; mkdir would then fail
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
# A SKIP IS NOT A PASS - and two of these lines said it was.
#
# Every conditional section announces itself when it declines to run: the clock on a machine that
# cannot know the time, PCI on a Pi 2 that has none, DHCP with nothing serving it, DNS with no
# internet, churn with no disk. That was always right - a silent skip is a test that has quietly
# stopped testing.
#
# Five of them said `SKIP`. Two said `PASS  ... - skipped`, which claims a check succeeded when it
# never ran. That is an unearned claim, and on a machine whose disk is raw or absent it is not a
# small one: the storage sections are a large part of this suite, and a reader scanning for PASS
# would conclude the filesystem had been exercised. The inconsistency is the tell - the wording
# drifted where nobody was comparing the two.
#
# All seven are `skip` STATEMENTS now, not echoes - which is the part that makes the difference
# countable. `skip` is the counterpart to `fail` the language was missing: `fail` says a check did
# not hold, `skip` says it was never in a position to run, and neither is a pass. The run ends with
#
#     --- skipped ---
#     SKIP  skip 'churn - no storage to churn on this machine; not a failure'
#     run: ran 492, failed 0, skipped 3
#
# so a machine with a raw or absent disk reports 0 FAILURES and says plainly how much it did not do.
# An echo could never do that: it printed a word into a wall of output and the tally never saw it.

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
    skip 'for-line: no writable storage on this machine'
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
dir /tour | assert lacks work             # proof the defer ran: /tour/work is gone

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
assert fails dir /tour                    # the tour dir is gone

echo ''
echo '#################### gsh LANGUAGE TOUR complete ####################'
echo ''
