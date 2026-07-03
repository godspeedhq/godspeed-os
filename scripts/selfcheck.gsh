# GodspeedOS extensive self-check suite.
# Run it with:  gsh> selfcheck      (runs this embedded suite IN MEMORY - no disk write,
# so it is not capped by the on-disk file size; it just needs a flashed GSFS drive for
# the file-command tests). Passes iff the summary says "failed 0".
#
# Covers every shell utility's main functions + negative cases, EXCEPT:
#   - observe : it is a live full-screen view (only `observe now` is a snapshot; tested).
#   - drives  : flashing/relabel/reset touch disks and prompt y/N - not scriptable.
# Re-runnable: everything is created under /sc and deleted at the end.
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
# #  tour must finish "failed 0" - the syntax below is executable proof,    #
# #  not pseudocode. Read top-to-bottom to learn the language. Works under  #
# #  /tour (removed at the end). `import` is a `run <file>`-time feature, so #
# #  it is shown (not run) here - see the note near the bottom.             #
# ##########################################################################

mkdir /tour                              # a scratch directory for the tour's files

# -- 1. VARIABLES ----------------------------------------------------------
#  `let` binds an IMMUTABLE variable; `let mut` a mutable one (reassign it later
#  with `name = ...`). `$name` expands anywhere; "..." interpolates, '...' is raw.
let name = Ada                           # immutable binding
let mut hits = 0                         # mutable counter (bumped in a loop below)
echo "hello, $name" | assert contains hello, Ada     # double quotes interpolate
echo 'raw text - $name stays literal'                # single quotes: no expansion

# -- 2. ARITHMETIC (inline, space-separated, NOT a command) ----------------
#  Operators + - * / % with ( ) grouping and real precedence.
let total = 2 + 3 * 4                    # * binds tighter than + -> 14
echo $total | assert contains 14
let grouped = ( 2 + 3 ) * 4              # parentheses override precedence -> 20
echo $grouped | assert contains 20

# -- 3. RESULT + IF / ELSE, comparisons, `in` ------------------------------
#  Every command yields Ok/Err; `result` is the previous one's outcome. `if`
#  takes a command OR a comparison (== != < > <= >=), plus `<val> in a b c`.
write /tour/a.txt hi                     # a real command...
if result == Ok { echo wrote-ok | assert contains wrote-ok }   # ...check its result
if $total > 10 { echo big | assert contains big } else { fail "math broke" }
if $name in Ada Bob Cy { echo known-name | assert contains known-name }

# -- 4. SWITCH (several values per arm, `_` default) -----------------------
switch $name {
    Bob Cy   { fail "wrong arm" }        # an arm may list multiple values
    Ada      { echo matched-ada | assert contains matched-ada }
    _        { fail "default must not run" }
}

# -- 5. $( ) CAPTURE (a producer's output into a variable) -----------------
#  Capture a BARE producer: $(echo ...), $(date), $(read /file), $(greet)...
let phrase = $(echo hi there)            # -> "hi there"
echo got:$phrase | assert contains got:hi

# -- 6. FOR LOOPS (words, range, mutable accumulation) ---------------------
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

# -- 7. UNBOUNDED loop + break / continue ----------------------------------
let mut k = 0
loop {                                   # runs until `break` (100k-iteration backstop)
    k = $k + 1
    if $k == 2 { continue }              # skip the rest of THIS pass
    if $k > 4  { break }                 # leave the loop entirely
    echo pass-$k                         # prints pass-1, pass-3, pass-4
}

# -- 8. FUNCTIONS (named params, return, bounded recursion) ----------------
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

# -- 9. DEFER (cleanup on scope exit - LIFO, even on fail) ------------------
fn build_thing {
    mkdir /tour/work
    defer delete /tour/work recursive    # runs when this function returns, however we leave it
    write /tour/work/out done
    read /tour/work/out | assert contains done
}                                        # <-- the deferred delete fires HERE, on return
build_thing
ls /tour | assert lacks work             # proof the defer ran: /tour/work is gone

# -- 10. RECORD AGGREGATORS (typed-pipe reducers) --------------------------
#  Pipes carry TYPED records, so a pipeline can REDUCE - impossible for byte pipes.
write /tour/inv.json '[{"item":"a","qty":10},{"item":"b","qty":20},{"item":"c","qty":30}]'
read /tour/inv.json | from json | count   | assert contains 3    # row count (dual: rows|lines)
read /tour/inv.json | from json | sum qty | assert contains 60   # 10 + 20 + 30
read /tour/inv.json | from json | min qty | assert contains 10
read /tour/inv.json | from json | max qty | assert contains 30
read /tour/inv.json | from json | avg qty | assert contains 20

# -- IMPORT (shown, not run: libraries load at `run <file>` time) ----------
#  A .gsh run from disk can pull functions from a library file:
#      from /lib/assert.gsh import ok fails as denied   # selective, with `as` rename
#      import /lib/math.gsh                              # all of a lib's functions
#  Names collide loudly (resolve with `as`); the run's pre-scan then indexes
#  the imported functions. Exercised end-to-end by `osdev test files`.

# -- tour cleanup: leave nothing behind ------------------------------------
delete /tour recursive
assert fails ls /tour                    # the tour dir is gone

# ===== meta: the result model + the assert forms themselves =====
assert ok echo hello
assert fails totallybogus
assert fails-with Unknown totallybogus
assert ok result
echo one two three | assert contains two
echo keep this | assert lacks secret
echo "spaced words stay" | assert contains spaced words stay
echo nothing | match zzz | assert empty

# ===== self-documentation: <util> help / <util> version =====
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
assert ok clear help

# ===== system info - now PIPE PRODUCERS (text emitters captured via Out), bare + piped =====
assert ok about
assert ok cores
assert ok mem
assert ok date
assert ok date epoch
about | assert contains GodspeedOS
cores | assert contains cores
mem | assert contains used
date | assert contains :
help | assert contains status
# uptime - a record producer (wall-clock RTC delta): bare grid + json + column projection.
assert ok uptime
uptime | assert contains seconds
uptime | to json | assert contains seconds
uptime | select seconds | to json | assert lacks uptime

# ===== introspection producers: status / caps (+ every where operator, no spawn) =====
assert ok status
status | assert contains shell
status | where name=shell | assert contains shell
status | where name!=shell | assert lacks shell
status | where core=0 | assert contains shell
status | where state=Running | assert contains shell
status | where slot>=0 | assert contains shell
status | where core<1 | assert contains shell
status | where name~super | assert contains supervisor
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

# ===== lifecycle guardrails (negative only - safe, deterministic) =====
# The shell COMMAND guards spawn/kill of the supervisor (the recovery authority) - a command-layer
# hygiene check, not "can't recover" (the supervisor is kernel-restartable, Phase 6). `kill shell` is
# NOT tested here: the shell is restartable now, so it succeeds (self-restart) and would kill this run.
assert fails spawn supervisor
assert fails-with Denied spawn supervisor
assert fails kill supervisor
assert fails-with Denied kill supervisor
assert fails spawn nosuchservice
assert fails-with Unknown spawn nosuchservice
assert fails kill nosuchservice
assert fails restart supervisor
assert fails restart nosuchservice

# ===== files: create / read / overwrite / append / empty / quoted =====
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
move /sc/d2 /sc/d3
assert ok read /sc/d3/f.txt
assert fails read /sc/d2/f.txt
rename /sc/d1 dd1
assert ok read /sc/dd1/f.txt
assert fails read /sc/d1/f.txt

# ===== byte pipes: producers + filters (each line spawns a service; kept lean) =====
greet | assert contains hello
greet | match capability | assert contains capability
greet | count | assert contains 3 lines
greet | sort | first 1 | assert contains capability
greet | sort | last 1 | assert contains ambient
echo lower CASE | upper | assert contains LOWER CASE
echo alpha beta gamma | match beta | assert contains beta

# ===== record service over the binary wire codec (roster) - lean operator sample =====
assert ok roster
roster | where role=core | assert contains Matthew
roster | where role!=core | assert lacks Matthew
roster | where seat>1 | assert lacks Matthew
roster | where seat=1 | assert contains Matthew
roster | where name~ar | assert contains Mark
roster | sort reverse seat | assert contains John
roster | to json | assert contains role
roster | to json | from json | where role=core | assert contains Matthew
roster | select name seat | to json | assert contains Luke

# ===== json <-> records bridge (exhaustive where/select/sort - no service spawn) =====
write /sc/data.json '[{"name":"x","n":1},{"name":"y","n":2},{"name":"z","n":3}]'
read /sc/data.json | from json | assert contains y
read /sc/data.json | from json | where n>1 | assert contains z
read /sc/data.json | from json | where n>1 | assert lacks x
read /sc/data.json | from json | where n<2 | assert contains x
read /sc/data.json | from json | where n=2 | assert contains y
read /sc/data.json | from json | where n!=2 | assert lacks y
read /sc/data.json | from json | where n>=2 | assert lacks x
read /sc/data.json | from json | where n<=1 | assert contains x
read /sc/data.json | from json | where name~y | assert contains y
read /sc/data.json | from json | select name | assert contains z
read /sc/data.json | from json | select name n | to yaml | assert contains name
read /sc/data.json | from json | sort n | assert contains x
read /sc/data.json | from json | sort reverse n | assert contains z

# ===== fsck: drives check rebuilds bitmap/free from the populated tree, finds no corruption =====
assert ok drives check

# ===== scrub: read-only CRC integrity sweep over the populated tree finds no bit-rot =====
assert ok drives scrub

# ===== file-as-capability (§7.10, P2): open a file as a REAL kernel cap and exercise every
# property - read/write THROUGH the cap, non-escalation (a read-only cap can't write, at both
# the kernel and fs layers), unforgeable handle, revoke-on-close. `fcap` is Ok only if all hold.
# It is self-contained: it creates and deletes its own throwaway file, so it takes no argument. =====
assert ok fcap

# ===== cleanup: proves delete + delete recursive =====
delete /sc/a.txt
assert fails read /sc/a.txt
delete /sc recursive
assert fails ls /sc
