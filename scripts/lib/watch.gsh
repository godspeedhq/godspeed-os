# watch - re-run a command every 2 seconds until q quits it (system library).
#
# The live view for anything: `watch mem` shows memory changing, `watch net` the link,
# `watch status` the service table. Each pass clears the screen and re-runs the command,
# so the display refreshes in place (a real live view on the framebuffer console, which
# has no scrollback). A failing command keeps being watched - that is the point of
# watching (waiting for it to come good); q is the exit either way.
#
# Built on the `wait` utility (a q-abortable pause): `if !wait 2 { break }` ends the loop
# the moment q arrives. Pure composition - no new kernel or service surface (26.2).
if $argcount == 0 {
    fail 'usage: watch <command ...>   (e.g. watch mem; q quits)'
} else if $arg1 == version {
    echo 'watch 0.1.0'
    echo 'Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors.'
} else if $arg1 == help {
    echo 'watch 0.1.0 - re-run a command every 2 seconds until q'
    echo ''
    echo 'usage:'
    echo '  watch <command ...>         clear + run the command, every 2s, q quits'
    echo '      e.g. watch mem'
    echo '      e.g. watch net'
    echo '  watch version               print the version'
    echo '  watch help                  print this message'
} else {
    loop {
        clear
        echo "watch: $args   (q to quit)"
        $args
        if !wait 2 { break }
    }
}
