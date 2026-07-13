# busiest - the service table ranked by resource use, biggest first (system library).
#
# "Who is busiest?" - the question an operator actually asks. One record pipe:
# `status` produces the live service table (mem/queue/restarts columns), `sort
# reverse <col>` ranks it descending. Default column is mem (footprint); pass a
# column to rank by churn or backlog instead:
#
#   busiest             by memory (who is heaviest)
#   busiest restarts    by deaths recovered (who churns)
#   busiest queue       by endpoint backlog (whose mailbox is filling)
#
# Pure composition of the record model (26.2; docs/records.md) - like `size`,
# this script is a NAMED question over pipes you could type yourself.
if $argcount == 0 {
    status | sort reverse mem
} else if $arg1 == version {
    echo 'busiest 0.1.0'
    echo 'Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors.'
} else if $arg1 == help {
    echo 'busiest 0.1.0 - the service table ranked by resource use, biggest first'
    echo ''
    echo 'usage:'
    echo '  busiest [column]            rank services by mem (default), restarts, or queue'
    echo '      e.g. busiest'
    echo '      e.g. busiest restarts'
    echo '  busiest version             print the version'
    echo '  busiest help                print this message'
} else {
    status | sort reverse $arg1
}
