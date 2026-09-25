# GodspeedOS self-check, part 2 of 9: the assert forms, self-documentation, system info, introspection, observe
#
# One of the parts `selfcheck` runs, in the order they are listed in `SELFCHECK_PARTS`
# (`services/shell/src/main.rs`). `selfcheck meta` runs THIS part alone; `selfcheck` runs them all
# and prints one tally for the whole suite.
#
# They are separate files because a baked script is indexed with u16 offsets and the single file had
# reached 65,139 bytes of a 65,536 ceiling (`backlog/47`). Each part now carries its own budget - and
# its own 256-statement detail budget, so the report no longer stops naming statements part way.
#
# No part may declare a `fn` or a `let` another part uses: each is interpreted on its own, with a
# fresh variable table. What DOES carry across is the working directory and the disk.

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
assert ok dir help
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
assert ok whatis dir
whatis dir | assert contains built-in
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
    skip 'date - the clock is not set on this machine (no RTC, no network); not a failure'
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
