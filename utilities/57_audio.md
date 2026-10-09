<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `audio` - sound: what is playing, the volume, the codec's power, a test tone

Version reported by `audio version`. Implementation shape: **shell built-in**, asking the board's audio
driver over IPC - `audio-driver` on x86, `pwm-audio` on the Pis. The driver owns the controller; the shell owns the words.

## Status, as built and honest (2026-10-03)

**Built and run in QEMU only** (`intel-hda` with the `hda-output` codec, `mixer=on`), on branch
`feat/audio`: every verb in section 1 answered as section 2 says, and the WAV QEMU wrote agrees - the
level follows the volume, volume 0 and mute are silent, a tone lasts as long as asked and `q` cuts it
short (`docs/audio.md`, "Step A4, first half"). **On the Pis** the same verbs reach `pwm-audio`, which
drives the 3.5 mm jack by PWM - built, run in QEMU (which can only show that it refuses an emulator that
does not pace its DMA), and **heard on a Pi 4** (2026-10-03: tone, volume, mute and unmute through the
jack); the Pi 2 is built and not yet heard. Not run on hardware on x86: on the T630 the driver surveys the codec and stops before playback, and every verb below
answers that this codec has not had playback verified yet (`docs/audio.md`, step A6).

The verbs in section 1 are built, and the volume, the mute and the output survive a reboot (section 5).
`hardware`, `outputs`, `output` and `debug` were built on 2026-10-09 (`feat/audio-finish`) and run in
QEMU, which has ONE audio controller with ONE output - so the reports, the in-use marks and a refused name
are shown there; a second controller and switching between two outputs are not, until hardware with them.
The rest of the surface the operator agreed - `system sounds` and the keyboard shortcuts - is designed in
`docs/audio.md` and not built yet; each of those words answers `not built yet` rather than being mistaken
for a fault.

## 1. Verbs

| Verb | Kind | What it does |
|---|---|---|
| `audio` | help | usage (rule 1). Never an alias for `status` |
| `audio status` | report, pipes | on or off, volume, muted, output, what is playing and how far through, underruns |
| `audio info` | report, pipes | the detail a fault needs: controller version, codec, the path from converter to pin, the amplifier and the step it is at, the format, the ring, interrupt or polling |
| `audio outputs` | report, pipes as records | BUILT: the outputs the device has - line out, speaker, headphone - with `*` on the one playing, and whether something is plugged into each where the jack can tell (`plugged in`, `empty`, or `cannot tell`). Two of a kind are numbered: `line out`, `line out 2` |
| `audio output <name>` | action | BUILT: play through that output - `audio output headphone`. Names come from `audio outputs`; an unknown one is refused with the list. Refused while something plays, and kept in `/audio.settings` |
| `audio hardware` | report, pipes as records | BUILT 2026-10-09: every audio device on this machine, one row each - its PCI address as `hardware` names it (or `jack` on the Pis), what it is, who made it, the service that drives it or `-`, its state (the driver's own word for the one it holds: `ready`, `off`, `surveyed, not played (A6)`), and `*` on the one `audio` talks to. Answers with or without an audio driver running. The T630 has two controllers (its analog codec and the Radeon's HDMI audio), which is why this exists |
| `audio hardware <device>` | report, pipes | BUILT 2026-10-09: one device in full, by the name `audio hardware` gives it - why it is or is not driven, its registers and grant as `hardware <device>` shows them, and the driver's account (`audio info`) for the one in use. `audio hardware use` is not built: no machine has two audio devices a driver can play on |
| `audio volume <0-100>` | action | set the volume. Reading it is `audio status` - one way to ask (rule 3) |
| `audio mute` | action | silence the output, keeping the volume |
| `audio unmute` | action | restore the volume set before `mute` |
| `audio on` | action | bring the codec back to full power, then re-apply the volume and the mute |
| `audio off` | action | stop anything playing and put the codec in its lowest power state; the controller stays up |
| `audio off hard` | action | stop anything playing and hold the whole controller in reset - the closest HD Audio has to cutting the power. `audio on` brings it back |
| `audio tone <hz> [seconds]` | action | play a sine the driver generates itself, 20 to 20000 Hz, 2 s unless told (tenths allowed: `0.5`, up to 600). Blocks with `[q] quit`; `q` STOPS the tone (rule 11) |
| `audio play <path>` | action | play a WAV file from disk: 16-bit PCM, mono or stereo, 44100 or 48000 Hz where the codec offers it. Blocks with `[q] quit`; `q` STOPS it. Anything else is refused with the reason |
| `audio debug [view]` | report, pipes | BUILT: the driver's own account of itself, one view of `stats` (bare), `codec`, `stream`, `trace` or `registers` - verbs sent and unanswered, interrupts, underruns and the last sound's rate by the clock; the whole widget graph; the output stream's registers and buffer descriptors; the last 64 verbs and their answers; the controller's globals. On a driver that surveyed its codec and stopped (the T630 today), `codec`, `trace` and `registers` still answer. The Pis' jack answers every view, `codec` and `trace` with a line saying it has neither |
| `audio help` | | usage, one real example per row |
| `audio version` | | the version and the collective copyright line (rules 5 and 6) |

**`mute` and `unmute` are two verbs, not two spellings**: two distinct actions, so rule 3's ban on synonyms
does not apply.

**Volume is 0 to 100, and 0 is not mute.** Both are silent, and they stay separate states: `mute` keeps
the volume and `unmute` returns to it. On x86 the scale is linear in the codec's own amplifier steps, which
are even steps of decibels, so equal steps of volume sound like equal changes; `audio info` shows the step.
The Pis have no amplifier: `pwm-audio` scales the samples by the square of the volume, for the same
reason.

## 2. What each action answers

Every change of state is READ BACK from the codec before it is reported: `- verified` when the codec
agrees, `- unverified (the codec could not be asked)` when it could not be asked, `- unconfirmed (this
codec does not report it, so it cannot say)` when the codec does not model the change at all (QEMU's
codec reports no power states, so `audio off` is unconfirmed there), and `FAILED - the codec reads back
something else` when it disagrees.

| Asked | Answer |
|---|---|
| `audio volume 60` | `volume 60 - verified` |
| `audio volume 0` | `volume 0 - silent - verified` |
| `audio volume 60` while off | ``volume 60 - kept; audio is off, so it is set at `audio on` `` |
| `audio mute` | `muted - verified`; if already: `already muted` (nothing sent) |
| `audio unmute` | `unmuted - volume 60 - verified`; at volume 0: `unmuted - volume 0, silent - verified`; if not muted: `not muted` |
| `audio off` | `audio off - the codec is powered down; audio on brings it back - verified` |
| `audio off hard` | `audio off (hard - the controller is held in reset; audio on brings it back) - verified` |
| `audio on` | `audio on - volume 60, unmuted, output line out - verified`; if on: `already on` |
| `audio output headphone` | `output headphone - verified`; already in use: `already playing through headphone` (nothing sent); nothing plugged in: says so on the next line |
| `audio output speakers` | `audio: no output called 'speakers' - this device has:` and the list |
| `audio tone 440 2` | `playing 440 Hz for 2.0 s  [q] quit`, then `played 440 Hz for 2.0 s` or `stopped after 1.2 s` |
| `audio tone` while off | ``audio is off - `audio on` first`` |
| `audio tone` muted, or at volume 0 | plays, and says first: `muted - nothing will be heard` / `volume is 0 - nothing will be heard` |
| `audio play /song.wav` | `playing /song.wav (48000 Hz, 16-bit, stereo, 0:02)  [q] quit`, then `played 0:02`, or `stopped after 1.4 s` |
| `audio play` that ran dry | `played 0:02, 180 ms of silence where the samples did not arrive in time` - never hidden |
| `audio play /x.wav`, 24-bit | `audio: /x.wav is 24-bit - this plays 16-bit PCM` |
| `audio play /x.wav`, 8000 Hz | `audio: /x.wav is 8000 Hz - this codec plays 44100 or 48000 Hz` |
| `audio play /x.mp3` | `audio: /x.mp3 is not a WAV file`; compressed WAV: `is compressed - this plays uncompressed PCM` |

## 3. Pipes (rule 12)

`status` and `info` start pipes, as labelled lines: `audio status | match volume`, `audio info | write
/audio-info.txt`. `outputs` pipes as records - `output`, `plugged`, `in_use` - so `audio outputs | count`
is how many there are; `debug` pipes as text (`audio debug codec | write /codec.txt` captures a new
machine's codec); `hardware` pipes as records too - `device`, `kind`, `made_by`, `driver`, `state`,
`in_use`. The actions refuse with a sentence naming the reports. Piping sound IN will be refused
in the design: a pipe carries 16 KiB, a tenth of a second of sound, so `play` takes a PATH and the file is
the adapter (`docs/audio.md`).

## 4. Failure says which half failed

| Situation | What `audio` says |
|---|---|
| No audio driver running (the VisionFive, which has no audio output, or a board whose jack was not found) | `no audio hardware on this machine` - not an error to ask |
| The driver is running and did not answer | `audio: the audio driver is not answering` |
| No HD Audio controller | `audio: no audio hardware on this machine` |
| The controller did not leave reset | `audio: the controller is there and did not come out of reset - the serial log says more` |
| No codec on the link | `audio: the controller is up but no codec answered` |
| No usable output path | `audio: the codec offers no output this driver can use` |
| A codec playback has not been verified on (the T630 today) | `audio: this codec has not had playback verified yet - the driver surveyed it and stopped (docs/audio.md, A6)` |
| A tone still playing five seconds after it should have ended | stopped, and said |

An absent, wedged or restarting driver makes `audio` return with a loud sentence, never hang: every ask
is bounded, and every answer is immediate by design - a tone is started and answered, then followed with
`status` (`sdk/audio`'s wire protocol, which is tagged so a late answer is never read as the next one).

## 5. Settings that survive a reboot: `/audio.settings`

**The volume, the mute and the output are kept on disk**, in plain labelled lines, readable with
`read /audio.settings`:

```
volume 60
muted no
output headphone
```

- **The driver owns the file.** It reads it once when it comes up and writes it after a change the codec
  did not contradict - a setting the codec refused is not written. `on` and `off` are NOT kept: audio
  comes up on at every boot.
- **Never written while a tone plays.** A write blocks the driver, and a slow one mid-tone would starve
  the sound; a change made during a tone is written when it ends.
- **Bounded and tolerant.** The file is read in one piece, at most 256 bytes; a line the driver does not
  know is ignored and said once in the log; no file means the defaults (volume 50, unmuted). Where `fs`
  is absent - no data disk - or does not answer, the driver says so once, carries on with what it holds,
  and says a failed write once rather than on every change.
- `output` is written only once one has been chosen with `audio output`; until then the driver plays
  through the first output its survey found. At boot it is restored if the codec still has an output of
  that kind, and the log says so either way. The Pis' jack is one output, so `pwm-audio` keeps no line.

## 6. Tab completion and words from elsewhere (rules 8 and 9)

`audio` completes its BUILT verbs, and `off` completes `hard`. The words people bring from other systems
are hints, never aliases: `beep` and `speaker-test` point to `audio tone`, `amixer` and `alsamixer` to
`audio volume`, and `aplay` to `audio play`. `audio play ` completes a PATH, which is why `audio` is not
among the commands whose arguments are keywords only.
