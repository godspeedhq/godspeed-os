# online - probe the network path right now and verdict each step (system library).
#
# `net` reports the status SNAPSHOT; `online` EXERCISES the path: it shows the live
# status for context, then really resolves a name (DNS end to end) and really pings
# the internet (ICMP to 8.8.8.8), printing ok/FAIL per probe - so one command tells
# you WHICH layer broke: cable/router (the net block), name resolution, or the wider
# internet. Built on gsh's command-as-condition (`if <probe> { ok } else { FAIL }`)
# and the Result model: net dns / ping return Err when the probe fails. Pure
# composition - no new kernel or service surface (26.2).
if $argcount == 0 {
    echo '== online =='
    net
    if net dns example.com {
        echo 'dns       ok'
    } else {
        echo 'dns       FAIL'
    }
    if ping count 2 8.8.8.8 {
        echo 'internet  ok'
    } else {
        echo 'internet  FAIL'
    }
    echo '== end of online =='
} else if $arg1 == version {
    echo 'online 0.1.0'
    echo 'Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors.'
} else if $arg1 == help {
    echo 'online 0.1.0 - probe the network and verdict each layer'
    echo ''
    echo 'usage:'
    echo '  online                      net status, then live DNS + internet probes (ok/FAIL each)'
    echo '      e.g. online'
    echo '  online version              print the version'
    echo '  online help                 print this message'
} else {
    fail "unknown: online $arg1   (online help for usage)"
}
