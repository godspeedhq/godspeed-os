# GodspeedOS self-check, part 3 of 9: hw-enumerator, the lifecycle guardrails, and trace
#
# One of the parts `selfcheck` runs, in the order they are listed in `SELFCHECK_PARTS`
# (`services/shell/src/main.rs`). `selfcheck hardware` runs THIS part alone; `selfcheck` runs them all
# and prints one tally for the whole suite.
#
# They are separate files because a baked script is indexed with u16 offsets and the single file had
# reached 65,139 bytes of a 65,536 ceiling (`backlog/47`). Each part now carries its own budget - and
# its own 256-statement detail budget, so the report no longer stops naming statements part way.
#
# No part may declare a `fn` or a `let` another part uses: each is interpreted on its own, with a
# fresh variable table. What DOES carry across is the working directory and the disk.

# ===== hw-enumerator: hardware discovery in USERSPACE (step D2) =====
echo ''
echo '===== hw-enumerator: userspace PCI discovery + its narrow authority ====='
# NOT EVERY MACHINE HAS PCI. This service exists on x86 and on the Pi 4; the Pi 2's peripherals hang
# off a memory-mapped bus with no PCI at all, so there is nothing here to enumerate and no service to
# ask. Probe for it and SKIP OUT LOUD, the same way the clock check does above - a silent skip is a
# test that has quietly stopped testing, and asserting it unconditionally would fail the Pi 2 for
# lacking hardware rather than for anything being wrong.
# The probe has to survive BOTH machines, and getting it wrong is quiet rather than loud - which is
# why it is written this way and not the obvious way.
#
# `for line in (status | where ...)` is the shape that reads best and it does NOT work: gsh refuses to
# capture a PIPELINE (bounded stack - it says so). The refusal made the loop body never run, `hwe`
# stayed 0, and the suite cheerfully printed SKIP on a machine that HAS the service. A probe that
# fails safe-looking is worse than one that fails loudly, so it is staged through a file the way the
# error message and every other capture in this suite do it.
#
# `count` is what makes the answer unambiguous: it counts DATA rows, not the header, so a match is 1
# and no match is 0. Reading the raw table instead would see a header row either way and always say
# "present". Root is used for the staging file because `/sc` is not created until much later in this
# script, and writing into a missing parent fails.
status | where name contains hw-enumerator | count | write /hwe.txt
let mut hwe = 0
for line in (read /hwe.txt) { if $line > 0 { hwe = 1 } }
delete /hwe.txt
if $hwe > 0 {
    # It is alive, and it is where the supervisor put it.
    status | where name contains hw-enumerator | assert contains hw-enumerator
    # It survived to serve: a service that logged its scan and then died would still be "in the
    # table" for a moment, so assert the state the supervisor keeps it in.
    status | where name contains hw-enumerator | assert lacks Dead
    # The kernel directory resolves it by name - the property that makes it reacquirable (§14.3).
    trace endpoints | assert contains hw-enumerator

    # THE AUTHORITY, which is the part actually worth pinning. `pci_cfg` is a hardware capability
    # granted to a userspace service, so its SHAPE is a security claim and not an implementation
    # detail: one configuration READ and nothing else. `caps` names the resource rather than printing
    # it as an anonymous id, so the claim is readable here at all.
    caps hw-enumerator | assert contains pci_cfg
    caps hw-enumerator | where resource=pci_cfg | assert contains read
    # THE REGRESSION GUARD. Config space holds every BAR and every command register, so write
    # authority over it is write authority over every device on the bus - there is no narrower form,
    # because the target is chosen by data rather than by the interface. It was minted READ|WRITE
    # once, before the write operation was removed. If anyone re-adds that right, this line fails and
    # says why, which is the whole point of writing it down as a test rather than as a comment.
    caps hw-enumerator | where resource=pci_cfg | assert lacks write
    # And it holds no authority it has no business holding: discovery does not spawn, kill, or reboot.
    caps hw-enumerator | assert lacks service_control
    caps hw-enumerator | assert lacks reboot
    caps hw-enumerator | assert lacks image_spawn
} else {
    skip 'hw-enumerator - this machine has no PCI to enumerate (Pi 2); not a failure'
}

# `caps` must NAME a well-known resource, never print it as an anonymous number.
#
# THIS ASSERTS A PIPED `caps`, and for a while that was not the same thing as the `caps` a person
# reads. There were TWO copies of the naming table - one in the record producer, one in the console
# renderer - so naming all sixteen resources fixed the piped view while the console view kept
# printing `endpoint#8` for `reboot`. The piped test passed the whole time. The console output cannot
# be captured (piping is what switches renderers), so no assertion can ever guard it directly; the
# only real fix was to delete the duplicate so both views read from ONE table. This line therefore
# guards the naming for both, and that is only true while that remains a single table (§26.4). Ten of the sixteen
# used to fall through to an `endpoint#N` fallback, which reported (for instance) the shell's authority
# to reboot the machine as "endpoint#8" - a label naming the wrong KIND of thing, so a reader could not
# tell real authority from an ordinary IPC endpoint. Authority has to be readable (§26.9). The shell
# holds `reboot`, so it is the honest witness for this on every machine.
caps shell | assert contains reboot
caps shell | assert lacks endpoint#8

# ===== lifecycle guardrails + supervisor recovery (safe, deterministic) =====
echo ''
echo '===== lifecycle guardrails + supervisor recovery (safe, deterministic) ====='
# The shell COMMAND refuses spawn/restart of the supervisor (the recovery authority) - a command-layer
# hygiene check (no duplicate or self-restart of the restart authority), NOT "can't recover". `kill
# supervisor` is NOT refused: the supervisor is kernel-restartable (Phase 6), so a kill SUCCEEDS and it
# revives - asserted positively just below. `kill shell` is NOT tested: the shell is restartable now, so
# it would kill this run.
assert fails spawn supervisor
assert fails-with Denied spawn supervisor
# POSITIVE (Test 15 / Commandment V): kill the supervisor and assert it comes back to LIFE. `chaos
# kill-storm supervisor 1` captures its generation, kills it, waits on the generation BUMP (the truth,
# bounded - never a fixed sleep), and returns Ok only if a HIGHER generation appears; it prints
# "killed gen N -> recovered gen M". A no-show returns Err, so `assert ok` fails LOUDLY.
assert ok chaos kill-storm supervisor 1
assert fails spawn nosuchservice
assert fails-with Unknown spawn nosuchservice
assert fails kill nosuchservice
assert fails restart supervisor
assert fails restart nosuchservice

# ===== trace: LIVE kernel state - what is stuck right now =====
echo ''
echo '===== trace: blocked / chain / deps / endpoints ====='
# Every view must ANSWER. A healthy machine has nothing blocked, and saying so is the correct answer -
# an empty result would not be (idle on your own endpoint is not stuck).
# `blocked`, `chain` and `status` print a report rather than records, so they are not pipe producers -
# `assert ok` is the form for those, and it is not a weaker check here: each returns Err when it cannot
# answer, which is exactly the failure being guarded against.
# BOTH commands answer `help` and `version`, per conventions rules 1, 5 and 6.
assert ok trace help
assert ok trace version
assert ok events help
assert ok events version
assert ok trace blocked
assert ok trace chain shell
# `deps` reads the LIVE capability table, so the shell must show the peers it actually holds.
trace deps shell | assert contains fs
trace deps fs | assert contains block-driver
# The tree and the record stream are the same data - the grid header names every filterable column.
trace deps shell | to grid | assert contains parent
trace deps shell | where peer contains fs | assert contains fs
# The endpoint inventory, and the inverse lookup it exists to feed.
trace endpoints | assert contains events
trace endpoints | where name contains fs | assert contains fs
# The ring itself answers, and reports its drop count (a silent loss is the bug - invariant 12).
assert ok events status
# The shell has been calling `fs` throughout this suite, so the ring holds real traffic.
events ipc | assert contains outcome