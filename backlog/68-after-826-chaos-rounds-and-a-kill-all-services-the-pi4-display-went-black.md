# 68. After 826 rounds of chaos and a `kill all-services`, the Pi 4's display went black and the console's paint took 595 ms

**Status: OPEN - observed 2026-09-30 on the Pi 4, once, at the end of the first chaos run with the radio in the image. Not investigated; recorded so it is not lost under `backlog/66`.**

## What the log holds

- `chaos max-carnage`, 826 rounds. `console` was killed and respawned 418 times, every respawn logging
  `framebuffer 1920x1080 32bpp` at the same physical address and `serving the display`. The kernel did
  not panic; nothing wedged; the prompt answered afterwards over serial.
- The operator then ran `kill all-services`. The supervisor's respawn found the console alive and
  adopted it (`adopted running console`). Half a second earlier that console instance had logged
  `paint took 595302 us - far slower than this display should be; check the framebuffer memory type
  (pass 184)` - the first and only such line in the run.
- The operator reports the TV black from then on. The serial console kept working.

## What is and is not known

595 ms per repaint is the fingerprint of the framebuffer mapped Device-nGnRnE, which `2f84177c` fixed
for the ordinary spawn. Whether the 418th respawn's mapping was different, whether `kill all-services`
changed the grant, or whether the display itself lost the buffer, the log cannot say: the console's own
instrument fired once, at pass 184 of an instance that had painted normally for two minutes. Not
reproduced. Not to be assumed a wifi finding - the radio was already dead for the whole storm
(`docs/wifi.md` 45) and shares nothing with the display path.

## Next

Reproduce without the radio: a Pi 4 chaos run to a few hundred rounds, then `kill all-services`, with
the display watched. If the black screen returns, read `bootcon::reclaim_on_death` and the `spawn[fb]`
memory attribute on aarch64 against the console's respawn path.
