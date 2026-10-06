# 75. One directory lists the same name twice

**Status: OPEN - seen once, 2026-10-06, on the Pi 2's USB stick; not investigated. No cause is claimed here.**

## What was seen

On the R4b card for the USB WiFi dongle (`docs/wifi-usb.md` 10), `dir` at the Pi 2's prompt listed `/` as:

```
  NAME                  TYPE       SIZE  MODIFIED
  wifi.keys             file       72 B  2026-10-05 12:09
  .gsh_history          file       40 B  2026-10-05 11:57
  before_chaos.txt      file   12.0 KiB  unknown
  after_chaos.txt       file   12.0 KiB  unknown
  .gsh_history          file       16 B  unknown
  cutme.txt             file        5 B  2026-09-22 12:06
  clock.last            file       10 B  unknown
  networks.txt          file      826 B  2026-10-02 11:01
  wifi.keys             file       72 B  2026-10-02 18:23
  audio.settings        file       19 B  2026-10-03 22:15
  wifi-soak.gsh         file       60 B  2026-10-04 07:36
  11 entries
```

Two entries named `wifi.keys` and two named `.gsh_history`, each pair with different sizes or dates. A
directory holds one entry per name, so either the directory really holds two, or `dir` is showing one
twice, and the different dates say the first is more likely.

The same stick has been moved between the Pi 2, the Pi 4 and the VisionFive, and written on all three:
`/wifi.keys` by `wifi-driver` on the Pi 4 and the VisionFive, `.gsh_history` by the shell on every board.
`wifi-usb` read `/wifi.keys` on this boot and found one network, so a lookup by name returns ONE of them;
which one was not checked.

## What would tell the cases apart, and is cheap

- `drives check` on that stick: does the checker see two entries with one name, and does it call that
  bad? A checker that passes this tree has a gap of its own.
- `read /wifi.keys` and `read /.gsh_history`: which of each pair a lookup returns.
- Whether a fresh stick, `drives flash`ed and written on one board only, ever shows it - which separates
  "a write path that does not replace by name" from "trees written by different builds disagreeing".

## Why it is recorded and not chased now

It turned up on a WiFi card (`feat/wifi-driver`) whose subject is not the filesystem, and one sighting
with no reproduction is not enough to start changing `fs`. It is written down so the next session on
`fs` starts from it rather than from nothing (26.7).
