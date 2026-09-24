# GodspeedOS self-check, part 6 of 9: files, directories, copy/move/rename, cd, dir/find/tree, seal
#
# One of the parts `selfcheck` runs, in the order they are listed in `SELFCHECK_PARTS`
# (`services/shell/src/main.rs`). `selfcheck files` runs THIS part alone; `selfcheck` runs them all
# and prints one tally for the whole suite.
#
# They are separate files because a baked script is indexed with u16 offsets and the single file had
# reached 65,139 bytes of a 65,536 ceiling (`backlog/47`). Each part now carries its own budget - and
# its own 256-statement detail budget, so the report no longer stops naming statements part way.
#
# No part may declare a `fn` or a `let` another part uses: each is interpreted on its own, with a
# fresh variable table. What DOES carry across is the working directory and the disk.

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
if dir /sc { delete /sc recursive }
mkdir /sc
assert ok dir /sc
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

# ===== events: the metric rows that need FILE TRAFFIC to exist (moved here deliberately) =====
# `fs` publishes `requests` and `blk.outages` every 32 requests it serves, and the SDK publishes
# `msgs.received` on a service's first message and every 64th. The sink's table is VOLATILE - a chaos
# storm that restarts `events` wipes it - so these rows exist only once their owner has served enough
# traffic SINCE that restart. Asserting them in the metrics section above meant asserting them before
# this suite had done any file work, which is why they failed on the T630 after a 100-round storm and
# passed everywhere else: pure luck about where each service sat in its publish interval.
#
# By this line the suite has written a 256 KiB capture and this whole files section through `fs`, so
# both owners are far past their interval. Deterministic, and no extra runtime.
#
# AND IT IS DELIBERATELY AFTER `chaos kill-storm events` ABOVE, which makes this a regression test for
# the bug it caught: a service whose FIRST emission landed while the sink was mid-restart used to latch
# "no sink" for its entire life and go permanently silent, because it never sent again and so never
# reached the reacquire path. `fs` did exactly that on the T630 - six rows before the storm, zero for
# the rest of the boot. These three rows existing AFTER the sink was deliberately killed is the proof
# that a publisher recovers rather than dying quiet.
# DRIVE THE TRAFFIC THESE ROWS ARE MADE OF, immediately before asserting them. A row appears when
# its owner RECEIVES something, so asserting one for a service that happens to be idle is asserting
# a coin flip. `block-driver` only receives when `fs` touches the disk, and after a chaos storm has
# restarted the sink there is no guarantee anything has since. Observed exactly once in four runs on
# a single-core Wyse - the rarest kind of failure to chase and the easiest to remove.
#
# A write and a read back is one round trip through fs to block-driver and back, so by the next line
# both have received something since the sink last restarted. Deterministic, not hopeful.
write /sc/mrow.txt row
read /sc/mrow.txt | assert contains row
events metrics | assert contains blk.outages
events metrics | where owner contains block-driver | assert contains msgs.received
events metrics | where owner contains fs | assert contains msgs.received

# ===== directories: mkdir (parents) + delete guard =====
echo ''
echo '===== directories: mkdir (parents) + delete guard ====='
assert fails mkdir /sc/x/y/z
mkdir /sc/x/y/z parents
assert ok dir /sc/x/y/z
mkdir /sc/x/y2 parents
assert ok dir /sc/x/y2
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
dir | assert contains a.txt
cd /sc/d1
cd ..
dir | assert contains a.txt
cd -
assert ok read /sc/a.txt
assert fails cd /sc/a.txt
cd /

# ===== dir / find / tree as record producers (still referencing d1/d2) =====
echo ''
echo '===== dir / find / tree as record producers (still referencing d1/d2) ====='
dir /sc | where type=file | assert contains a.txt
dir /sc | where type=dir | assert contains d1
dir /sc | where type=file | assert lacks d1
dir /sc | select name | assert contains a.txt
dir / | where type=dir | assert contains sc
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

# A directory may not be moved INTO ITSELF or into its own subtree. Either would unlink the
# subtree from its parent while an entry inside it still points at it - a cycle, unreachable
# from the root. `drives check` rebuilds the free bitmap by WALKING the tree, so those blocks
# would be marked free and handed to the next allocation: a leak that becomes data loss.
#
# Guarded twice on purpose, and both are asserted here because they fail differently. The shell
# refuses it before sending (a better message, at the prompt). `fs` refuses it too, because a
# check in the caller is a convention and only a check in the OWNER is an enforcement - the tree
# and the bitmap rebuild that depends on it both belong to `fs`.
assert fails move /sc/dd1 /sc/dd1
assert fails move /sc/dd1 /sc/dd1/inner
assert fails move /sc/dd1 /sc/dd1/a/b/c
# ...while a SIBLING that merely shares a prefix is a perfectly good destination, which is the
# case a sloppy prefix test gets wrong.
mkdir /sc/dd1x
move /sc/dd1x /sc/dd1y
assert ok dir /sc/dd1y
assert fails dir /sc/dd1x
delete /sc/dd1y recursive

# ===== seal: content frozen, permanently =====
echo ''
echo '===== seal: a sealed file cannot be rewritten ====='
write /sc/frozen.txt original
# `yes` skips the [y/N] prompt: a confirm reads the console, which a script cannot answer.
seal /sc/frozen.txt yes
read /sc/frozen.txt | assert contains original
# Every write route must refuse it, and the content must be untouched afterwards.
assert fails write /sc/frozen.txt tampered
read /sc/frozen.txt | assert contains original
read /sc/frozen.txt | assert lacks tampered
# In a PIPE `dir` emits records, so the seal is a COLUMN rather than text: a separate `sealed`
# column, not a new `type` value, so existing `where type=file` queries keep their meaning.
dir /sc | where sealed=true | assert contains frozen.txt
dir /sc | where sealed=false | assert lacks frozen.txt
# EVERY WRITE ROUTE, not just `write`. `copy` goes through a DIFFERENT one (write_new + streaming
# write_at), and on 2026-09-18 that route had no seal check at all: it truncated the file and wrote a
# replacement entry with the flag CLEARED, so `copy` silently unsealed. Proving one route refuses does
# not prove the others, which is the whole reason this line exists.
write /sc/replacement.txt replacement-content
assert fails copy /sc/replacement.txt /sc/frozen.txt
read /sc/frozen.txt | assert contains original
read /sc/frozen.txt | assert lacks replacement-content
delete /sc/replacement.txt
# A sealed file is still a FILE to a query. If sealing changed an entry's type, every `where
# type=file` anyone has already written would silently stop matching it.
dir /sc | where type=file | assert contains frozen.txt
# Only a FILE can be sealed - a directory is refused, which is what keeps TYPE single-valued (there
# is no dir+sealed state to render).
assert fails seal /sc yes
assert fails seal /sc/no-such-file.txt yes
# `seal help` must answer (conventions rule 1). It did NOT until 2026-09-17 - the command was
# dispatched from a block that registered no help block, and the vocabulary checker caught it.
assert ok seal help
# A seal freezes CONTENT, not existence: renaming and deleting still work, and that is deliberate
# (see utilities/50_seal.md - an unremovable file is a denial of service, not a guarantee).
rename /sc/frozen.txt frozen2.txt
assert ok read /sc/frozen2.txt
delete /sc/frozen2.txt
assert fails read /sc/frozen2.txt
