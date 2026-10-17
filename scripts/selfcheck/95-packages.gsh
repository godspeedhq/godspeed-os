# GodspeedOS self-check, part 11 of 11: packages - the whole lifecycle, against the real registry
#
# One of the parts `selfcheck` runs, in the order they are listed in `SELFCHECK_PARTS`
# (`services/shell/src/main.rs`). `selfcheck packages` runs THIS part alone. The command is
# `utilities/61_godspeed.md`; `osdev test godspeed` checks the printed answers in QEMU, and this part runs
# the same lifecycle on whatever machine it is on - every board, over its own network.
#
# IT TOUCHES ONLY ITS OWN PACKAGES. Everything here is a `godspeed/selfcheck-*` package, published for this
# part alone (the projects are `selfcheck-service`, `-utility`, `-app` and `-driver` beside the OS). The
# `hello-*` ones are examples an operator may really install, so this part never installs, upgrades or
# removes them, nor anything else it did not put there.
#
# A LEFTOVER IS CLEARED, AND SAID; NOTHING ELSE IS REPAIRED. A run that was interrupted can leave a
# selfcheck package installed or fetched. Those are this part's own, so it uninstalls and removes them
# first, out loud, and then tests from nothing. It repairs nothing else - not the trust record, not
# another package, not the installed list: a self-check that fixes what it finds hides the very faults it
# exists to report (CLAUDE.md 26.7). Anything broken outside its own packages is a FAIL, left as found.
#
# A SKIP IS NOT A PASS (part 1). The lifecycle needs the registry, so with no lease, or a registry that does
# not answer, every check below is skipped by name. The driver drives QEMU's `edu` test device, which no
# board has, so on a board its start is skipped while its verify still runs.
#
# WHAT IS ASSERTED, AND WHAT IS ONLY REPORTED. `godspeed upgrade` and `install` are actions, and an action
# refuses to start a pipe (utilities/0_conventions), so a script cannot read the line that says whether a
# replacement was hot ("replaced live, with no restart") or cold ("stopped and started"). It is printed
# above each check for whoever reads the run; what is asserted is the state after: the version installed,
# and running. `osdev test godspeed` asserts the hot and the cold lines themselves.

echo ''
echo '===== packages: clear what an interrupted run left of this part own packages ====='
# Under `if`, so "not installed here" is no failure: the point is only that none is left.
for p in selfcheck-service selfcheck-utility selfcheck-app selfcheck-driver {
    if godspeed uninstall godspeed/$p yes { echo "packages: cleared an installed godspeed/$p left by an earlier run" }
    if godspeed remove godspeed/$p { echo "packages: cleared a fetched godspeed/$p left by an earlier run" }
}
godspeed list installed | where package contains selfcheck- | assert empty
godspeed list fetched | where package contains selfcheck- | assert empty

# ---- the gate: is the registry reachable? ----------------------------------------------------
#
# Staged through a file: gsh does not capture a pipeline into a `for` (part 3 says why, and what it cost),
# and `count` counts data rows, so a verified TLS row is 1 and anything else is 0. Root is used because the
# part may run alone, before any other part has made its folders.
godspeed check | where item=tls | where value=verified | count | write /gsreg.txt
let mut reg = 0
for line in (read /gsreg.txt) { if $line > 0 { reg = 1 } }
delete /gsreg.txt

if $reg > 0 {
    echo ''
    echo '===== packages: the registry lists them, and each verifies from the disk ====='
    godspeed search selfcheck | assert contains godspeed/selfcheck-service
    godspeed search selfcheck | assert contains godspeed/selfcheck-driver
    godspeed list versions godspeed/selfcheck-service | assert contains 1.0.0
    godspeed list versions godspeed/selfcheck-service | assert contains 1.2.0
    for p in selfcheck-utility selfcheck-app selfcheck-driver {
        assert ok godspeed fetch godspeed/$p
    }
    assert ok godspeed fetch godspeed/selfcheck-service@1.1.0
    assert ok godspeed fetch godspeed/selfcheck-service@1.2.0
    assert ok godspeed fetch godspeed/selfcheck-service@1.0.0
    # Every copy checks from the disk alone - signature, key, contract, binary - and none is damaged.
    godspeed verify | where package contains selfcheck- | where status=DAMAGED | assert empty
    godspeed verify | where package contains selfcheck- | assert contains intact

    echo ''
    echo '===== packages: a utility runs when typed, is given its arguments, and pipes ====='
    assert ok godspeed install godspeed/selfcheck-utility yes
    selfcheck-utility abc | assert contains given: abc
    selfcheck-utility | assert contains given: nothing
    godspeed list installed | where package=godspeed/selfcheck-utility | assert contains utility

    echo ''
    echo '===== packages: an app has the screen, and gives it back ====='
    assert ok godspeed install godspeed/selfcheck-app yes
    # The app closes itself at once; that this script goes on to the next line is the shell having the
    # console back.
    assert ok selfcheck-app xyz
    godspeed list installed | where package=godspeed/selfcheck-app | assert contains app

    echo ''
    echo '===== packages: a service is installed, upgraded while it runs, and replaced by an older one ====='
    assert ok godspeed install godspeed/selfcheck-service@1.1.0 yes
    godspeed list installed | where package=godspeed/selfcheck-service | assert contains running
    # 1.1.0 and 1.2.0 both declare `handover`: printed above as "replaced live" where this machine has a
    # hardware random source for the keys, and as a stop and start where it has none.
    assert ok godspeed upgrade godspeed/selfcheck-service yes
    godspeed list installed | where package=godspeed/selfcheck-service | assert contains 1.2.0
    godspeed list installed | where package=godspeed/selfcheck-service | assert contains running
    # 1.0.0 does not declare it: a stop and start, checked before the running one is stopped.
    assert ok godspeed install godspeed/selfcheck-service@1.0.0 yes
    godspeed list installed | where package=godspeed/selfcheck-service | assert contains 1.0.0
    godspeed list installed | where package=godspeed/selfcheck-service | assert contains running

    echo ''
    echo '===== packages: a driver drives its device, where there is one ====='
    # QEMU's `edu`. On a board the start is refused for want of the device and the install puts back what
    # was there (nothing): skipped, because no board has the device - the verify above still ran.
    if godspeed install godspeed/selfcheck-driver yes {
        godspeed list installed | where package=godspeed/selfcheck-driver | assert contains running
    } else {
        skip 'packages: selfcheck-driver - its device (QEMU edu) is not on this machine; it verified above'
    }

    echo ''
    echo '===== packages: what is refused ====='
    # A version that was never published, and a package that does not exist.
    assert fails godspeed install godspeed/selfcheck-service@9.9.9 yes
    assert fails godspeed fetch godspeed/selfcheck-nothing

    echo ''
    echo '===== packages: uninstalled and removed, leaving nothing ====='
    for p in selfcheck-service selfcheck-utility selfcheck-app selfcheck-driver {
        if godspeed uninstall godspeed/$p yes { echo "packages: uninstalled godspeed/$p" }
        assert ok godspeed remove godspeed/$p
    }
    godspeed list installed | where package contains selfcheck- | assert empty
    godspeed list fetched | where package contains selfcheck- | assert empty
    # A uninstalled utility is no longer a command.
    assert fails selfcheck-utility abc
} else {
    skip 'packages - the registry is not reachable from here (no network, or it did not answer); not a failure'
}
