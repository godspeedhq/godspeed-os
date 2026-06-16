# Small host-baked smoke suite. `osdev test script` bakes THIS as a file into a GSFS disk
# and runs `run /smoke.gsh` — proving the script-disk → run-from-file path (including a piped
# assert, which a file can carry but on-device `write` cannot author). The extensive coverage
# lives in the embedded suite (`selfcheck` / scripts/selfcheck.gsh). Passes iff "failed 0".
assert ok echo hi
echo hello world | assert contains world
mkdir /sm
write /sm/f.txt data
read /sm/f.txt | assert contains data
ls /sm | where type=file | assert contains f.txt
greet | count | assert contains 3 lines
roster | where role=core | assert contains Matthew
assert fails read /sm/nope
assert fails-with FileNotFound read /sm/nope
delete /sm recursive
assert fails ls /sm
