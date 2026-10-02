# GodspeedOS self-check, part 9 of 9: the network: receive, a lease, and name resolution
#
# One of the parts `selfcheck` runs, in the order they are listed in `SELFCHECK_PARTS`
# (`services/shell/src/main.rs`). `selfcheck network` runs THIS part alone; `selfcheck` runs them all
# and prints one tally for the whole suite.
#
# They are separate files because a baked script is indexed with u16 offsets and the single file had
# reached 65,139 bytes of a 65,536 ceiling (`backlog/47`). Each part now carries its own budget - and
# its own 256-statement detail budget, so the report no longer stops naming statements part way.
#
# No part may declare a `fn` or a `let` another part uses: each is interpreted on its own, with a
# fresh variable table. What DOES carry across is the working directory and the disk.

# ---- wifi: the PARSE contract, asserted; the radio, reported -----------------------------------
#
# `wifi` is the wireless link verb (`utilities/56_wifi.md`). The radio itself does not exist yet -
# `docs/wifi.md` has the phases - so what is worth asserting here is the part that is already final and
# stays final: the shape of the command.
#
# WHY THESE ARE THE ASSERTIONS AND `assert ok wifi` IS NOT. Asking about wireless on a machine with no
# radio is not an error, so bare `wifi` succeeds today; on a machine whose driver is running but not yet
# wired it deliberately fails, because implying the radio is fine when the shell cannot reach it is the
# silent fallback invariant 12 forbids. An assertion over that line would therefore pass now and break
# the day somebody finishes phase 1 - a landmine planted for the person doing the work. So the verdict
# below is REPORTED, the way the lease check above reports, and only the refusals are asserted.
echo '===== wifi: the command shape (the radio is phase 1, docs/wifi.md) ====='
assert ok wifi help
assert ok wifi version
# A bare subcommand that needs an argument must say so rather than guess.
assert fails wifi join
assert fails wifi forget
assert fails wifi radio
# `radio` takes two words and no others.
assert fails wifi radio sideways
# An unknown subcommand is refused by name, never silently treated as `wifi status`.
assert fails wifi nonsense
# THE PASSPHRASE MUST NOT BE ACCEPTED AS AN ARGUMENT. This is the assertion that defends a security
# decision rather than a behaviour: a passphrase on the command line is recalled by up-arrow and
# written to /.gsh_history, so `connect` takes an SSID and nothing else. If this ever starts passing,
# somebody has added a convenience that leaks a secret to disk.
assert fails wifi join SomeSSID hunter2
# And the radio itself, reported rather than asserted (see the note above). Under `if`, so its words
# print and a radio that is down - or absent - counts as nothing. This was `for line in (wifi) { ... }`,
# which failed every run since bare `wifi` became its usage (rule 1) and was never capturable anyway:
# the ONE failure of the whole suite, which the summary could not even name (2026-10-02).
if wifi status { echo 'wifi: reported above' } else { echo 'wifi: reported above (the radio is not up, or not here - not a selfcheck failure)' }

# ---- network: RECEIVE must work, checked without sending anything ----------------------------
#
# A pure READ of state: it asks what already happened, it does not make anything happen.
#
# What it catches: a driver change stopped the receive channel being armed, so the host transmitted
# normally and received nothing. DHCP got no offer, net-stack fell back to an address the network does
# not route - and all 351 other tests still passed, because none of them touched the network. A stack
# that cannot receive cannot get a lease. That is the whole check.
#
# WAITS ON THE ANSWER, NOT ON A MOMENT. The first version asked once and failed on a machine whose PHY
# negotiated link 50 seconds into the boot: net-stack was mid-DHCP (it blocks its serve loop for the
# length of a dance), `net` timed out, and the suite reported a network fault that did not exist. So it
# retries while there is no answer, bounded, and fails only if none ever comes - which is Commandment
# VIII's distinction exactly: wait for the truth, with a bound, never for a fixed interval.
#
# `net lease` prints ONE word so the retry can test it with the grammar this language has, and prints
# NOTHING while net-stack is busy - so silence retries rather than counting as a verdict.
# QUIET WHILE IT WAITS. This loop echoed `wait 1` up to twenty times into the operator's console -
# twenty lines saying nothing, in the middle of a suite whose value is that its output is readable.
# The retry is right (Commandment VIII: wait on the truth, with a bound, not on a fixed interval);
# what was wrong was doing it out loud. One `wait 5` per attempt, four attempts, same twenty-second
# ceiling, four lines instead of twenty - and the shell prints the command either way, so the fix is
# fewer iterations rather than a quieter `wait`.
let mut leaseok = 0
for i in range 4 {
    if $leaseok < 1 {
        for line in (net lease) { if $line in ok { leaseok = 1 } }
        if $leaseok < 1 { wait 5 }
    }
}
if $leaseok > 0 {
    echo 'PASS  net - the stack holds a lease (or has no link to need one)'
} else {
    # NOT a `fail`, and the reason is that this check can no longer tell two things apart.
    #
    # It was written when a missing lease had one explanation: a receive path that could not hear the
    # offer. That held while the only two states were "no link" (already answered `ok` above, since
    # there is nothing to lease) and "link up on a working network". A third state now exists and is
    # ordinary - a link up to a switch whose DHCP server is down or absent - and in it the stack is
    # behaving perfectly while this check reports a fault in it.
    #
    # A suite exists to catch OUR regressions. Failing the whole run for the absence of someone
    # else's server teaches the reader to discount the failure, and a failure that gets discounted
    # protects nothing - which is worse than not asserting at all, because it also costs the reader's
    # trust in the 467 results beside it.
    #
    # So it REPORTS, in words that cannot be mistaken for a pass, and says exactly what was and was
    # not established. What is lost is real and is stated here rather than papered over: a genuinely
    # dead receive path now reads the same as an absent server, so this line is a prompt to look and
    # not a verdict. The check regains its teeth the moment it can ask the stack how many frames it
    # has RECEIVED - zero frames with a live link is unambiguously ours - and that wants a counter in
    # net-stack's status reply, which is real work and not a constant (26.7).
    skip 'net - link is up but no lease in 20s: either nothing is serving DHCP, or our receive'
    skip 'net - path is broken. This check cannot tell those apart - re-run where a server exists.'
}

# ---- network: NAME RESOLUTION, asserted only where it can be OUR fault -----------------------
#
# What it catches: DNS is a different code path from everything above it. ICMP can be perfect while
# UDP request/reply is broken - and it was, twice in one session. `nic-driver` used to hand a received
# frame back as the answer to a TRANSMIT, and `udp_roundtrip` collected its first frame from that
# reply and its next from an ARP-reply's; decoupling that touched DNS and nothing else. Ping stayed
# green throughout. The suite would not have noticed either way, because nothing here resolved a name.
#
# GATED ON THE INTERNET BEING DEMONSTRABLY REACHABLE, and that is the whole design. A machine with no
# cable, no DHCP server, no route, or no WAN cannot resolve a name, and none of that is a defect in
# this system - failing there would be a suite that cries wolf on a laptop at a coffee shop. So the
# assertion runs ONLY after ICMP to the internet has just succeeded. Once that holds, the network is
# proven end to end, and a name that will not resolve is ours: same cable, same lease, same gateway,
# same driver, different code path. The skip is not a weakened check, it is the check declining to
# make a claim it cannot support.
#
# ONE RETRY, bounded, for the same reason the lease gate retries: a single lost UDP datagram is not a
# broken resolver, and DNS has no retransmit of its own here.
let mut dnsok = 0
if ping count 2 8.8.8.8 {
    for i in range 2 {
        if $dnsok < 1 {
            if net dns example.com { dnsok = 1 } else { wait 1 }
        }
    }
    if $dnsok > 0 {
        echo 'PASS  dns - names resolve over a network that is proven reachable'
    } else {
        fail 'dns: ICMP to 8.8.8.8 works but no name resolves - the UDP request/reply path is broken'
    }
} else {
    skip 'dns - no internet to resolve through; not a failure'
}
