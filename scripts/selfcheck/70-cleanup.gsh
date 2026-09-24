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
# MAKES ITS OWN SUBJECT. This used to delete `/sc/a.txt` and `/sc` and assume an earlier part had
# built them, which made it the one part that could not run alone: `selfcheck cleanup` on a clean tree
# deleted nothing and failed. A part that only works after another part is not a part.
if dir /sc { delete /sc recursive }
mkdir /sc
write /sc/a.txt doomed
assert ok read /sc/a.txt
delete /sc/a.txt
assert fails read /sc/a.txt

# A NON-EMPTY DIRECTORY MUST REFUSE a plain delete - the guardrail, not an accident of ordering.
mkdir /sc/deep
write /sc/deep/inner.txt also-doomed
assert fails delete /sc
delete /sc recursive
assert fails dir /sc
