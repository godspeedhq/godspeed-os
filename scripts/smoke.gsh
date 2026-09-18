# Small host-baked smoke suite. `osdev test script` bakes THIS as a file into a GSFS disk
# and runs `run /smoke.gsh` - proving the script-disk → run-from-file path (including a piped
# assert, which a file can carry but on-device `write` cannot author). The extensive coverage
# lives in the embedded suite (`selfcheck` / scripts/selfcheck.gsh). Passes iff "failed 0".
# Which build produced this log? Same reason as selfcheck.gsh: a run that cannot identify its own
# image is worth less than it looks.
version
assert ok echo hi
echo hello world | assert contains world
mkdir /sm
write /sm/f.txt data
read /sm/f.txt | assert contains data
dir /sm | where type=file | assert contains f.txt
# guard (Tier 2): a function must NOT shadow a piped producer - defining `fn greet` must not
# hijack `greet | ...`, which still runs the greet SERVICE (a function is not a pipe source).
fn greet { echo FN-GREET-BUG }
greet | count | assert contains 3 lines
# $( ) capture stages a pipeline through a file (a direct pipeline capture would overflow the stack):
greet | count | write /sm/c.txt
let cnt = $(read /sm/c.txt)
echo cnt-is:$cnt | assert contains 3 lines
# THE `paginate` NO-HUMAN GUARD, proven where it can only be proven: inside a script.
# A pager that waits for a key in a run nobody is watching does not degrade, it HANGS - and that
# is the whole reason paging is an explicit stage rather than something `dir` and `help` do on
# their own. NO KEYS ARE SENT for this line. If the guard is wrong the suite never finishes; if
# it is right the rows print and the next assert runs. This cannot be tested from the interactive
# shell, because a line containing `|` is a pipeline and so `write` can never store one.
dir /sm | paginate
assert ok echo past-paginate
roster | where role=core | assert contains Matthew
assert fails read /sm/nope
assert fails-with FileNotFound read /sm/nope
delete /sm recursive
assert fails dir /sm
