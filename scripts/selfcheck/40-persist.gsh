# GodspeedOS self-check, part 5 of 9: events persist: capture to disk, through a service that is NOT events
#
# One of the parts `selfcheck` runs, in the order they are listed in `SELFCHECK_PARTS`
# (`services/shell/src/main.rs`). `selfcheck persist` runs THIS part alone; `selfcheck` runs them all
# and prints one tally for the whole suite.
#
# They are separate files because a baked script is indexed with u16 offsets and the single file had
# reached 65,139 bytes of a 65,536 ceiling (`backlog/47`). Each part now carries its own budget - and
# its own 256-statement detail budget, so the report no longer stops naming statements part way.
#
# No part may declare a `fn` or a `let` another part uses: each is interpreted on its own, with a
# fresh variable table. What DOES carry across is the working directory and the disk.

# ===== events persist: capture to disk, via a service that is NOT `events` =====
# `recorder` drains `events` and writes the file. It is spawned ON DEMAND by the line below and is
# absent from the kernel managed-service lists, so this whole feature costs the kernel nothing.
#
# Why it is a separate service at all: a file write BLOCKS on a reply, and a blocked `events` stops
# draining its endpoint and drops the very events worth capturing. Here the blocking is harmless -
# nothing depends on `recorder`, so a stalled disk stalls it alone and the volatile window survives.
# NOT asserted: the exact idle STATE. On the first selfcheck of a boot `recorder` has never been
# spawned and status says "not running"; on a second run it is already there and says "idle". Both mean
# "not capturing", and asserting one of them made the suite pass only on a fresh boot - the same
# re-runnability trap as the `delete /sc` line above. It must ANSWER; which flavour of not-capturing it
# is carries no information.
assert ok events persist status
assert ok events persist start /sc/cap.log 256KiB
# THE CAPTURE PREPARES BEFORE IT RECORDS. `start` answers at once and the extent is made readable in
# the recorder's own loop, so nothing blocks the prompt on device I/O - which is what broke on the Pi 4,
# where filling 4 MiB over USB took longer than the caller was willing to wait.
#
# SO WAIT ON THE TRUTH, NOT ON A CLOCK (Commandment VIII). This was `wait 3` racing a variable
# pre-fill and it lost intermittently - `backlog/36` has the post-mortem. Staged through a file
# because gsh refuses to capture a pipeline (see the hw-enumerator probe above); `count` counts DATA
# rows, so a match is 1 and no match is 0.
let mut capready = 0
for i in range 30 {
    if $capready < 1 {
        events persist status | where state=recording | count | write /sc/pr.txt
        for line in (read /sc/pr.txt) { if $line > 0 { capready = 1 } }
        if $capready < 1 { wait 1 }
    }
}
delete /sc/pr.txt
if $capready > 0 {
    events persist status | assert contains recording
} else {
    fail 'events persist: the capture never reached `recording` in 30s - the extent pre-fill did not finish'
}
# THE RECORDER IS ALIVE - assert it before trusting anything below (`backlog/23`). Every assertion
# from here to `capacity` reads the SHELL's rendering of a status line, and that rendering does not
# need `recorder` to exist. On the Pi 4 it crashed mid-section and four of them passed over the
# corpse, so `ran 461, failed 0` slept through a service fault. Same pattern as hw-enumerator above.
status | where name contains recorder | assert contains recorder
status | where name contains recorder | assert lacks Dead
# BOUNDED AT TWO FILES, forever. The cap is not a policy the recorder enforces by counting - `fs`
# allocates a file's whole extent up front, so the size is fixed when the capture starts and total
# disk use is twice that, no matter how long it runs. A forgotten capture cannot fill a disk.
#
# It ROTATES rather than stopping, because stopping keeps the wrong half: a fixed file that stops when
# full preserves the START of a session and discards everything after - and the reason to run a capture
# for an hour is almost always to catch something at the END.
events persist status | assert contains rotations
# COVERAGE IS MEASURED, not the number that was asked for: `covers` is what the budget buys at the
# rate this machine is actually logging at, so a duration is a target and never a promise.
events persist status | assert contains covers
events persist status | assert contains kib_day
# EVERY BUDGET CARRIES A UNIT. A bare number would have to mean megabytes or minutes by
# convention, and `16m` cannot be read as either without guessing - so nothing is bare, and a
# token without a unit is unambiguously a service name.
assert fails events persist start /sc/bad.log 64MB
events persist status | to json | assert contains capacity
dir /sc | assert contains cap.log
# STICKY: recorded in a plain-text marker the shell reads at the next boot. Plain text on purpose -
# `read /persist.conf` shows exactly what will happen, which is the difference between a setting and a
# surprise. A capture that resumed silently forever because someone forgot is the hazard here.
assert ok events persist start /sc/sticky.log 256KiB sticky
read /persist.conf | assert contains /sc/sticky.log
# Same wait as above: a fresh capture PREPARES before it records.
wait 3
events persist status | assert contains recording
# AN EXPLICIT STOP STAYS STOPPED. Leaving the marker would make the one command that means "enough"
# the one that did not take, and the capture would return at the next boot.
assert ok events persist stop
assert fails read /persist.conf
assert ok events persist stop
# THE CAPTURE MUST BE READABLE WITH `read`, which is not a given and was not true at first.
# `OP_WRITE_NEW` allocates the extent but writes no data blocks, so everything past the last chunk
# written had a stored CRC of zero and `fs` correctly refused the whole file:
#   fs: data block CRC mismatch at lba 4229 (stored 0x00000000, actual 0x0fbb6d54) - refusing
# A capture that cannot be read back is not a capture. The file is zero-filled on creation now, and
# this is the assertion that would have caught it.
events persist start /sc/tiny.log 256KiB
wait 3
events persist stop
# BOTH IN THE ON-DISK FORM `owner: text`. The header and footer are the only two lines the recorder
# writes directly; every other line arrives from `events` and is converted on the way in. They used to
# be emitted in the WIRE form, putting a 0x1F control byte in the first and last line of every capture.
# The old assertion here was `contains recorder:`, which passed anyway on a fast machine - the
# recorder's own log line came back through the drain and supplied a `recorder:` by luck. The Pi 2 was
# slow enough to stop the capture before that tick fired, and failed. Assert the WHOLE header and
# footer, so the form is pinned and no accident can satisfy it.
read /sc/tiny.log | assert contains recorder: capture started
read /sc/tiny.log | assert contains recorder: capture ended

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
# TWO COMMANDS, TWO SOURCES - and each refuses the other's views by NAME, so a reader who had the
# right question and the wrong command is told which one to use.
assert fails events deps shell
assert fails trace metrics

# Every view refuses an unknown subject LOUDLY rather than answering with nothing.
assert fails trace chain nosuchsvc
assert fails trace deps nosuchsvc
assert fails trace endpoint notanumber
assert fails events nosuchview

# THE SINK IS RESTARTABLE, AND THE READER MUST SURVIVE IT. `events` holds the trace ring; killing it
# invalidates every cached capability to it. Without a reacquire the shell keeps a stale generation
# forever, so `events ipc` reports a live service as unreachable and every emission logs a kernel
# gen-mismatch - which is exactly what a chaos storm produced on hardware (cap 985 vs record 1025)
# before this was fixed. Kill it, then prove the views still work (14.3: reacquire by name, retry).
assert ok chaos kill-storm events 1
assert ok events status
events ipc | assert contains outcome
trace endpoints | assert contains events
# THE METRIC TABLE IS VOLATILE AND DIES WITH THE SINK. That is correct, not a gap: a restart is a
# re-init and not a resume (14.2), and it is exactly why `events` must never acquire a durable-storage
# dependency - a service that reports a storage failure must not be downstream of storage. What has to
# survive the kill is the VIEW, which answers again and refills as services publish. The one thing it
# can never report is its OWN death; the supervisor's death notification and the kernel's unconditional
# serial write do that, and both sit beneath it.
assert ok events metrics
