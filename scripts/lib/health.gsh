# health - a one-shot GodspeedOS health dashboard (system library).
#
# Composes the system-info utilities into one view, so you get the whole picture
# without typing six commands. It is a gsh SCRIPT, not a Rust built-in: baked into
# the image and resolved PATH-like (just type `health`). The first citizen of the
# system library - proof that features grow by userspace composition, not new kernel
# or service surface. Add a script to scripts/lib/ and it becomes a command like this.
#
# Library scripts self-document THROUGH THEIR PARAMS (the user's convention): `$arg1` is
# checked for the universal `version` / `help` words before the body runs, in gsh itself.
if $argcount == 0 {
    echo '== GodspeedOS health =='
    date
    cores
    uptime
    mem
    net
    drives
    echo '== end of health =='
} else if $arg1 == version {
    echo 'health 0.1.0'
    echo 'Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors.'
} else if $arg1 == help {
    echo 'health 0.1.0 - one-shot system health dashboard'
    echo ''
    echo 'usage:'
    echo '  health                      date, cores, uptime, mem, net, drives in one view'
    echo '      e.g. health'
    echo '  health version              print the version'
    echo '  health help                 print this message'
} else {
    fail "unknown: health $arg1   (health help for usage)"
}
