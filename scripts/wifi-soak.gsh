# wifi-soak - keep a joined radio's link from being idle (backlog/64).
#
# The first idle soak lost the link six minutes after the join: the access point disassociated the
# station for INACTIVITY (802.11 reason 4). The radio was constantly awake, so it was not dozing through
# the access point's poll - and a link nothing ever sends on looks gone. This sends ONE echo to the
# gateway each minute, the smallest trickle that answers whether traffic is what keeps the link up.
#
# usage:   run /wifi-soak.gsh <gateway-ip>        e.g. run /wifi-soak.gsh 192.168.11.1
# stop:    q (during the minute's wait)
#
# Same shape as the `watch` library command, without its screen clearing, so the serial log stays a
# readable record of every echo.
if $argcount == 0 {
    fail 'usage: run /wifi-soak.gsh <gateway-ip>   (q stops it)'
}
echo "wifi-soak: one echo to $arg1 per minute until q"
loop {
    ping count 1 $arg1
    if !wait 60 { break }
}
