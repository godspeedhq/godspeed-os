# GodspeedOS self-check, part 10 of 11: ask - plain questions, answered from what the services know
#
# One of the parts `selfcheck` runs, in the order they are listed in `SELFCHECK_PARTS`
# (`services/shell/src/main.rs`). `selfcheck ask` runs THIS part alone. The command is
# `utilities/60_ask.md`; `osdev test shell` checks the printed answers, and this part checks the RECORDS
# on whatever machine it runs on - every board, not only QEMU.
#
# Every check pipes. Piped, an answer is one record table (label, source, rule, text) with a `status`
# row - `answered`, or the refusal (`action`, `not-understood`, ...) - and a `verdict` row, so a check
# reads the field it means instead of a sentence's wording. Nothing here depends on what this machine
# HAS: a verdict that differs by board (healthy or degraded, which radio, which network) is asserted
# only to be an answer, never to be one answer.

echo ''
echo '===== ask: the questions it understands are answered ====='
ask "is my system healthy?" | where label=status | assert contains answered
ask "what failed?" | where label=status | assert contains answered
ask "is the network up?" | where label=status | assert contains answered
# The suite wrote files in earlier parts, so a disk is mounted - and mounted is all ask may claim:
# nobody read every block, and what fs does not report is said to be unknown, not taken as fine.
ask "is the filesystem healthy?" | where label=verdict | assert contains MOUNTED
ask "is the filesystem healthy?" | where label=unknown | assert contains journal
ask "what to say?" | where label=verdict | assert contains YOU CAN ASK
ask "what to say?" | assert contains who can reboot

echo ''
echo '===== ask: what a service is, what it can do, who can do a thing ====='
# What a service IS comes from its own crate's description; a term from GLOSSARY.md.
ask "what is fs?" | assert contains The filesystem
ask "what is MISCIS?" | assert contains Memory isolation
# What it CAN do, and who can, from the capabilities the kernel holds for each task. The shell holds
# `reboot` on every board (the hardware part asserts it through `caps`), so it is the honest witness.
ask "what can shell do?" | assert contains reboot
ask "who can reboot?" | where label=verdict | assert contains CAN REBOOT THE MACHINE
ask "who can reboot?" | where label=fact | assert contains shell
# A table that could not be read would be an UNKNOWN line; on a healthy machine there is none.
ask "who can reboot?" | where label=unknown | assert empty

echo ''
echo '===== ask: how a service ended, from the kernel death notice ====='
# `chaos kill-storm time 1` kills `time` and waits on its generation BUMP - the restart, not a sleep. A
# kill is asked for, so the answer says KILLED and names who called it, and never CRASHED.
assert ok chaos kill-storm time 1
ask "why did time restart?" | where label=verdict | assert contains WAS KILLED BY
ask "why did time restart?" | where label=verdict | assert lacks CRASHED
# The kill restarted `time`, and on a board with no clock chip (the Pis, the VisionFive) the wall clock
# lives THERE: the new instance re-reads its saved floor and asks the network again. The next part,
# packages, checks a certificate's dates against that clock, and found it unset - so on those boards the
# full `selfcheck` skipped the whole package lifecycle, while `selfcheck packages` alone ran it (Pi 4,
# 2026-10-27). Wait for the clock to come back, bounded at 20 tries a second apart. READ ONLY: `date epoch`
# asks and sets nothing (part 2 says why a check must not sync the clock), and a machine whose clock was
# never set just spends the 20 s and goes on. (The first version of this wait found a bug in `time`: it
# retried its floor read only after two SILENT seconds, so this very polling kept the clock unset for all
# 20 s. `time` now retries on elapsed time, and the clock comes back within a few seconds of the kill.)
let mut clock_tries = 0
loop {
    if date epoch { break }
    clock_tries = $clock_tries + 1
    if $clock_tries > 20 { break }
    wait 1
}

echo ''
echo '===== ask: what it will not do, and what it does not understand ====='
# ask reads; it changes nothing. A request to act is refused, by status, not guessed at.
ask "restart fs" | where label=status | assert contains action
ask "why is my computer sad?" | where label=status | assert contains not-understood
# Curly quotes, as a phone types them, quote the question too.
ask “what to say?” | where label=verdict | assert contains YOU CAN ASK
# An Easter egg says what it is: a joke is never mistaken for an answer.
ask "tell me a joke" | where label=verdict | assert contains A JOKE
# A foreground program for one question: nothing of it is left running once the answer is in.
status | where name=ask | assert empty
status | where name=ask-net | assert empty
