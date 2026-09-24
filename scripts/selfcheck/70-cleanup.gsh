# GodspeedOS self-check, part 8 of 9: delete and delete recursive, proved by removing what the suite made
#
# One of the parts `selfcheck` runs, in the order they are listed in `SELFCHECK_PARTS`
# (`services/shell/src/main.rs`). `selfcheck cleanup` runs THIS part alone; `selfcheck` runs them all
# and prints one tally for the whole suite.
#
# They are separate files because a baked script is indexed with u16 offsets and the single file had
# reached 65,139 bytes of a 65,536 ceiling (`backlog/47`). Each part now carries its own budget - and
# its own 256-statement detail budget, so the report no longer stops naming statements part way.
#
# No part may declare a `fn` or a `let` another part uses: each is interpreted on its own, with a
# fresh variable table. What DOES carry across is the working directory and the disk.

# ===== cleanup: proves delete + delete recursive =====
echo ''
echo '===== cleanup: proves delete + delete recursive ====='
delete /sc/a.txt
assert fails read /sc/a.txt
delete /sc recursive
assert fails dir /sc
