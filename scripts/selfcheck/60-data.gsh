# GodspeedOS self-check, part 7 of 9: byte pipes, records, json, fsck, scrub, file-as-capability, fmt, churn, jobs
#
# One of the parts `selfcheck` runs, in the order they are listed in `SELFCHECK_PARTS`
# (`services/shell/src/main.rs`). `selfcheck data` runs THIS part alone; `selfcheck` runs them all
# and prints one tally for the whole suite.
#
# They are separate files because a baked script is indexed with u16 offsets and the single file had
# reached 65,139 bytes of a 65,536 ceiling (`backlog/47`). Each part now carries its own budget - and
# its own 256-statement detail budget, so the report no longer stops naming statements part way.
#
# No part may declare a `fn` or a `let` another part uses: each is interpreted on its own, with a
# fresh variable table. What DOES carry across is the working directory and the disk.

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

# ===== churn: sustained writes, and the evidence a POWER CUT leaves behind =====
#
# THE FIRST BLOCK IS THE IMPORTANT ONE, and it is why churn belongs in here at all.
#
# `churn` exists to be INTERRUPTED. The commit-to-checkpoint window is sub-millisecond, so the only
# way a human ever lands in it is to write thousands of transactions and pull the cord. When that
# happens the machine reboots with `/churn` still on disk, holding the only evidence of whether
# recovery held - and that evidence is destroyed by the next churn that overwrites it.
#
# So: if a previous run is still there, VERIFY IT BEFORE STARTING A NEW ONE. After a cut the whole
# post-mortem becomes one command - boot, run selfcheck, and it tells you whether any file holds a
# mix of two generations. Forgetting to run `churn verify` before the next churn is how that answer
# gets lost, and it is an easy thing to forget at a bench with the lid off.
#
# What selfcheck CANNOT do is the cut itself. No self-test can pull its own power, so a deliberate
# recovery test is still `churn <seconds>` and a hand on the cord - this automates the verdict, not
# the fault.
echo '===== churn: sustained writes, and any evidence left by an earlier power cut ====='
if dir /churn {
    echo 'selfcheck: an earlier churn is still on disk - verifying it BEFORE it is overwritten'
    if churn verify {
        echo 'PASS  churn - the earlier run holds no torn file (if it was cut, recovery held)'
    } else {
        fail 'churn: a file from the earlier run holds a MIX of two generations - DATA INTEGRITY, report the serial'
    }
}
# ...then a short fresh run, as an ordinary exercise of the write path under sustained load.
# Deliberately brief: this is the integrity half, and a long churn here would make every selfcheck
# slow for a test whose interesting half needs a human anyway.
if churn 4 {
    if churn verify {
        echo 'PASS  churn - thousands of transactions, every file holds one generation end to end'
    } else {
        fail 'churn: a file was torn by an UNINTERRUPTED run - that is a write-path fault, not a power cut'
    }
    churn reset
} else {
    skip 'churn - no storage to churn on this machine; not a failure'
}

# ===== job control (utilities/55_background.md; backlog/40 on why this is short) =====
echo ''
echo '===== job control: background / jobs / foreground ====='
assert fails background selfcheck
assert fails foreground 99
assert fails jobs quit 99
background drives scrub
jobs | assert contains 'drives scrub'
