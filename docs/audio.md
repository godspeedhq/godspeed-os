# Audio

**Status: steps A1-A5 built and run in QEMU (2026-10-03; A4's outputs, debug and system sounds not built), built on
`feat/audio` and since merged to `main`. The driver resets an Intel High Definition Audio controller, finds its codec and output
path, moves codec commands onto the CORB and RIRB, and serves a tagged request protocol; the shell's
`audio` sets the volume, mutes, powers the codec down and up and plays tones (`utilities/57_audio.md`),
each checked against the WAV QEMU wrote; the volume and the mute survive a reboot in `/audio.settings`.
Interrupt-driven, IOMMU-confined, restartable; `play` streams PCM (A5). The Pis' 3.5 mm jack is driven by
`pwm-audio` and was HEARD on a Pi 4 (2026-10-03); the Pi 2 is built and not yet heard. Not yet: `outputs`,
`debug`, system sounds, the shortcuts; the HDA driver not run on hardware past the codec survey (A6).**

Audio is two things at once here. It is the system's first sound, and it is the planned **independent
test of `gs::driver`** (`docs/driver-library.md`, "Wi-Fi discovers; audio tests"): a second kind of
driver, sharing no protocol with Wi-Fi, written against the mechanisms the Wi-Fi work found. Where those
fit naturally they are probably general; where they have to be bent, the library is what is wrong. So
this driver uses `gs::driver` for every wait and hold, and every bend is written down below.

## The hardware

**Target: Intel High Definition Audio (HDA)**, on x86. It is what QEMU emulates (`-device intel-hda`
plus a codec), which makes it the one audio device here whose output a test can CHECK - QEMU writes it
to a WAV file - and it is what the HP T630 has.

| Machine | Audio | Notes |
|---|---|---|
| QEMU | `intel-hda` (ICH6, `8086:2668`), codec `hda-output` (`1af4:0012`) | DAC node 2 -> line-out pin node 3. Immediate Command registers implemented. Sound to `build/qemu_audio.wav` (`osdev run`) |
| HP T630 | `00:09.2` AMD FCH Azalia (`1022:157a`), codec Realtek ALC255 (`10ec:0255`, subsystem `103c:8158`) | internal speaker, front headset jack, rear line-out. Linux: snoop via PCI config 0x42, trust LPIB, 40-bit DMA |
| HP T630 | `00:01.1` Radeon HDMI audio (`1002:9840`) | a SECOND class-0x0403 controller - see "Found while preparing" |
| Pi 2 / Pi 4 | 3.5 mm jack driven by PWM, fed by the BCM DMA engine (section "The Pis and the VisionFive") | not HDA. Needs the GPIO pinmux and the clock manager, both SHARED SoC blocks; no QEMU model. BUILT: the kernel prepares both as part of the grant (`pwm-audio`); heard on a Pi 4, the Pi 2 not yet |
| VisionFive 2 Lite | no analog output; HDMI only | confirmed three ways: the board's port list, the vendor device tree disabling its PWM-DAC, and the board's own Linux log (`build/serial_output_risc_v_original.log`): `ALSA device list: No soundcards found` |

Sources: the HDA specification rev 1.0a; QEMU `hw/audio/intel-hda.c`, `hda-codec.c`; Linux
`sound/hda/controllers/intel.c`, `sound/hda/core/controller.c`, `stream.c`, `codecs/realtek/alc269.c`;
linux-hardware.org probes of the T630. The divergences from Linux are recorded where they happen
(CLAUDE.md 26.14).

## The process, applied to this driver

- **Every wait is `gs::driver`'s.** Condition waits are `wait::until` / `Deadline` / `until_paced`;
  gaps nothing reports the end of are `delay::hold` (spinning) or `delay::hold_parked` (long, sleeping).
- **A wait that has to be bent is a finding** about the library, recorded under the step that met it.
- **Something device-neutral written by hand here** is a candidate for the library only if another
  driver already writes it by hand - found repeated, never imagined.
- **One change per step, a prediction before it runs, QEMU first, the gates on every build, and
  "Verified" only after the run** - the same as every driver step in `docs/driver-library.md`.

## The plan

| Step | What | Where it runs |
|---|---|---|
| **A1** | Reset, find the codecs, walk the widget graph, report an output path. Immediate Command registers; no DMA, no interrupt | QEMU - **built** |
| **A2** | CORB/RIRB, the command rings the spec requires (Immediate Command is optional, and unknown on the T630's FCH). The first DMA - used only on QEMU's codec until A6, for the T630's own reasons | QEMU - **built** |
| **A3** | Configure the path (power, amps, pin control, converter format and stream tag) and play a tone the driver generates itself: one output stream, a BDL, a cyclic buffer in the DMA arena, polled LPIB | QEMU - **built**, checked by reading the WAV QEMU wrote |
| **A4** | A request protocol (tagged, defined once and shared with the shell), the `audio` utility as specified below, and `/audio.settings` | QEMU - **protocol, the first verbs (status, info, volume, mute, unmute, on, off, off hard, tone), `/audio.settings` and `osdev test audio` built; `hardware`, `outputs`, `output`, `debug`, system sounds and the keyboard shortcuts built 2026-10-09** |
| **A5** | `audio play <path>`: the shell reads the WAV and streams chunks; the driver answers each with the free space left; underruns write silence and are counted | QEMU - **built** |
| A6 | Real sound on hardware. **HEARD on the Wyse 5070 and, on the T630 through its speaker and headphones (2026-10-09)**, which needs neither kernel fix nor a snoop bit; then the T630: the kernel fixes below, the AMD snoop bit, the ALC255's real path walk with EAPD. A person listening on each | Wyse, then T630 |
| **later** | Interrupt-driven refill and IOMMU confinement - both **built**. (Restart management was done after A3) | QEMU |

**Before A3, the library work the process asks for.** Two things audio needs are already written by
hand in other drivers, so by the rule they are library candidates NOW, and audio is meant to test them
rather than be their next copy:

- **Interrupt waiting** - wait on a timed receive, tell an interrupt from a request, serve the request
  rather than drop it. `xhci`, `ehci` and `dwc2` each write it.
- **DMA arena layout** - named regions with alignment and a fit check against the arena. `xhci`,
  `ehci`, `nic-driver`, `dwmac` and `genet` each write it.

Extracting them touches those drivers and needs their hardware, so it is its own work, ahead of the
step that first needs each (A3 for the arena, the interrupt step for the other).

## The `audio` utility (specification, agreed 2026-10-03)

**Partly built (2026-10-03).** The verbs the shell answers are specified in `utilities/57_audio.md`,
which is now the authority for them; this section stays the agreed design for the rest - `outputs`,
`output`, `debug`, `system sounds` and the shortcuts (`play` and `/audio.settings` are built since). What follows was written
before any of it existed. It lived here, not in `utilities/`, until the shell answered `audio`: Commandment X fails a `utilities/` spec for a verb the
shell does not have, and the reverse (`utilities/0_conventions.md` 2a). On the day it is built it moves
to a numbered spec of its own under `utilities/`, with the shell's eight registration sites. Modelled on `utilities/56_wifi.md`,
which is the closest device-control utility, and held to the fourteen rules of `0_conventions.md`.

### Verbs

| Verb | Kind | What it does |
|---|---|---|
| `audio` | help | usage, as every utility (rule 1). Never an alias for `status` |
| `audio status` | report, pipes | the everyday answer, read live from the driver: on or off, muted, volume, output, idle or playing (and how far through), underruns since start |
| `audio info` | report, pipes | the detail a fault needs: controller and codec identity, outputs the codec offers, supported rates, ring size, interrupt or polling, position |
| `audio outputs` | report, pipes as records | the outputs the codec actually has - speaker, headphone, line out - with which is selected and whether something is plugged into each |
| `audio output <name>` | action | choose the output: `audio output headphone`. Names come from `audio outputs`; an unknown one is refused with the list |
| `audio hardware` | report, pipes as records | (agreed and built 2026-10-09) every audio device on the machine, one row each: its PCI address or `jack`, what it is, who made it, its driver or `-`, its state, and `*` on the one `audio` talks to. See "Which device: `audio hardware`" below |
| `audio hardware <device>` | report, pipes | (agreed 2026-10-09) one device in full: identity, bus address, what was granted, and why it is or is not driven |
| `audio volume <0-100>` | action | set the volume. Reading it is `audio status` - one way to ask (rule 3) |
| `audio mute` | action | silence the output, keeping the volume |
| `audio unmute` | action | restore the volume that was set before `mute` |
| `audio tone <hz> [seconds]` | action | play a sine wave the driver generates itself: `audio tone 440 2`. Two seconds if no length is given. Blocks with `[q] quit` |
| `audio play <path>` | action | play a WAV file from disk: `audio play /music/test.wav`. Blocks with `[q] quit` |
| `audio on` | action | bring the codec back to full power, then re-apply volume, mute and output |
| `audio off` | action | stop anything playing and put the codec in its lowest power state. The controller stays up |
| `audio off hard` | action | stop anything playing and hold the whole controller in reset - the closest HD Audio has to cutting the power. `audio on` brings it back |
| `audio system sounds on` / `audio system sounds off` | action | the short sounds the system makes on its own - an error, a refusal, a finished job, a device plugged in. Section below |
| `audio debug [codec\|stream\|stats\|trace\|registers]` | report, pipes | the driver's own account of itself. Section below |
| `audio help` | | usage, one real example per row |
| `audio version` | | the version and the collective copyright line (rules 5 and 6) |

**`mute` and `unmute` are two verbs, not two spellings** of one: two distinct actions, so rule 3's ban on
synonyms does not apply, the way `scan` and `list` are two verbs in `wifi`.

**Volume is 0 to 100, and 0 is not mute** (the operator's call, 2026-10-03, reversing a first draft that
refused 0). Both are silent, and they stay separate states: `mute` keeps the volume and `unmute` returns to
it, so muting at volume 0 and unmuting gives volume 0 back, and says so. Nothing is ever silent without
saying why - `status` shows both, and `tone` or `play` at volume 0 warns that nothing will be heard. The
scale is PERCEPTUAL - each
step sounds like the same change - mapped onto the codec's own amplifier steps (QEMU's has 74, the
ALC255's about 87); the driver does the mapping, and says the codec step it chose in `audio info`.

### What each action answers

Every action answers in one line, and every change of state is READ BACK from the codec before it is
reported, as `wifi radio off` is: `- verified` when the codec agrees, `- unverified` when it could not be
asked, `FAILED` when it disagrees.

| Asked | Answer |
|---|---|
| `audio volume 60` | `volume 60 - verified` |
| `audio volume 0` | `volume 0 - silent - verified` |
| `audio mute` | `muted - verified`; if already: `already muted` (nothing sent) |
| `audio unmute` | `unmuted - volume 60 - verified`; at volume 0: `unmuted - volume 0, silent - verified`; if not muted: `not muted` |
| `audio off` | `audio off - the codec is powered down; audio on brings it back - verified` |
| `audio off hard` | `audio off (hard - the controller is held in reset; audio on brings it back) - verified` |
| `audio on` | `audio on - volume 60, unmuted, output speaker - verified`; if on: `already on` |
| `audio tone 440 2` | `playing 440 Hz for 2 s  [q] quit`, then `played 440 Hz for 2 s` or `stopped after 1.3 s` |
| `audio play /x.wav` | `playing /x.wav (48000 Hz, 16-bit, stereo, 3:12)  [q] quit`, then `played 3:12` or `played 3:12, 2 underruns (180 ms of silence)` |
| `audio play` or `tone` while off | ``audio is off - `audio on` first`` |
| `audio play` or `tone` at volume 0, or muted | plays, and says first: `volume is 0 - nothing will be heard` / `muted - nothing will be heard` |

### Pipes (rule 12): every report is data

`audio` is pipeable by the standing rule for every new utility: **a report starts a pipe - as RECORDS
where it is a table, as labelled lines where it is not - and an action refuses with a sentence that names
the reports.** The same facts are decoded once for both the screen and the pipe.

| Report | In a pipe | Fields |
|---|---|---|
| `audio status` | labelled lines | `power` (on / off / off-hard), `muted` (yes / no), `volume` (0-100), `output`, `playing` (nothing, or what and how far), `system-sounds` (on / off), `underruns` |
| `audio info` | labelled lines | `controller`, `codec`, `outputs`, `rates`, `ring-ms`, `mode` (interrupt / polling), `position` |
| `audio outputs` | RECORDS, one per output | `output` (speaker / headphone / line-out), `node` (the pin, number), `plugged` (yes / no / unknown), `selected` (yes / no) |
| `audio debug codec` | RECORDS, one per node | `node`, `type` (output / input / mixer / selector / pin / power / volume-knob / beep / other), `caps`, `connections`, `config` (pins only), `device` (pins only) |
| `audio debug trace` | RECORDS, one per verb | `seq`, `codec`, `node`, `verb`, `response`, `us` (how long the answer took; empty if it timed out) |
| `audio debug stats`, `stream`, `registers` | labelled lines | the counters and registers, one `name value` per line |
| `audio version` | text | as every utility |

Numbers are typed as numbers, so the record stages compare them, not strings; register and capability
values print in hex on the screen and travel as numbers in a record.

```
audio status | match volume                 the volume line
audio outputs | where plugged=yes           what is plugged in
audio outputs | where selected=yes | select output
audio outputs | count                        how many outputs the codec has
audio debug codec | where type=pin           every pin on the codec
audio debug codec | to json | write /codec.json
audio debug trace | where us>1000            verbs the codec was slow to answer
audio debug trace | max us
audio debug stats | match underrun
audio info | write /audio-info.txt
```

**The actions refuse, and say what to pipe instead.** `volume`, `mute`, `unmute`, `output`, `tone`,
`play`, `on`, `off` and `system sounds` are actions: their value is the change, and what they print is a
confirmation, not data. In a pipe each answers ``audio volume is an action - pipe a report: audio status,
audio info, audio outputs, audio debug``, and adds one row to the "Not a pipe source" table of
`0_conventions.md` rule 12.

**Piping sound in is refused, with a reason.** `read /x.wav | audio play` would be the obvious shape, and
a pipe carries at most 16 KiB - about a tenth of a second of 48 kHz stereo (`docs/pipes.md`). So `play` as a
pipe sink answers ``a pipe carries 16 KiB, a tenth of a second of sound - play the file: `audio play
<path>` ``: the file is the adapter, as everywhere else in the shell.

**A report that fails says why on the console and stops the pipe**, rather than sending its sentence down
it: with no audio hardware, `audio outputs | count` prints `no audio hardware on this machine` and counts
nothing. An empty answer is zero rows, so `count` says 0 rather than counting a sentence.

**Tested in QEMU, not assumed:** each report through `count`, `where` and `to json`, and each action's
refusal, in the A4 suite - the lesson of `wifi`, whose spec promised pipes for weeks while the shell
refused them because `wifi` was missing from its list of producers.

### Blocking, quitting, the background (rules 10 and 11)

`tone` and `play` block and show `[q] quit`; `q` tells the driver to stop, which plays out nothing more,
writes silence and stops the stream (rule 11 - quitting stops the task, not just the shell's view of it).
The other actions return within a second. Playing in the background comes later, on the existing `jobs`
model, so `jobs quit` stops it - which is why there is no `audio stop`: it would be a second way to stop.

### Settings that survive a reboot

**Volume, mute and output are kept on disk, in `/audio.settings`**, at the operator's call. The shape is
the one `/wifi.keys` already proved (`utilities/56_wifi.md` 6):

- **The driver owns the file**, reads it once when it comes up and writes it after every change it
  verified. A setting the codec refused is not written.
- **Plain labelled lines**, readable with `read /audio.settings`:
  ```
  volume 60
  muted no
  output speaker
  system-sounds on
  ```
- **Bounded and tolerant.** Fixed-size, read in one piece; an unknown line is ignored and said once; a
  missing file means the defaults (volume 50, unmuted, the codec's first connected output); an `fs` that
  is absent or mid-restart costs a line in the log and the driver carries on with what it holds.
- **System sounds are kept** with the rest. **`on`/`off` is NOT kept.** Audio comes up on at every boot; an `off` is for this session.
- It makes `fs` a peer of the driver, reacquired by name when `fs` restarts (Commandment IX), arriving
  with the step that writes it (A4).

### Failure says which half failed

| Situation | What `audio` says |
|---|---|
| No HD Audio controller on this machine | `no audio hardware on this machine` - not an error to ask |
| Controller present, driver not answering | `audio: the audio driver is not answering` |
| Driver up, no codec on the link | `audio: the controller is up but no codec answered` |
| Codec up, no output path | `audio: the codec offers no output this driver can use` (and `audio info` lists what it found) |
| A file that is not a WAV, or not 16-bit PCM | `audio: /x.mp3 is not a WAV file` / `audio: /x.wav is 24-bit - this plays 16-bit PCM` |
| Playback underran | said at the end, with the count and the silence it cost - never hidden |

An absent, wedged or restarting driver makes `audio` return with a loud unavailable, never hang
(Commandment VIII at the command layer).

### Which device: `audio hardware` (agreed 2026-10-09)

Asked for by the operator, on the model of `wifi hardware` (`utilities/56_wifi.md` 11). The reason is
sharper than symmetry: the rest of `audio` assumes a machine has one audio device, and the T630 has two
class-0x0403 controllers - its analog Azalia with the ALC255, and the Radeon's HDMI audio - of which the
kernel binds the FIRST, the HDMI one ("Found while preparing" 2, `backlog/80` K2). A report listing both,
with which one the driver holds, would have shown that on the first boot instead of in an audit.

- **`audio hardware`** is a report, one row per audio device, and pipes as records: its name, what it
  is (an HD Audio controller, or the PWM jack), who made it by the bus's IDs, the service that drives it
  or `-`, its state, and `*` on the one every other `audio` verb talks to.
- **Named by bus address, not `analog` or `hdmi`** (decided while building it, 2026-10-09): a device is
  named as `hardware` names it - `00:09.2` - or `jack` on the Pis. A controller no driver holds has a
  codec nobody has asked, so calling it `analog` or `hdmi` would be a guess; the vendor IDs beside it
  (an AMD GPU's against an AMD chipset's) say which is which without one.
- **`audio hardware <device>`** is one device in full, as labelled lines: controller and codec identity,
  the bus address, what the grant gave (window, interrupt, IOMMU confinement) and why it is or is not
  driven.
- **`audio hardware use <device>` is NOT agreed yet**: no machine has two audio devices a driver can play
  on. It comes with A6, if the HDMI audio ever becomes one; until then it is not in the help.

**No kernel change.** The rows come from what the `hardware` utility already gathers - the PCI class-0x04
devices from `hw-enumerator` and the Pis' jack as a SoC device - and the driver adds what only it knows
about the one it is bound to.

**Not the same as the other reports.** `audio hardware` is what is on the machine; `audio outputs` the
jacks on the device in use; `audio info` that device summarised; `audio debug` the driver's own account
of itself. The same split `wifi` has.

### Keyboard shortcuts (agreed 2026-10-03)

| Keys | Does |
|---|---|
| Ctrl+Alt+Up | volume up 5 (stops at 100) |
| Ctrl+Alt+Down | volume down 5 (stops at 0) |
| Ctrl+Alt+M | mute, or unmute if muted |

**The SHELL acts on them; the keyboard drivers only say they happened.** This is the Ctrl+Alt+Del
pattern (CLAUDE.md 6.4, the SEC-2 follow-up): `xhci` and `ehci` put a byte no typed key produces on the
console stream, and the shell - which holds the authority - sends the request to `audio-driver`. The
keyboard drivers gain no capability and never speak to the audio driver. Each press is the same request
as the typed verb, so it is read back from the codec and written to `/audio.settings` like one.

**What the console shows.** Every press shows its result, as read back - never what was asked for:

- **At the prompt**, one notice line above it, then the prompt and whatever was half-typed are redrawn,
  so typing is not disturbed:
  ```
  volume 65  [#############-------]
  ```
  A press while that notice is still the last line OVERWRITES it rather than adding one, so holding the
  keys gives one line that counts, not twenty. Mute shows `muted (volume 65)`; at volume 0, `volume 0 -
  silent`. The bar is ASCII and decoration; the number is the fact (rule 7).
- **During `audio play` and `audio tone`**, no separate notice: the volume is part of the status line
  already being redrawn, `playing /music/test.wav  1:02 / 3:12  volume 65  [q] quit`.
- **When it cannot**, one line per press, never a silent no-op: `audio: the audio driver is not
  answering`, or `no audio hardware on this machine`.

**Where they work, and where they do not.** At the prompt and during `play`/`tone` - where the shell is
reading the keyboard. Not while a full-screen program owns the console (`edit`, `observe`): the shell is
not reading then, the same limit Ctrl+Alt+Del has. Not reliably over a SERIAL terminal either, which may
not send Ctrl+Alt+arrows as a distinct key (and a Windows host may take some Ctrl+Alt chords itself): the
serial console keeps the typed verbs.

**Not a second way to ask.** Rule 3 bans two command names for one thing, not a key binding for a command
- Ctrl+Alt+Del and `reboot` already coexist the same way.

**Later: the keyboard's own volume keys.** Many keyboards have dedicated volume up, down and mute keys.
The USB keyboard drivers speak the HID boot protocol, which never reports them; they arrive on the
consumer-control report, which both drivers would have to learn. Worth doing, recorded rather than
promised.

### System sounds (agreed 2026-10-03)

Short sounds the system makes on its own, as Windows and macOS do. **`audio system sounds on`** and
**`audio system sounds off`** switch them; `audio status` shows which (`system sounds on`), so there is one
way to ask. Kept in `/audio.settings`. **On by default.**

`system` is a verb with one child today, and the precedent for four words is `wifi radio off hard`. Anything
that joins it later must be about system-wide audio behaviour, so `system` does not become a drawer.

| Sound | When | Shape |
|---|---|---|
| error | a command not found; a command that failed | two short falling tones |
| refused | an action that is not allowed - a capability the shell does not hold, a refused request | one low tone |
| done | a background job finished | a short rising chirp |
| plugged / unplugged | a USB device arrives / leaves | a rising / falling pair |

**The driver GENERATES them; no sound files ship.** Errors happen when things are going wrong, `fs` being
down among them, and a sound that had to be read from disk first would be silent exactly then - and would
cost every error two `fs` round trips. They are our own tones, not copies of anyone else's. A WAV of the
same name under `/sounds/` (`/sounds/error.wav`) REPLACES a built-in one; when it cannot be read, the
built-in plays instead.

**They never slow anything down.** The shell asks for the sound and does not wait for it: the error prints
at once either way, and an absent or restarting driver means no sound, not a delay.

**They do not machine-gun.** Only what is typed at the prompt makes a sound, never a script - a script that
hits fifty errors would otherwise sound fifty times. At most one sound per half-second. Mute and volume
apply.

Built after playback works (after A3's tone generator); the driver's synthesis is the same one `audio tone`
uses.

### `audio debug` - the driver's account of itself

Like `wifi debug` (`utilities/56_wifi.md` 4g): `status` and `info` are the user's view, `debug` is the
driver's. All of it pipes - `audio debug codec | write /codec.txt` is how a new machine's codec gets
captured.

| View | Shows | For |
|---|---|---|
| `codec` | the whole widget graph: every node, its type, capabilities, connection list and pin configuration | finding a real codec's output path - the T630's ALC255 is found from this, not assumed |
| `stream` | the output stream's registers (control, status, position, buffer length, last valid index, format) and its buffer descriptors | playback that stalls or glitches |
| `stats` | verbs sent and timed out, interrupts, underruns, how full the ring is, and the measured play rate against the wall clock | proving the DMA engine and the link clock run at the right speed |
| `trace` | the last 64 verbs and their responses, from a fixed ring in the driver | exactly what was said to the codec, as `wifi debug trace` is for frames |
| `registers` | the controller's global registers | the first look when nothing works |

A bare `audio debug` is the `stats` view. `codec` can come early - A1 already walks the graph.

### Tab completion and words from elsewhere (rules 8 and 9)

`audio` completes from the command table and its verbs from `SUBCMD_FIRST`; `output` completes from the
codec's outputs, `off` to `hard`, and `play` completes a PATH - so `audio` stays out of `NO_PATH_CMDS`
in the shell. The words people bring from other systems are HINTS, never aliases, through
`FOREIGN_HINTS`: aplay points to `audio play`, beep and speaker-test to `audio tone`, amixer and alsamixer
to `audio volume`. Their own vocabulary (mixer, PCM, sinks) stays out of the user's.

### Not in scope, for now

Recording (QEMU's codec has no input); more than one stream at a time; formats other than 16-bit PCM
WAV; per-application volume; HDMI audio.

### What it needs from the driver

The verbs above define the driver's request protocol at A4, defined ONCE in a shared module the shell and
the driver both use, as `sdk/wifi/src/wire.rs` is for `wifi`, and TAGGED so a late reply can never be
taken for the answer to a later question (`backlog/70`). One request per verb above - status, info,
outputs, set output, set volume, mute, unmute, power on / off / hard, tone - and for `play` a stream: open
with the format, PCM chunks of at most 4 KiB each answered at once with the free space left, then end. The
names are A4's to choose.

## Scheduling and power

Audio is a continuous stream to the ear and a periodic task to the scheduler. The controller pulls
samples from a ring in memory by DMA at exactly the sample rate; the driver only has to refill the ring
before it runs dry. So the service spends nearly all its time in **BlockRecv**, wakes (**Ready**,
**Running**) when a chunk of the ring has been played, copies the next chunk in, and blocks again: at
48 kHz 16-bit stereo, 192 KB/s, about 47 short wakes a second during playback and none when idle.

**No CPU clock lease.** The Wi-Fi firmware upload needed the cores fast because the CPU was the data
path. Here the DMA engine is, and 192 KB/s is negligible at any clock; `power`'s clock control is the
Pi 4's anyway. What matters is wake-up latency against the 10 ms scheduling quantum: a ring holding
100-250 ms of sound gives many quanta of margin, and an underrun's first answer is a bigger ring, not a
faster CPU. Later, `power` may own the codec's and controller's device power states, and playback is a
real customer that tickless idle (`docs/power.md` phase 2) must not starve.

## Step A1: find the codec and the way out (2026-10-03)

`services/audio-driver`: x86 only, embedded by `services/supervisor/build.rs` as a board fact
(`has_audio_driver`), spawned at boot, named by class `0x040300`, BAR0. Holds the register window and
nothing else.

**What it does:** reads GCAP, resets the controller (`GCTL.CRST` to 0 and back), waits for the codecs
to announce themselves, and for each codec in STATESTS finds the audio function group, reads every pin's
default configuration, and searches - depth-bounded - from each connected output pin back through mixers
and selectors to a converter.

**How it fit `gs::driver`** - every wait and hold in A1, with no bending:

| Wait | Kind | Call |
|---|---|---|
| `GCTL.CRST` reads 0, then 1 | condition, 100 ms (Linux) | `wait::until` |
| held in reset | gap, spec >= 100 us | `delay::hold`, 1 ms |
| codecs announce in STATESTS | gap, spec >= 521 us: a codec that is not there never sets its bit, so no condition can say "all have answered" | `delay::hold`, 2 ms |
| immediate command free, then answered | condition, Linux 50 us | `wait::until`, 1 ms |

The STATESTS case is the one worth noting: it LOOKS like a wait and is a hold, and the library's split
between the two made the right shape the obvious one.

**Verified in QEMU** (`osdev run`, `build/audio_a1_qemu.log`):

```
audio-driver: HD Audio 1.0, 4 output / 4 input / 0 bidirectional stream(s), window 65536 bytes
audio-driver: codec(s) answered at mask 0x0001
audio-driver: codec 0: vendor 1af4 device 0012
audio-driver: codec 0 audio function group at node 0x01, widgets 0x02..0x03
audio-driver: codec 0 output path: 0x02 -> 0x03 (line out, config 0x00004010)
```

That is QEMU's model exactly: one codec, DAC 2 to line-out pin 3. Not run on the T630: A1's command path
is the optional one, and A6 is where hardware comes in.

**Not done in A1, deliberately, and recorded:**

- **Not restart-managed.** The supervisor spawns it and does not restart it. Making it restartable needs
  the kernel's two name lists (restart count, death notification), and its first DMA needs the kernel to
  stop the device's bus mastering on its death, which is also done by name today. Both are kernel
  changes, asked for at A2. (Both made after A3: see "The kernel changes audio needed".)
- **Requests are refused** with one status byte (`0xFF`): there is nothing to offer until A4.

## Steps A2 and A3: the command rings, and a tone (2026-10-03)

Built as one change and run as one, because A2 alone has nothing to show but A1's answers again.

**A2.** After A1's survey, on QEMU's codec only, the driver stops the CORB and RIRB, points them at the
arena, resets their pointers and starts them; every later verb goes through them. It proves the rings by
asking again what the survey asked - the codec's vendor - and comparing.

**A3.** It configures the path: power to D0 (and waits until the widget says it is there), output amps
unmuted at full gain, each node's selection of the next, the pin's output enable and EAPD where the pin
has it, then the converter's format (48 kHz, 16-bit, stereo) and stream tag. Then one output stream: a
four-entry buffer descriptor list over a 64 KiB ring in the arena (about a third of a second), the ring
filled, RUN, and refilled behind the stream's position every 10 ms until the tone has gone out. The tone
is a sine from a fixed-point phase accumulator - no floating point and no table.

**The DMA arena.** 24 pages, granted by the supervisor's row: CORB 1 KiB, RIRB 2 KiB, the BDL, and the
64 KiB ring. Its layout is a hand-written table of offsets - the SIXTH such table in the tree (`xhci`,
`ehci`, `nic-driver`, `dwmac`, `genet` before it), recorded at the table as a finding: arena layout is a
library candidate audio confirmed rather than tested, because it was not extracted first.

**What a death would leave running** - as first built, before the kernel change below: the kernel
stopped a dead driver's bus mastering only for the drivers it named, and this one was not named. So the
output stream would have gone on READING the ring (stale sound, no writes); the RIRB WRITES only in answer
to a verb, and a dead driver sends none; and the arena is a permanent DMA reservation, reused across the
driver's respawns and never handed back to the general pool (`kernel/src/task/mod.rs`), so even a stray
write would land in reserved memory, never a page table. That is why A2-A3 shipped before the change
was made, in QEMU only. Since the change, nothing is left running.

**How it fit `gs::driver`** - A2 and A3's waits, with no bending:

| Wait | Kind | Call |
|---|---|---|
| the ring DMA engines stop, start; the CORB read pointer resets | condition, 10 ms | `wait::until` |
| a verb's answer arrives in the RIRB, unsolicited responses passed over | condition with more than one way through, 1 s (Linux) | `Deadline::start` |
| a widget reaches D0 | condition, each look a verb, 200 ms | `until_paced`, 1 ms pace |
| the stream's SRST set and cleared; RUN cleared | condition, 10 ms | `wait::until` |
| the tone played out, refilling the ring behind it | condition, the tone's length plus a second, a look every 10 ms | `until_paced` |

The last row is worth a note. `until_paced` was written for watching a condition; here its closure also
DOES the step's work - it reads the stream's position, refills the ring behind it and reports whether the
tone has gone out. It fit without change: the deadline bounds the playback, the pace is the refill
period, and the result says whether the stream really played at the rate it was set to. A refill loop
is a paced, bounded wait whose condition is "played out", and the library already had that shape.

**A bug QEMU found, and the lesson.** The first run answered one command through the rings and then
nothing: `CORBRP` stopped at 1. The RIRB response-interrupt control bit (`RIRBCTL` bit 0) was off. With
RINTCNT at 1 the controller stops taking commands once one response is outstanding, until software clears
the response status - and that status is only ever set with the bit on. Linux sets both; A2 set only the
DMA-run bit. It is not optional in practice, whatever its name suggests, and the constant now says so.

**Where it was run.** In the BARE-METAL image (`osdev shell`, which now attaches `intel-hda` with its
sound to `build/qemu_audio.wav`), not the full test image `osdev run` builds. In the full image the kernel
found no 24 contiguous free pages by the time the driver spawned - and `nic-driver` failed to get its
arena in the same boot - after 387 spawns, most of them test probes. That belongs with item 3 of "Found
while preparing", not to audio. The bare-metal image is also the one hardware gets.

**Verified in QEMU** (`build/audio_a3_qemu.log`, `build/audio_a3_qemu.wav`):

```
audio-driver: A2 - codec commands now go through the CORB and RIRB (vendor 1af4 read back through them)
audio-driver: A3 self-test - playing 1000 Hz for 1000 ms
audio-driver: A3 self-test - played 1000 Hz for 1000 ms in 977 ms by the clock, 0 underrun(s)
```

And the capture, read back by a script that finds the sound, measures it and looks for the tone:

```
format: 48000 Hz, 2 ch, 16-bit; 46069 frames = 0.960 s captured
tone: starts 0.000 s, lasts 0.960 s, rms 11584, peak 16381
frequency: 999.7 Hz by zero crossings; power at 1000 Hz is 48716x the strongest neighbour
```

999.7 Hz; RMS 11584 against a peak of 16381 is exactly peak / sqrt(2), a clean sine at the half scale the
generator writes. **What the capture does not show:** the last 40 ms of the tone and the silence after it.
QEMU writes its WAV through a buffer and was stopped by being killed, which does not flush it; the A4 test
stops QEMU through its monitor so the tail and the silence can be checked too.

**Not done in A2-A3, and recorded:**

- **The kernel changes** (next section) - made after this step, on the operator's go-ahead.
- **The tone plays at every start**, as the step's self-test. It goes when A4 gives the driver a request
  for it.
- **No interrupt.** The refill polls; the step that adds interrupt waiting - itself a library candidate -
  changes how the wait ends, not the loop. (Done since: see "Interrupt-driven refill".)

## The kernel changes audio needed (made 2026-10-03, on the operator's go-ahead)

Two, and **neither adds a kernel responsibility** (MISCIS, CLAUDE.md 4.3): both are things the kernel
already does for other drivers, today keyed by a hand-kept list of service NAMES.

1. **Stop a dead driver's DMA by what it holds, not by its name.** On a driver's death the kernel clears
   PCI bus mastering on its device - for `xhci`, `ehci`, `block-driver` and `nic-driver`, named one by one
   (`kernel/src/task/scheduler.rs`). Keyed instead on "this task was granted a PCI device" (it has a
   BDF), it covers `audio-driver` and every PCI driver after it, and no list has to remember a new name.
   This is interrupt routing and memory isolation the kernel already enforces (I, M), applied to a
   device the capability model already granted. **It needs a second line to be safe:** the per-slot
   device record is written at a DMA driver's spawn and never cleared, so a later task reusing the slot
   inherits it. The name list hides that today; keyed on the record alone, a probe dying in a slot `xhci`
   once held would stop the live `xhci` controller. So the death path TAKES the record (reads it and
   resets it to "no device") rather than reading it.
2. **Restart management for `audio-driver`.** The kernel names the services whose deaths it counts and
   reports to the supervisor (two lists in `scheduler.rs`). Adding `audio-driver` lets the supervisor
   restart it; the same name-keyed lists could later become a property the supervisor declares.

The first is a correctness fix for every future PCI driver as much as for audio; the second is one name.
Both are re-verification work rather than new mechanism (the kernel's own rule: the cost of a kernel
change is re-verification).

**As made.** In `kernel/src/task/scheduler.rs` the death path's name check became
`take_task_hw_bdf(slot) != 0xFFFF`; the IOMMU revert inside it stays keyed on `xhci` and `ehci`, which
is a separate fact (only they are confined) - superseded below: the release is now for any device. `take_task_hw_bdf` reads the record and resets it in one
step, and a failed spawn (`cleanup_partial_spawn` in `kernel/src/task/mod.rs`) takes it too, since a
spawn can fail after recording its device. `audio-driver` joined both death lists in the kernel (superseded below: the kernel keeps no lists, and
death follows `SPAWN_FLAG_WATCHED`), the supervisor's `MANAGED` roster and its death loop (`services/supervisor/src/main.rs`).

**Verified in QEMU** (`build/audio_kill_qemu.log`, bare-metal image), killing three services over the
control channel:

```
control: KILL audio-driver
pci: BDF 0x0020 bus-master DISABLED on driver death (DMA quiesced)
supervisor: audio-driver died, restarting
pci: BDF 0x0020 bus-master + memory-space ENABLED for DMA driver spawn
supervisor: audio-driver restarted
audio-driver: A3 self-test - played 1000 Hz for 1000 ms in 990 ms by the clock, 0 underrun(s)
control: KILL time                      (drives no device: no bus-master line, as predicted)
control: KILL nic-driver
pci: BDF 0x0018 bus-master DISABLED on driver death (DMA quiesced)   (as before the change)
supervisor: nic-driver restarted
```

The capture holds both instances' tones - 1.94 s at 1000.2 Hz, RMS 11584 against a peak of 16381, the
same clean half-scale sine. QEMU's WAV grows only while a stream runs, so the two tones are back to back
with no gap between them. The quit through QEMU's escape did not flush the tail either; that waits for
A4's monitor-driven test. The identity suite passes 24 of 24 on the changed kernel
(`build/audio_identity.log`), and the Pi 2, Pi 4 and VisionFive images build: with `audio-driver` in the
supervisor's roster, `scripts/service_embed_check.py` now lists it as absent on those three ports by
design, as it does `wifi-driver` on the boards without that radio. Not covered by this run: the stale-record case itself (a slot that held a
device reused by a task that holds none), which needs slot reuse a single kill cannot arrange.

## Interrupt-driven refill (2026-10-03)

**The library first.** Interrupt waiting was a library candidate before audio began (`xhci`, `ehci` and
`dwc2` each write it), so it went into `gs::driver` as `irq` before audio used it, and audio is its
first user: `docs/driver-library.md`, step 2.

**The driver.** The spawn row asks for a vector (`hwclass::pci_irq`); the kernel picks one from its MSI
pool and programs the controller. Each of the four buffer descriptors sets IOC, the stream's IOCE is on,
and INTCTL enables the controller's interrupt for that stream alone (GIE and its SIE bit; the codec
command interrupt stays off, since the rings are waited on by polling). The refill loop waits on
`irq.wait(150 ms)`: an interrupt clears the stream's status and re-arms, a timeout is a WATCHDOG wake
(it clears the status too, because a lost interrupt leaves it set and a set status raises no new
interrupt), and a request that arrives mid-tone is answered rather than taken and dropped. A period is
about 85 ms, so a healthy stream never lets the watchdog fire. With no interrupt routed the same loop
polls every 10 ms and says so.

**Verified in QEMU** (`build/audio_irq_qemu.log`):

```
pci-msi: class 0x040300 BDF 0x0020 -> vector 0x30 (pool slot 0)
audio-driver: A3 self-test - played 1000 Hz for 1000 ms in 995 ms by the clock, 0 underrun(s)
audio-driver: refilled on 13 interrupt(s), 0 watchdog wake(s), 0 request(s) answered mid-tone
...killed, restarted (same vector):
audio-driver: A3 self-test - played 1000 Hz for 1000 ms in 1005 ms by the clock, 0 underrun(s)
audio-driver: refilled on 13 interrupt(s), 0 watchdog wake(s), 0 request(s) answered mid-tone
```

Both instances' tones are in the capture, 998.4 Hz by zero crossings over the pair and the same clean
half-scale sine. **The prediction was 12 interrupts, and it was 13 both times** - recorded rather than
explained: the likely cause is the stream position lagging its interrupt, so the twelfth look reads
just short of the tone's length, and that stays a hypothesis until the position at each interrupt is
logged.

## IOMMU confinement (2026-10-03)

**The driver is spawned confined** (`hwclass::pci_irq(0x04_03_00, 0, true)`): the IOMMU maps exactly its
arena, so the controller can reach the command rings, the buffer descriptor list and the ring of sound
and nothing else. Every DMA it makes legitimately is inside the arena, so confinement refuses nothing.

**It needed one kernel change, made on the operator's go-ahead, adding no responsibility.** The kernel
undid a device's confinement on its driver's death only for `xhci` and `ehci`, by name. Confined without
that, every restart would have re-confined the device and leaked the previous I/O page table's frames,
the respawn overwriting the record that pointed at them. The death path now releases for ANY task that
holds a device, which is exact: the release looks the device up in its own table of confined devices
and does nothing for one that was never confined. Same shape as the bus-master change before it.
Recorded in the constitution as an amendment to 6.4, which said `xhci` was the only confined driver.

**Verified in QEMU** on q35 with `-device amd-iommu` (`build/audio_iommu_qemu.log`,
`build/audio_iommu_qemu.wav`), killing the driver once over the control channel:

```
iommu: selftest PASS - arena 0x3461000/0x3478000 translate identity, 0x3479000 (outside) unmapped
iommu: confined BDF 00:04.0 -> domain 1 arena 0x3461000..0x3479000 (24 pages); DTE invalidated
audio-driver: A3 self-test - played 1000 Hz for 1000 ms in 996 ms by the clock, 0 underrun(s)
audio-driver: refilled on 13 interrupt(s), 0 watchdog wake(s), 0 request(s) answered mid-tone
pci: BDF 0x0020 bus-master DISABLED on driver death (DMA quiesced)
iommu: released BDF 00:04.0 -> DTE back to passthrough, I/O page table freed
iommu: selftest PASS - arena 0x3461000/0x3478000 translate identity, 0x3479000 (outside) unmapped
iommu: confined BDF 00:04.0 -> domain 1 arena 0x3461000..0x3479000 (24 pages); DTE invalidated
audio-driver: A3 self-test - played 1000 Hz for 1000 ms in 1006 ms by the clock, 0 underrun(s)
audio-driver: refilled on 13 interrupt(s), 0 watchdog wake(s), 0 request(s) answered mid-tone
```

QEMU was quit through its monitor this time, so the capture is whole: 1.997 s, both tones, 1000 Hz by
Goertzel (48716x the nearest neighbour) and the same half-scale sine. `osdev test iommu` (CLAUDE.md 22
Test 12, `xhci`) still passes on the changed kernel.

**What QEMU cannot show.** QEMU's `amd-iommu` does not raise a fault for an out-of-arena DMA (CLAUDE.md
22 Test 12 says why), so what is proven here is the STRUCTURE - the selftest's page-table walk - plus
the driver working through the confined domain. And the controller uses plain MSI, so its interrupt
message sits in configuration space, out of the driver's reach (`docs/iommu.md`). On the T630 the
driver does not use DMA yet (A6), so confinement there is untested.

*(Note 2026-10-09: CLAUDE.md 6.4's note of 2026-10-08 records the T630 confining `00:01.1` with an
arena - the Radeon HDMI audio controller, which is the device "Found while preparing" item 2 predicts a
class-code lookup picks first. So on the T630 the confined audio device is not the analog Azalia at
`00:09.2`, and what that confinement has been asked to carry is still untested.)*

## Step A4, first half: the protocol and the `audio` verbs (2026-10-03)

**The protocol** is `sdk/audio`'s `wire`, read by both sides, as `sdk/wifi`'s is for the radio. Every
request is tagged (`[TAGGED, tag, op, ...]`, answered `[TAGGED, tag, status, ...]`), so a late answer is
never read as the next request's (backlog/70) - in the protocol from its first byte rather than added
after. And **every answer is immediate**: `tone` starts the sound and answers, the shell follows it with
`status` every 200 ms, and `q` sends `stop`. The driver can do that because its one wait is
`gs::driver::irq`'s, which hands a request back mid-tone, so the radio's machinery for answers the shell is
OWED a slow driver has no work to do here.

**The driver became a service with state**: the path, the power, the volume, the mute, a tone if one
plays, and a serve loop in which an interrupt refills the ring and a request is answered. The start-up
self-test tone is gone; `audio tone` replaced it. The volume goes on the output amplifier nearest the
converter, as a gain linear in the amplifier's steps (which on a real codec are even steps of decibels),
and every change is read back with the matching GET verb before it is reported. Volume 0 sets the
amplifier's own mute while `muted` stays false - two states, as agreed.

**QEMU's codec changed to `mixer=on`** (`1af4:0012`), which gives it an amplifier - `mixer=off` has none, so
there was no volume to set. QEMU applies it to the samples, so the WAV shows the volume as well as the tone.

**Verified in QEMU** (`build/audio_shell_qemu.log`; the shell driven over a TCP serial port, QEMU quit
through its monitor): every verb in `utilities/57_audio.md` answered as that file says, including the
refusals (`volume 101`, `tone 5`, an unknown verb, `tone | count`, `play`, `tone` while off) and `beep`'s
hint. The capture, as runs of 100 ms:

| Asked | In the WAV |
|---|---|
| `tone 1000 1` at volume 50 (the start) | 1.0 s, RMS about 5,600 |
| `tone 1000 1` at volume 100 | 1.0 s, RMS about 11,400 - the unscaled half-scale sine |
| `tone 1000 1` at volume 0, then muted | 2.0 s of zeros |
| `tone 440 1.5` at volume 60 | 1.5 s at 440 Hz, RMS about 6,800 |
| `tone 1000 3`, `q` after about 1.2 s | 1.2 s, then nothing: `stopped after 1.2 s` |

QEMU scales the samples LINEARLY with the amplifier step (step 37 of 74 is about half the amplitude),
not in decibels as a real codec does, so its scale is louder at low volumes than hardware will be. A fact
about QEMU's model, recorded so a hardware run is not compared against it.

**Two findings on the way:**

- **QEMU's codec models no power states.** `audio off` first said FAILED: the function group was told D3
  and still read D0. Logging what it reported settled it rather than a guess - it reports supported power
  states 0, and D0 whatever it is told. So `off` now answers `unconfirmed (this codec does not report it)`
  where the codec offers no D3, a verdict of its own (`wire::UNSUPPORTED`): not a failure, and not
  claimed as verified either. `off hard` holds the controller in reset, which the controller DOES
  report, and is verified.
- **The new utility met every gate it should**, each one adding a registration: Commandment X (no spec
  under `utilities/` for a verb the shell answers), the per-verb `help` check, and `site_check` (a page and
  its counts on the website). That is the eight-sites table in `utilities/0_conventions.md` working.

## Step A4, `/audio.settings` (2026-10-03)

The volume and the mute are kept in `/audio.settings` (`utilities/57_audio.md` section 5), through
`gs::fs` rather than a third hand-built `fs` client: its wait is the kernel's `CallDeadline`, which takes
the matching reply and leaves everything else queued - so a stream interrupt or a client's request that
arrives during a settings write is still there afterwards. The driver gained one peer, `fs`, wired by the
supervisor and pinned in `COMMANDMENTS.baseline.toml` with its reason.

**Not written while a tone plays.** A write blocks the serve loop for as long as `fs` takes, and the ring
holds 341 ms of sound; a change made during a tone marks the settings dirty and is written when it ends.

**Verified in QEMU** with a formatted AHCI data disk (`build/audio_settings1_qemu.log`,
`build/audio_settings2_qemu.log`), as predicted before the runs:

```
boot 1: audio-driver: no /audio.settings yet - starting at the defaults; it is written at the first change
        audio volume 30 / audio mute / read /audio.settings  ->  volume 30, muted yes
boot 2: audio-driver: settings read from /audio.settings - volume 30, muted
        audio-driver: volume 30, muted, on node 0x02 (74 steps) - verified
        audio status -> volume 30, muted yes; unmute and volume 75 rewrote the file
```

And with no data disk (`build/audio_settings0_qemu.log`): `fs` answers "storage unavailable", the driver
says so once at the load and once at the first failed write - not again on the next change - and the
volume keeps working in memory.

## Step A4, `osdev test audio` (2026-10-03)

The checks every step above ran by hand, as one command: **`osdev test audio`**, 33 checks over two boots
on one disk formatted host-side. Boot 1 drives every built verb at the prompt and checks each answer
against `utilities/57_audio.md`; kills the driver over the control channel and checks the kernel stopped
its DMA, the supervisor restarted it and it read its settings back; then quits QEMU through its monitor,
so the WAV is whole, and READS it - about 3.2 s of tone, every block at 1000 Hz by zero crossings, a level
in the range volume 50 gives and another in the range volume 100 gives, and the muted tone silent. Boot 2
checks the volume came back from `/audio.settings` across a reboot.

**First run: 33 of 33** (`build/tests/audio_test_serial.log`, `build/tests/audio_test.wav`). It also said,
twice, that QEMU ignored `quit` and was killed instead - so the WAV might have lost its tail. The cause
was the harness, not QEMU: the monitor connection was dropped the moment `quit` was written, before
QEMU acted on it. Held open until QEMU exits, the second run quit cleanly both times, with the same 33 and
the same capture (32 sounding blocks, 9 silent) - so nothing had been lost the first time either, which
is now known rather than assumed.

## Step A5: `audio play` (2026-10-03)

**The protocol** gains three requests (`sdk/audio`): `OP_OPEN` (rate, channels, bits, length - answered
with the room in the ring), `OP_PCM` (up to 3,556 bytes of whole frames - answered at once with how many
were taken and the room left; frames past the room are not taken and the shell sends them again), and
`OP_END`. Every answer is still immediate: the shell waits on the RING, in 20 ms sleeps when it is full,
never on the driver.

**The driver** plays the stream through the same ring and interrupt as a tone. It does not start until
half the ring is written (or the end arrives), so a sender's first moments of jitter are absorbed rather
than heard. If the sender falls behind, the driver writes SILENCE ahead of the position and counts it,
rather than letting the ring replay stale sound; at the end it silences the free ring so nothing stale
plays while it drains. Mono is written to both sides. 44.1 and 48 kHz are accepted where the converter
says it takes them (its PCM parameter: QEMU's reads `0x000201fc`), and the converter and the stream are
set to the same rate, or the sound would play at the wrong speed. A stream nobody feeds for five seconds
is ended - a shell killed mid-file must not hold the device.

**The shell** reads the WAV header (`RIFF`/`WAVE`, `fmt `, `data`; PCM or extensible-PCM), refuses
anything it cannot play with the reason, and streams the file in `IO_CHUNK` reads. `q` sends `stop`.

**Verified by `osdev test audio`** (now 42 checks): `/song.wav` (2 s, 48 kHz stereo, 660 Hz) and
`/mono.wav` (1 s, 44.1 kHz mono, 330 Hz), generated by the test and baked onto its disk, played with 0
underruns and 0 ms of silence; a 24-bit and an 8 kHz file were refused with their reasons; `q` stopped a
file mid-way. The capture holds 32 blocks at 660 Hz and 10 at 330 Hz as well as the tones.

**Two of my checks were wrong on the first run, not the sound.** One looked for the word "silence" on the
serial port, where the driver's own log line ("0 ms of silence") also arrives. The other allowed only two
blocks at frequencies nobody asked for, and found five. Measured before changing anything: each of the
five is the 100 ms block that straddles a JOIN - 1000 Hz to silence, 1000 to 660, 660 to 330, 330 to 660 -
so its crossings come from two sounds. The check now requires every such block to sit at a join.
A later run found the rule still too narrow, again measured before it was changed: a join between the
volume-50 and volume-100 tones (the same frequency, a different LEVEL, phases not lined up) and a
660-to-330 join that spread over two blocks. A sound is now (frequency, level), and an odd block passes
only in a run of at most two between sounds that differ; a glitch planted inside a steady 660 Hz run is
still caught - checked by replaying the rule over the capture with one block altered.

## The Pis and the VisionFive (researched 2026-10-03; the Pis' jack built the same day and HEARD on a Pi 4 that night)

Researched from Circle (a bare-metal Raspberry Pi library), the BCM2835 and BCM2711 datasheets, the
Raspberry Pi device trees and StarFive's vendor kernel. The full notes, each item marked quoted or
inferred, were the input to the kernel proposal below.

**Pi 2 and Pi 4 share one PWM driver with three values changed:**

| | Pi 2 | Pi 4 |
|---|---|---|
| PWM block | PWM0, ARM `0x3F20C000` | PWM1, ARM `0xFE20C800` |
| Jack pins, alt 0 | GPIO 40 right, GPIO 45 left | GPIO 40 right, GPIO 41 left |
| PWM clock | PLLD 500 MHz / 2 = 250 MHz | PLLD 750 MHz / 6 = 125 MHz |
| DMA request line | 5 | 1 |

The DMA engine sees RAM at `0xC0000000 | phys` on both, so the arena must be below 1 GiB on the Pi 4.
The resolution is about 12.5 bits on the Pi 2 and 11.5 on the Pi 4 at 44.1 kHz - the jack's own limit.

**What a driver would need is the authority question.** Four 4 KiB pages are involved and three are
SHARED: the DMA page holds all fifteen channels and their common interrupt status; the GPIO page every
pin; the clock manager page every clock. Only the PWM page is audio's alone. So the shape is the one the
Pi 4's `DevicePower` already set: the kernel does the shared parts (the pin mux, the PWM clock) as part
of the grant, at spawn, and the driver is granted the PWM page and the DMA arena. The DMA channel page is
the open item: granting it hands over every channel, which on these boards (no IOMMU) is no more DMA
reach than the driver already has, but it is still more than the grant names. A proposal, not made.

### Built: `pwm-audio` and the kernel's part (2026-10-03, on the operator's go-ahead)

**The kernel** (CLAUDE.md 12.3, the 2026-10-03 amendment): a new device kind, `HwClass::AudioPwm`, that
every port answers through the seam (`fixed_device_present`) - true on the Pi 2, true on the Pi 4 only where a
boot probe found PWM1 answering. Granting it, the Pi's arch layer routes the jack's two pins to the PWM
(pulls off) and starts the PWM clock from PLLD (/2 on the Pi 2, /6 on the Pi 4), then maps the PWM page
at +0 and the DMA engine's page at +0x1000 as one 8 KiB window, and grants a 36-page DMA arena kept across
respawns. `pwm-audio` joined the two restart lists - and that is what the operator stopped, the same
day: see "No service names in the kernel" below. `hw_class_known` is now DERIVED from the class decoder:
it read `class <= 7`, a second copy of the decoder's range, and refused every spawn of the eighth kind
until the first boot showed it.

**The driver** (`services/pwm-audio`) speaks `sdk/audio`'s protocol, so `audio` is the same on every
board, and shares the test tone and `/audio.settings` with `audio-driver` - both moved into `sdk/audio`
when the second driver needed them. The DMA engine loops a 16-period ring (about 370 ms at 44.1 kHz)
that holds mid-scale silence when nothing plays, so sound starts and stops without a click; the driver
writes ahead of the engine's source address, polled every 10 ms. Volume is a square law applied to the
samples. A stream begins half a ring ahead of the engine, the cushion `audio-driver` gets by not starting
until half its ring is full. Which Pi it runs on comes from the supervisor's spawn row (`mode` 2 or 4),
declared once in the supervisor's per-board build facts.

**The PACING CHECK, which QEMU found.** Before looping the ring, the driver plays ONE period through a
control block that ends, and times it: a PWM that asks for its words at the sample rate takes about 23 ms.
That one measurement proves the clock, the pins, the DMA request line and the engine together, at the
right rate. Too fast means the engine is not paced at all and no ring is looped into it; never finishing
means the PWM never asked for data (its clock is not running).

**Verified in QEMU, which is all QEMU can verify here** (`raspi4b` and `raspi2b` model no working PWM
audio):

- **Pi 4:** the boot probe finds no PWM1 (`audio: no PWM1 at 0xFE20C800 ... no audio jack`), nothing is
  granted, the driver serves "no device" and `audio status` says `no audio hardware on this machine`. The
  first boot, before the probe existed, showed why it is needed: the driver's first write to the absent
  block was an external abort, and the supervisor respawned it forever.
- **Pi 2:** `raspi2b`'s PWM holds its registers (CTL reads back `0xa1e1`), but its DMA model runs a
  transfer to its end inside the write that starts it. The first boot looped a ring into it and the whole
  emulated machine froze at that write - found by trace lines, one per step, the last one printed being
  "writing CS START". With the pacing check the driver measures `one period went in 0 us where a paced
  PWM takes 23219 us`, refuses, and the machine boots to its prompt.
- Every port builds with every check; the x86 identity suite and `osdev test audio` pass on the changed
  kernel.

**What only the hardware can show, predicted before it is flashed.** On a real Pi 4 the boot log should
read `audio: PWM1 present`, then `jack pins 40/41 on PWM1, PWM clock PLLD/6`, then `pwm-audio: paced - one
period of 1024 frames took` about 23,200 us, then `Pi 4 jack up - PWM at 44100 Hz, range 2834 (about 11
bits)`. `audio tone 440 2` should be heard in headphones or powered speakers in the 3.5 mm jack, with no
click at its start or end; `audio volume 20` audibly quieter; `audio play` of a WAV on the disk heard
whole. On the Pi 2 the same, with pins 40/45, PLLD/2 and range 5669 (about 12 bits). If the pacing check
measures a period far from 23 ms, the PWM clock is not what the driver assumes, and the number says by
how much.

**Heard on a Pi 4, 2026-10-03, at `fd55d331`.** Headphones in the jack, every verb as predicted: `audio
tone 440 2` audible with no click at either end, `audio volume` audibly changing the level, `audio mute` and
`audio unmute` silencing and restoring it. The boot log read exactly the four predicted lines, and the
pacing check measured `one period of 1024 frames took 23102 us (expected 23219 us)` - the PWM runs 0.5%
fast, about 442 Hz for a 440 Hz tone, which is below what an ear notices and is the PLLD/6 divider being
an integer rather than anything wrong. The 2 s tone took 2014 ms by the clock with 1 underrun. After `kill
pwm-audio` the respawned driver read `volume 20` back from `/audio.settings`. The Pi 2's jack is built
the same way and has not yet been heard.

**Found the same night, not explained yet.** In the `chaos max-carnage` that followed, the SHELL took two
EL0 faults in four rounds - an instruction abort, then a write to a read-only page in its own text - each
1 to 7 ms after another service finished spawning, which ended the storm. Neither is the `0x2020...`
fingerprint of the four earlier Pi 4 shell sightings, and the Pi 4 had thrown one such fault per 100
rounds in August, before any audio code existed. The jack's DMA is RULED OUT by construction rather than
by assumption: its arena is reserved once and never recycled, `start` resets the channel and waits for it
before touching a control block, and every control block's destination is the PWM FIFO, so the engine
reads only its own arena and writes only to one peripheral register. **Not audio's, and not yet explained.** A
first hypothesis - the AArch64 context switch skipping its TLB flush when a respawn was handed a recycled
page-table root - was fixed and tested on the card, and the faults continued. `backlog/72` carries the
evidence, what is ruled out (the CPU clock, with `force_turbo=1`; this DMA; that TLB flush), and the
instrument that comes next.

**HDMI on the Pis** - the operator's TV is a better test than headphones. The firmware sets HDMI up at
boot (our console uses it), and Circle drives HDMI audio bare-metal on Pi 1 to 4 by feeding the HDMI
audio block by DMA. Same DMA engine, so the same authority question; to be researched as closely as PWM
was before anything is proposed.

**VisionFive 2 Lite: no analog audio at all.** The vendor device tree disables its PWM-DAC and reuses the
old left-channel pin as the Wi-Fi enable, and StarFive's own Linux on this board registers no sound card.
HDMI is the only path - an I2S transmitter feeding the Inno HDMI transmitter - and it needs the display
controller, its clocks, a power domain and two PMIC rails brought up first, on IP documented only in the
vendor kernel. Weeks, not days, and with nothing working on the board to compare against. A USB audio
dongle is the realistic route there - and it would work on every board - but it needs isochronous
transfers, which no USB driver here does yet.

## No service names in the kernel (2026-10-03)

Writing the Pis' jack, the kernel learned one more service name: `pwm-audio` went into the Pis'
fixed-window table and into two lists in the death path, which is how `wifi-driver`, `dwc2`, `nic-driver`
and `block-driver` had got their windows before it. The operator asked whether that was normal, given that
the only service the kernel is supposed to know is the supervisor. It was not, and "the existing practice"
was the debt rather than a licence for it. `docs/service-ownership.md` had already said why: once the
supervisor supplies the images, a table that grants authority BY NAME cannot be enforced, because any
image the supervisor starts under a matching name gets the device.

So everything the kernel decided by a service's name is now decided by something the spawn request
carries:

| What | Was decided by | Now decided by |
|---|---|---|
| A fixed device's window (Pi 2 `dwc2`, audio; Pi 4 GENET, the radio, audio) | the name, in map_fixed_driver_mmio (removed) | the device KIND in the request (`HwClass` -> `task::kind`), in `arch::imp::map_fixed_device` |
| Whether the device is there | audio_pwm_present (removed) | `arch::imp::fixed_device_present(kind)` |
| Who may cut a device's power (`DevicePower`) | the caller's name, `wifi-driver` | the kind the caller was granted, `WIFI_SDIO` |
| Whose death is reported to the supervisor | a list of 19 names | `SPAWN_FLAG_WATCHED`, set by the supervisor from `MANAGED` |
| Whose death counts as a restart | a second list of names | the same flag |
| Who gets the display back on death, and where console output goes | the name `console` | the task granted the `FRAMEBUFFER` kind |

**The radio got a kind of its own**, `WIFI_SDIO`, because it had none: its supervisor row said
`hwclass::NONE` and the kernel recognised it by name. A kind is the same trust as a PCI class code (step
D): it says what DEVICE a driver is for, which is what the grant is about.

**Two grants were removed rather than converted**, because nothing used them. `block-driver` was given the
Pi 2's Arasan EMMC window by name, and on that board it never reads it - the disk is a USB stick behind
`dwc2`, and the EMMC is the boot card, which writing to destroyed two of them. And `console_push` was
minted by matching a list of names that no longer named anything that spawned through that path, so it
granted nothing; the drivers hold the privilege bit `CONSOLE_PUSH` from their rows, as before.

**What the kernel still knows by name: `supervisor`**, because it spawns and respawns it, and a kernel
that is the recovery anchor must be able to name the one thing it recovers. The catalogue entries that
still exist for a direct spawn are the same exception.

**The gate followed the fact.** `V-managed-watched` compared three lists; there is one now, so it checks
the chain instead - `MANAGED` parses, `is_watched` answers from it, the spawn request sets the flag from
it, the syscall carries it, the spawn records it, the death path guards both actions on the record, and
no list of service names is back in that path. Each link has a probe that breaks it.

## Found while preparing

Recorded here because they were found on the way and do not belong to audio alone.

1. **Every PCI driver gets a fixed 64 KiB register window, whatever its device has**
   (`kernel/src/task/mod.rs`, `XHCI_MMIO_PAGES` mapped for every PCI driver). An HDA controller has 16 KiB;
   on the T630 the FCH audio window at `0xfeb60000` plus 64 KiB covers the HDMI audio controller's
   registers at `0xfeb64000` too. That is more authority than the grant names (CLAUDE.md 3.1), and it
   may already apply to today's PCI drivers. Not yet checked against them; the fix is to size the BAR.
2. **A class code picks the FIRST matching device** (`kernel/src/arch/x86_64/pci.rs`, `find_by_class`).
   The T630 has two class-0x0403 controllers and the first is the HDMI one, not the analog one. A
   supplied BDF selects the device for bus mastering but not, yet, for the window, the arena or the
   vector (`docs/service-ownership.md` D3). A6 needs that finished. QEMU, with one controller, cannot
   show either problem.
3. **In the full QEMU test image (`osdev run`), late boot spawns report `from image FAILED` while the
   service runs.** Nine services print `supervisor: spawn '<name>' from image FAILED (InvalidArgument)`
   after the kernel has logged `spawned OK` (`control`, `power`, `hw-enumerator`, `block-driver`,
   `shell`, `xhci`, `audio-driver`, `nic-driver`, `net-stack`), and the supervisor's name map ends with 4
   services. The kernel's `SpawnImage` returns an error only after the task exists, at the point it puts
   the new endpoint's capability into the supervisor's table, which suggests that table is full after
   the test probes. Not in the bare-metal T630 logs. Not yet investigated; not caused by audio.
4. **Three build paths ran no checks** - `osdev shell`, `osdev test` and `scripts/pi4_build.py` - and the
   first two are the ones every QEMU run here used. Asking for the interrupt changed the spawn row and
   not the contract; that mismatch was booted, tested and built into a Pi 4 image cleanly, and was
   refused only by the Pi 2 and VisionFive scripts. All three now run the same checks as `osdev build`,
   each shown to refuse by putting the mismatch back (2026-10-03). The Pi 4 one matters most: it builds
   the images the Wi-Fi work flashes.

## Step A4: `audio outputs` and `audio output` (2026-10-09, `feat/audio-finish`)

**The survey keeps every output, not the first.** It used to return the first pin with a path to a
converter and forget the rest; it now records every such pin of the first codec that has one (up to
`wire::OUTPUTS_MAX`, the rest said in the log), and plays through the first until told otherwise. Two
ops join the protocol: `OP_OUTPUTS` lists each output's pin, its default-device field, whether it is the
one playing and whether anything is plugged in, and `OP_OUTPUT` chooses one by pin.

**Presence is the pin's own report, or "cannot tell".** A pin whose capabilities say it can sense a jack
is asked (`GET_PIN_SENSE`, with the `SET_PIN_SENSE` trigger first where the pin needs one); a pin that
cannot is listed as `cannot tell`, never guessed. QEMU's `hda-output` pin cannot.

**Choosing an output** switches the old pin's output enable and EAPD off, configures the new path exactly
as bring-up does (power, amplifiers, selections, output enable, EAPD, converter format), puts the volume
on the new path's amplifier, and reads the new pin back: its output enable set is `verified`. Refused while
something plays - a path changed under a running stream cuts it mid-sound - and while audio is off.

**Kept across a reboot** as `output <name>` in `/audio.settings`, written only once an output has been
chosen. At boot the driver restores it if the codec still has an output of that kind, and says either way.
`pwm-audio` answers the same two ops for the Pis' one jack (`headphone`, `cannot tell`).

**What QEMU can show, and what it cannot.** Its codec has one output, a line out, so `osdev test audio`
checks the list, the in-use mark, the pipe as records, that choosing the output in use sends nothing, and
that an output the codec lacks is refused with the list - 46 checks, all passing. Switching between two
outputs needs a codec with two: the T630's ALC255 has a headphone jack, a speaker and a line out, and is
A6's.

## Step A4: `audio hardware` (2026-10-09, `feat/audio-finish`)

**Built from `hardware`'s own gathering**, so the two commands can never disagree about what is on the
machine or who drives it: the rows are `hardware`'s multimedia-class PCI devices and the Pis' jack, and
the device `audio` talks to is the one the running audio driver was given - which `hardware` marks by the
rule the supervisor spawns by, the first device of the driver's class. For that one device the state is
the driver's own word, read live: `ready`, `off`, or why it stopped (`surveyed, not played (A6)` on the
T630 today). Every other device's state is `hardware`'s: `not driven` with the driver that took the first
of its class, or `no driver`. `audio hardware <device>` adds `hardware why`'s reason and `hardware
<device>`'s registers and grant, and `audio info` for the one in use.

**It answers whether or not an audio driver runs**, because the case it exists for is a driver holding
the wrong device - and a machine with no audio driver at all still has its devices listed.

**QEMU has one controller**, so `osdev test audio` shows the row, the mark, the pipe, one device in full
and a refused name (50 checks, all passing). The T630 is the machine with two, and is where this shows
`backlog/80` K2 in one line.

## Step A4: `audio debug` (2026-10-09, `feat/audio-finish`)

**One op, five views, paged.** `OP_DEBUG` takes a view and a page and answers labelled lines of text, at
most `wire::DEBUG_PAGE` bytes at a time with a flag saying whether more follows. The driver renders the
WHOLE view for every page and cuts the page from it (`wire::Page`, shared by both drivers), so it holds
nothing between an asker's pages - an asker that never comes back for page 2 costs nothing. Pages are
bounded too (`DEBUG_PAGES_MAX`), so a reader looping on "more" cannot loop forever.

**What each view reads, on the HD Audio driver:**
- `stats` - verbs sent and unanswered (counted in the one place every verb goes, `Hda::send`), how
  commands travel (rings or immediate), interrupts taken, underruns, what is playing and how far the ring
  is ahead, and the LAST sound: how much it played against how long that took by the clock, as a percent
  of real time. That last line is the one that proves the DMA engine and the link clock run at the rate
  they were set to; QEMU's TCG clock is not the device's, so the test does not assert its value.
- `codec` - every widget of the codec's audio function group: type, capabilities, output amplifier,
  connections and the selected one, and for a pin its configuration default, pin capabilities and
  control, with the pin playing marked. Read live, with verbs.
- `stream` - the output stream descriptor's control, status, position, length, last valid index and
  format, the BDL's address and every entry.
- `trace` - the last 64 verbs and their answers, from a fixed ring in `Hda`, decoded by shape: a 12-bit
  verb (0x7.. and 0xF..) with an 8-bit payload, or a 4-bit verb (format, amplifier) with a 16-bit one.
- `registers` - GCAP, the version, GCTL, STATESTS, INTCTL, INTSTS, the wall clock, and the CORB and RIRB.

**The views outlive a stopped bring-up.** On the T630 the driver surveys the ALC255 and stops before
playback (A6). It used to keep nothing past that point, so the dump A6 begins from was unavailable on
exactly the machine that needs it. The driver now keeps the surveyed controller (`Device::Surveyed`):
every request is still answered with why there is no device, except `debug codec`, `trace` and
`registers`, which answer as on a working one. `audio debug codec | write /codec.txt` on the T630 is the
first step of A6.

**On the Pis** `pwm-audio` answers all five: `stats` with the clock, the PWM range and the pacing check
its start-up already measured (kept now, not only logged), `stream` with the DMA channel's control, its
control block and where in the ring it is reading, `registers` with the PWM's and the channel's, and
`codec` and `trace` with one line each saying a PWM jack has neither.

`osdev test audio` runs every view and the pipe: 57 checks, all passing.

## Step A4: system sounds (2026-10-09, `feat/audio-finish`)

**Built as the spec above describes, with these decisions made while building it:**

- **The driver decides, every sender only asks.** `OP_SOUND` names a sound; the driver plays it only
  when the system sounds are on, audio is on, nothing else is playing, and `wire::SOUND_GAP_MS` (500 ms)
  has passed since the last. The switch and the gap therefore live in ONE place for every sender,
  rather than in each.
- **No sender waits.** The request is sent by `try_send` with no reply capability, so the driver answers
  nobody: no audio driver, a driver mid-restart, a full queue or the sounds switched off all mean no
  sound, never a delay - an error is printed at once either way.
- **The sounds are written once** (`sdk/audio`'s `sounds`), as short sequences of our own tones with a
  3 ms fade at each end, so both drivers play the same sounds and neither clicks. Each is shorter than
  either driver's ring, so it is rendered whole before it starts and never needs refilling. Mute and the
  volume apply as to anything else. A finished sound writes no log line.
- **Three senders, each the one that knows.** The SHELL, after a command typed at the prompt returns an
  error (`Denied` is the refusal sound, anything else the error sound) - never in a script, because a
  script's commands do not pass through the prompt. `COPIER`, once per job when it reaches done, wherever
  the five kinds finish (`board::COPIER_PEERS` gives it the board's audio driver; its contract and
  `COMMANDMENTS.baseline.toml` pin the grant). The SUPERVISOR, when it starts or stops a USB device's
  driver because the device arrived or left - and only after boot, since a device reported at boot was
  already plugged in.
- **Not built: the `/sounds/<name>.wav` override.** Reading a file on every error is the round trip to
  `fs` the built-in sounds exist to avoid, and it is not needed for the sounds to be useful. Recorded
  rather than half-done.
- **A limit, said:** only USB devices the supervisor starts a driver for make a plug sound - today the
  WiFi dongle. A keyboard or a disk is bound inside its USB host and is never reported to it.

**What QEMU shows.** `osdev test audio` switches the sounds off for boot 1 (that session types commands
that fail on purpose, and its capture must hold only the tones asked for), checks the switch, `already
off`, the status line and that `off` survives the reboot; then in boot 2 switches them on, types a
refused command, and finds the 220 Hz refusal in that boot's capture. 62 checks, all passing. The "done"
and plug sounds need a background job and a USB dongle with an audio device present, and are owed a
hardware check.

## Step A4: the keyboard shortcuts (2026-10-09, `feat/audio-finish`)

**The Ctrl+Alt+Del pattern, as agreed.** The keyboard drivers decode the chord and put a signal byte on
the console stream (`hid::VOLUME_UP_SIGNAL`, `VOLUME_DOWN_SIGNAL`, `MUTE_TOGGLE_SIGNAL`, 0x81-0x83,
outside ASCII); the shell, which reads the console, does the same request the typed verb would, so the
driver reads it back and keeps it in `/audio.settings`. No keyboard driver gains a capability or speaks to
the audio driver.

**In the decoder, not in each driver.** The chord is recognised inside `hid::emit_key`, which both a fresh
key press and the auto-repeat pass through - so it works in `xhci`, `ehci` and `dwc2` alike with no change
to any of them, a held Ctrl+Alt+Up sweeps the volume, and Ctrl+Alt+M is kept out of auto-repeat so a held
key does not flap the mute. Ctrl+Alt+M is no longer the Ctrl+M carriage return it would otherwise be, and
Ctrl+Alt+Up no longer a cursor-up; every other key means what it did. Host-tested (`sdk/rust/src/hid.rs`).

**The notice, at the prompt**, is one line above it, as read back - `volume 65  [#############-------]`,
`volume 0 - silent`, `muted (volume 65)` - with the prompt and the half-typed line put back under it. A
press while that notice is still the line above overwrites it, so holding the keys gives one line; any
other key ends that, since it may have moved the screen. When it cannot be done, the line says why: `no
audio hardware on this machine`, or `audio: the audio driver is not answering`.

**During `audio tone` and `audio play`** the spec has the volume join the status line those commands
redraw. They redraw none - each prints its line once - so a shortcut there is done at once and said on a
line of its own. Recorded as the difference from the spec rather than papered over.

**`gs::io::keys`** re-exports the four signal bytes, so the shell names them from the standard library and
the drivers from the SDK - one definition. It also closed one of the gaps `stdlib_gap_check` counts (the
shell's Ctrl+Alt+Del byte), and that check's baseline went from 8 to 7.

**What QEMU shows.** It cannot press the chord, so `osdev test audio` sends the signal bytes down the serial
line - the byte a keyboard driver would put on the console - and checks up, down, mute and unmute as read
back, and that `audio status` agrees: 67 checks, all passing. The chord itself is owed a hardware check on
a real keyboard.

## The Dell Wyse 5070's audio, as `audio debug` read it (2026-10-09)

The A4 hardware card on the Wyse, and the first dump `Device::Surveyed` was kept for. The driver
surveyed the codec and stopped, as it must on a codec playback has not been verified on; `audio
hardware` showed `00:0e.0  HD audio  Intel 8086:3198  audio-driver  surveyed, not played (A6)`, every
other verb answered with that reason, and Ctrl+Alt+Up and Down reached the shell from a real keyboard,
repeating when held. `selfcheck` ran 537 with 0 failed before and after a 100-round `chaos max-carnage
all-services`. Saved on the Wyse's disk as `/wyse-codec.txt`.

`audio debug codec`:

```
codec 0 - vendor 10ec device 0225, audio function group 0x01
widgets      0x02..0x24 (35); function group power D0
node 0x02  output      caps 0x0000041d  amp-out 0x00025757 (87 steps)
node 0x03  output      caps 0x0000041d  amp-out 0x00025757 (87 steps)
node 0x04  vendor      caps 0x00f00000
node 0x05  vendor      caps 0x00f00000
node 0x06  output      caps 0x00000411
node 0x07  input       caps 0x0010051b  from 0x24
node 0x08  input       caps 0x0010051b  from 0x23
node 0x09  input       caps 0x0010051b  from 0x22
node 0x0a  vendor      caps 0x00f00000
node 0x0b  vendor      caps 0x00f00000
node 0x0c  vendor      caps 0x00f00000
node 0x0d  vendor      caps 0x00f00000
node 0x0e  vendor      caps 0x00f00000
node 0x0f  vendor      caps 0x00f00000
node 0x10  vendor      caps 0x00f00000
node 0x11  vendor      caps 0x00f00000
node 0x12  pin         caps 0x0040040b  config 0x40000000 (line out, nothing attached)  pincap 0x00000020  control 0x00
node 0x13  pin         caps 0x0040040b  config 0x411111f0 (speaker, nothing attached)  pincap 0x00000020  control 0x00
node 0x14  pin         caps 0x0040058d  amp-out 0x80000000 (0 steps)  config 0x90170110 (speaker)  pincap 0x00010014  control 0x00  from 0x02
node 0x15  vendor      caps 0x00f00000
node 0x16  pin         caps 0x0040058d  amp-out 0x80000000 (0 steps)  config 0x411111f0 (speaker, nothing attached)  pincap 0x0000001c  control 0x00  from 0x02 0x03 (selected 0)
node 0x17  pin         caps 0x0040058d  amp-out 0x80000000 (0 steps)  config 0x411111f0 (speaker, nothing attached)  pincap 0x0000001c  control 0x00  from 0x02 0x03 0x06 (selected 0)
node 0x18  pin         caps 0x0040048b  config 0x411111f0 (speaker, nothing attached)  pincap 0x00000024  control 0x00
node 0x19  pin         caps 0x0040048b  config 0x411111f0 (speaker, nothing attached)  pincap 0x00003724  control 0x20
node 0x1a  pin         caps 0x0040048b  config 0x411111f0 (speaker, nothing attached)  pincap 0x00003724  control 0x00
node 0x1b  pin         caps 0x0040058f  amp-out 0x80000000 (0 steps)  config 0x02011020 (line out)  pincap 0x00013734  control 0x00  from 0x02 0x03 (selected 0)
node 0x1c  vendor      caps 0x00f00000
node 0x1d  pin         caps 0x00400400  config 0x40438029 (S/PDIF out, nothing attached)  pincap 0x00000020  control 0x20
node 0x1e  pin         caps 0x00400501  config 0x411111f0 (speaker, nothing attached)  pincap 0x00000010  control 0x40  from 0x06
node 0x1f  vendor      caps 0x00f00000
node 0x20  vendor      caps 0x00f00040
node 0x21  pin         caps 0x0040058d  amp-out 0x80000000 (0 steps)  config 0x0221101f (headphone)  pincap 0x0001001c  control 0x00  from 0x02 0x03 (selected 1)
node 0x22  mixer       caps 0x0020010b  from 0x19 0x1a 0x1b 0x1d 0x13 (selected 0)
node 0x23  mixer       caps 0x0020010b  from 0x19 0x1a 0x1b 0x1d 0x12 (selected 0)
node 0x24  selector    caps 0x00300101  from 0x12 0x13 0x18 (selected 0)
```

`audio debug registers`:

```
GCAP      0x6701: 6 output, 7 input, 0 bidirectional stream(s), 64-bit true
VMAJ.VMIN 1.0
GCTL      0x00000001 (bit 0: out of reset)
STATESTS  0x0005 (a bit per codec that answered)
INTCTL    0x00000000
INTSTS    0x40000000
WALCLK    0x4a633987
CORB      WP 0x0000 RP 0x0000 CTL 0x00 SIZE 0x42
RIRB      WP 0x0000 CTL 0x00 STS 0x00 SIZE 0x42 RINTCNT 0
window    65536 bytes
```

**What it says, read off the dump rather than a reference:**

- **One audio controller, and it is Intel's** (`hardware` lists one class-0x0403 device, 00:0e.0). So
  the first-of-class problem (Found while preparing, 2) does not bite here, and the AMD snoop bit is
  not a question on this machine. Codec 0 is a Realtek 10ec:0225; codec 2, Intel 8086:280d, is the
  display's audio and offers no path this driver uses.
- **Immediate Command works on this controller**: the whole survey ran on it with CORB and RIRB
  stopped (`CTL 0x00`). Across the boot and 25 restarts under chaos, two verbs went unanswered, both
  while chaos was killing services around a driver that was mid-survey.
- **Two converters, 0x02 and 0x03**, each with an 87-step output amplifier. The internal speaker, pin
  0x14, connects only to 0x02. The headphone jack 0x21 and line out 0x1b can take either, and the
  firmware left 0x21 on 0x03.
- **Every output pin was left disabled** (`control 0x00`), so whatever plays has to enable the pin it
  uses - which A3 already does on QEMU's codec.
- **The speaker, headphone and line-out pins are EAPD-capable** (bit 16 of each pincap), so their
  external amplifiers have to be switched on too. The pins carry no amplifier steps of their own, only
  a mute (`amp-out 0x80000000`).
- **Node 0x20 is a vendor widget.** Whether this codec needs vendor coefficients set before it makes a
  sound has not been checked against a reference.

**What this changes for A6.** The plan names the T630, which needs the two kernel fixes and the AMD
snoop bit before its codec is even the question. On the evidence above the Wyse needs neither the
first-of-class fix nor the snoop bit; what is left is the codec itself - pins, EAPD, and the open
question of node 0x20. Which machine A6 starts on is the operator's choice; this records that the Wyse
is the shorter road.

## Step A6 on the Wyse 5070, first image (2026-10-09)

**The change.** The driver played only on QEMU's codec. It now plays on any codec in `PLAYABLE`, a table
of `(vendor, device, coefficients)`, and the Wyse's Realtek `10ec:0225` is the second row. Nothing else
about the path is new: the survey, the command rings, power, amplifiers, the pin's output enable and
EAPD were already applied to whatever path the survey found.

**What Linux does for this machine, read rather than assumed** (`sound/pci/hda/hda_intel.c` and
`patch_realtek.c`, 6.12):

- The controller, `8086:3198`, is Gemini Lake: `AZX_DRIVER_SKL | AZX_DCAPS_INTEL_BROXTON`. Of what that
  turns on, two touch configuration space, which this driver cannot write: the snoop bit (`DEVC`, 0x78
  bit 11, written only if it is not already as wanted) and a clock-gating bit cleared around controller
  reset (`CGCTL`, 0x48). The reset already works here - the codecs answered - so the second is not in
  the way. The first is read on the card instead (`hardware 00:0e.0 debug`). `bxt_reduce_dma_latency` is
  for Apollo Lake only, and the link-clock setup applies only where the clock is still at 6 MHz.
- The codec, `10ec:0225`, gets three processing coefficients at probe, through vendor widget 0x20
  (`alc_fill_eapd_coef`): 0x67 bits 15:12 to 3, 0x36 bit 13 clear, 0x10 bit 9 clear. They are set before
  the path is configured, each read back and logged, and again after a hard off, since a link reset
  resets the codec. `alc225_init`'s headphone sequence is not done: this image plays through the
  internal speaker, pin 0x14, the first path the survey finds.

**The prediction, written before the boot:**

- The log says `codec commands now go through the CORB and RIRB`, then names three coefficients, each
  `read back the same`, then `audio-driver: ready`. `audio hardware` says `ready`, not `surveyed`.
- `audio tone 440` is HEARD from the Wyse's own speaker, and `audio status` shows it playing with the
  underrun count at 0.
- `audio volume 20` and then `audio volume 80` are audibly different; `audio mute` silences it.

**What would falsify it, and what each says:**

- A coefficient `DIFFERENT` or `did not answer`: the vendor widget is not where Linux expects it on this
  codec, and the bring-up stops there by design.
- `ready`, the stream running (`audio debug stream`: the link position moving) and SILENCE: the DMA is
  fine and the sound is lost after the converter - the speaker amplifier, a coefficient, or
  `alc225_init`'s sequence. If the position does not move, it is the controller: read `DEVC` first.
- `ready` and noise or clicks instead of a tone: the snoop bit, read from `hardware 00:0e.0 debug`.

## Step A6 on the Wyse 5070: HEARD (2026-10-09)

**The prediction held, all of it except the mute.** The rings came up, the three coefficients were each
`read back the same` (0x67 and 0x10 already as Linux wants them; 0x36 went 0x77d7 -> 0x57d7), the
driver said `ready`, and `audio hardware` said `ready`. `audio tone 440` was HEARD from the Wyse's own
speaker - a clean tone, the operator's words - the first sound this HDA driver has made on hardware.
`played 440 Hz for 2000 ms in 2028 ms by the clock, 0 underrun(s); 23 interrupt(s), 0 watchdog
wake(s)`: one interrupt per 85 ms period, so the stream ran on its MSI in real time. A 10 s tone at
880 Hz stopped on `q`, and volume 20 was quieter.

**The snoop bit was the wrong way and did not matter.** Configuration offset 0x78 read `0x2800`: bit 11
set, the device permitted to read without snooping, which Linux clears on this controller. The tone was
clean anyway, so on this machine the controller is not reading stale samples. Recorded rather than
explained: why it does not matter here is not known, and it is the first thing to look at if a later
machine plays noise.

**The mute did not work, and the driver said so.** `muted FAILED - the codec reads back something else`,
with the serial line `node 0x02 amplifier reads 0x11 after 0x91 was set`. The driver muted on the volume
amplifier, the converter's, and this codec's converter amplifier has no mute: bit 31 of its capabilities
(`0x00025757`) is clear, so the codec dropped the bit. QEMU's converter amplifier has one, which is why
QEMU could not show it. The read-back is what caught it.

**The fix:** the mute goes on an amplifier that has one. The volume stays on the converter's amplifier;
the mute is set there if it can mute, else on the nearest amplifier towards the pin that can - here the
speaker pin 0x14, `amp-out 0x80000000`, a mute and no steps - and if none on the path can, silence is
the volume amplifier's lowest step, said in the log. Both are read back. The log says where the mute
went (`node 0x02 has no mute - muting on node 0x14`).

**Prediction for the next card:** that log line at boot; `audio mute` answers `muted - verified` and a
tone is silent; `audio unmute` brings it back at the same level; volume 0 is silent too.

**The card (2026-10-09, same day): mute HARDWARE-VERIFIED on the Wyse.** Every line of the prediction
held: `node 0x02 has no mute - muting on node 0x14` at boot, `muted - verified` and the tone silent,
`unmuted - volume 50 - verified` and the tone back at the same level, `volume 0 - silent - verified`
and silent, `volume 50 - verified`. Each tone played 2000 ms in 2028 by the clock, 0 underruns, 23
interrupts. The operator: "mute works and all the commands too". A6 is done on the Wyse; the T630 is
what remains of it - the two kernel fixes and the AMD snoop bit come before its codec is the question.

## Step A6 on the T630, K2: the controller the driver is granted (2026-10-09)

**The fault.** A class code picks the FIRST device of the class, and on the T630 that is the HDMI audio
controller at 00:01.1, beside its GPU at 00:01.0; the analog one with the Realtek is 00:09.2. The
supervisor already supplied a BDF, but the kernel used it only for bus mastering and confinement - the
window, the arena and the vector still came from the first of the class.

**Kernel: one device per request.** `HwClass::pci_dev` decides the device a PCI spawn names, and every
grant goes through it: window, arena, vector, confinement, bus mastering. A supplied BDF selects it; none
means the first of the class, as before. A supplied BDF whose device is of a different class is refused
with no device granted, because a stale answer did exactly that on 2026-10-08 and confined the SATA
controller to `xhci`'s arena. That refusal is built and not yet seen firing.

**Which device: a fact from the reporter, a choice by the supervisor.** `hw-enumerator`'s class question
takes an optional byte, `PREFER_OWN` (`hwclass` in the SDK, one definition): the first device of the class
that is not a display's companion - a function other than 0 whose function 0 is a display controller -
or, if every one is, the first. The supervisor sets it. `hardware` asks the same question to say which
device a driver holds, so the report cannot show the driver on one controller while the kernel granted
another; QEMU caught exactly that the first time the suite ran with two.

**Pinned in QEMU.** `osdev test audio` now boots a decoy: an HD Audio controller with no codec as function
1 of a VGA at 00:06.0, so it is the first of its class, and the real one at 00:08.0. The driver is granted
00:08.0 (`BDF 0x0040 supplied for class 0x040300, the first of that class is 0x0031 - the supplied one is
granted`), finds QEMU's codec, plays into the WAV, and `audio hardware` shows the decoy `not driven`: 69
checks, all passing. The two controllers' registers are 16 KiB apart there, which is K1 in miniature.

## Step A6 on the T630, K1: the window is the BAR (2026-10-09)

**Worse than recorded.** "Found while preparing", 1, said the T630's audio window covers the HDMI audio
controller. Its BAR is at `0xfeb60000` and the window was 64 KiB, so it also reached `xhci`
(`0xfeb68000`), EHCI (`0xfeb6c000`) and the AHCI disk controller (`0xfeb6d000`) - and each of those
drivers' windows reached the ones above it.

**The fix.** x86 `pci::bar_len` measures a memory BAR when it is first granted - memory decode off, all
ones written, the mask read back, BAR and decode restored, the whole sequence under the configuration
lock so no other reader sees the mask - and caches it. The window is the BAR: whole pages mapped, the
`Mmio` length the BAR's exact size, so `Mmio`'s bounds check stops a driver at its device's last byte
even inside a shared page. At most 16 MiB per grant. Sizing happens at grant time and never at the boot
scan, where it would turn off a display controller's decode under the boot console.

**What is not fixed.** Only x86 measures. The other ports answer 0 and keep the fixed 64 KiB window,
and the spawn line says so; the Pi 4's and the VisionFive's `xhci` are PCIe devices this leaves as they
were.

**QEMU.** `audio-driver` 16384 bytes, `block-driver` (AHCI) 4096, `nic-driver` (e1000) 131072 - the
e1000's window GREW, because its BAR is 128 KiB and the fixed window had been half of it. The audio suite:
69 checks, all passing.

## Step A6 on the T630: the K1 and K2 card, and the second image (2026-10-09)

**The card held every line of the prediction.** `task: BDF 0x004a supplied for class 0x040300, the first
of that class is 0x0009 - the supplied one is granted`; the IOMMU confined 00:09.2, not 00:01.1; and every
window is its BAR now - audio 16384 bytes, `xhci` 8192, `ehci` 256, AHCI 1024, the RTL8168 4096. The driver
surveyed a Realtek **10ec:0255** at 00:09.2 and stopped, as it must on a codec not in `PLAYABLE`.
`selfcheck` ran 537 with 0 failed before and after a 100-round `chaos max-carnage all-services`, every
service panic in it the expected `EndpointDead`, and no driver faulted in its smaller window.

**Three facts the card added:**

- **The speaker path goes through a mixer**: converter 0x02 -> mixer 0x0c -> pin 0x14, and 0x0c's other
  input is 0x0b, the loopback mix of the microphones. A mixer has input amplifiers, and the driver set
  only output ones. Neither QEMU's codec nor the Wyse's has a mixer on its path, so this never arose.
- **The snoop bit is already on.** Configuration offset 0x42 reads `0x03`; the bit Linux sets for this
  controller (`ATI_SB450_HDAUDIO_ENABLE_SNOOP`, 0x02) is set. Linux writes the field as exactly 0x02, so
  it would also clear bit 0, whose meaning is not known here. Recorded, not acted on: the driver cannot
  write configuration space, and coherence is what the bit is for.
- **No interrupt at all.** The controller has no MSI capability and no INTx line (255), so the kernel says
  `accepted neither MSI nor MSI-X - no interrupt, must poll` and the driver refills every 10 ms, as it was
  built to.

**The second image, one concern: set up this codec's path as Linux does.** `PLAYABLE` gains
`10ec:0255` with the one coefficient Linux sets at probe (`alc_fill_eapd_coef`: 0x10 bit 9 clear), and
`configure_path` sets a mixer's input amplifiers: the input from the next node on the path unmuted at
0 dB (the amplifier's offset), the others muted, the first read back. The gate without the mixer would
very likely be silent, and the mixer cannot be seen without the gate. QEMU's audio suite: 69, all passing.

**Prediction:** the coefficient line `0x10: ... read back the same`, then `audio-driver: ready - serving
requests; NO interrupt was routed, so playback refills by polling`, and `node 0x02 has no mute - muting on
node 0x14`; `audio tone 440` HEARD from the T630's speaker; `audio debug codec` shows pin 0x14 `<- playing`.
Falsified by: a mixer line (`mixer 0x0c input 0 ... reads`), which says the amplifier would not unmute; or
`ready` with the link position moving and silence, which points past the mixer at the pin or the speaker
amplifier; or underruns, which would be the 10 ms polling, not the codec.

## Step A6 on the T630: silent, and the third image (2026-10-09)

**The second image was silent, on the speaker pin and on headphones.** Everything it predicted in the
log held - the coefficient read back, `ready ... refills by polling`, `node 0x02 has no mute - muting on
node 0x14`, no mixer line - and four 5-second tones each `played ... in 5003 ms by the clock, 0
underrun(s)`. Nothing was heard. Three things this does and does not say:

- **The 0 underruns prove less here than on the Wyse.** This controller has no interrupt, so the count
  is the driver's clock against its own refills; it does not show the controller fetched a sample. The
  link position during a tone (`audio debug stream`) is what shows that, and it was not read.
- **The headphones could not have worked.** The driver plays through the first path the survey finds,
  the speaker pin 0x14; the headphone jack is pin 0x21 and is reached by `audio output headphone`.
- **Whether the T630 has an internal speaker is not known here.** The pin's default configuration says
  fixed speaker (`0x90170110`), which is what the firmware claims, not proof one is fitted.

**Linux has no speaker quirk for this board.** Its subsystem id is `103c:8158`, and Linux's one entry for
it (`ALC256_FIXUP_HP_HEADSET_MIC`) only restarts headset-jack detection. What Linux DOES do for every
ALC255 is `alc256_init`, the headphone amplifier's power-up, and `PIN_HP` (0xc0) on a pin that can drive
headphones. Neither was done here.

**The third image, one concern: the ALC255's headphone output as Linux brings it up.** `PLAYABLE` rows can
carry a codec's own bring-up, and the ALC255's is `alc256_init`: node 0x57 coefficient 0x04 to low power,
the headphone pin muted and enabled as an output, coefficient 0x46 bits 13:12 cleared, 0x57/0x04 to high
power, node 0x53 coefficient 0x02 bit 15 pulsed, coefficient 0x36 written as 0x5757 - each read back and
logged. Coefficients can now be on any vendor node, not only 0x20. A headphone pin that can drive
headphones is enabled with its headphone amplifier (0xc0 rather than 0x40), on any codec.

**Prediction:** each coefficient line `read back the same`, then `ready`. After `audio output headphone`,
`audio debug codec` shows pin 0x21 `control 0xc0 <- playing`, and `audio tone 440` is HEARD in the
headphones. During a tone, `audio debug stream` shows the link position moving.

**What would falsify it, and what each says:** a coefficient `DIFFERENT` or `did not answer` - the hidden
nodes are not where Linux has them on this part, and bring-up stops there. Silence with the position NOT
moving - the stream is not running, so the codec was never the question: the controller (and its missing
interrupt, or the IOMMU) is. Silence with the position moving - the samples reach the codec and are lost
inside it, which leaves the converter's stream tag and format, read in `audio debug codec`.

**The card (2026-10-09, same day): HEARD on the T630, through headphones.** Every coefficient of
`alc256_init` read back the same - 0x57/0x04 `0xa09c -> 0xa099` and back, 0x53/0x02 pulsed, 0x36 `0x0004 ->
0x5757` - then `ready ... refills by polling`. After `audio output headphone` (`output now pin 0x21
(headphone)`), `audio debug codec` showed pin 0x21 `control 0xc0 <- playing`, and the operator heard the
tone in the headphones: the HDA driver now plays on both x86 machines, the T630 on a controller with no
interrupt at all. A 10 s tone through the speaker pin played its full length with 0 underruns; whether the
T630 has a speaker fitted for it to reach is still not known.

**What this does not cover, recorded:** the Wyse's headphone jack now gets the headphone amplifier too
(0xc0 on any pin that can drive headphones) and has not been listened to since; the Wyse's ALC225 has its
own Linux headphone sequence (`alc225_init`), not done; and the T630's speaker remains unexplained.

**And the T630's speaker (same day): HEARD, once the headphones were unplugged.** The silent speaker test
above ran with the headphones still in the jack; unplugged, the same image played the tone from the
T630's own speaker. So this board cuts its speaker in HARDWARE while the jack is occupied - the driver
does nothing to it. HP's spec sheet was not found; reseller spec tables list a built-in speaker.

**It is quiet, at volume 100, and the driver has nothing left to give.** Volume 100 is the converter's
top step, 87, which its capabilities (`0x00025757`) make 0 dB; mixer 0x0c's input is at its own 0 dB;
the speaker pin's amplifier is a mute and no steps; the widget graph has no other gain. The test tone is
half of full scale, 6 dB of deliberate headroom, so a full-scale WAV through `audio play` would be about
twice as loud and no more. What is left is the speaker itself - an inference, not a measurement.

**`audio` said "nothing is plugged into it" about that speaker.** Its pin can sense presence and read
nothing there while fitted and playing. A FIXED pin (connectivity `0b10`) is not a jack and its sense
means nothing, which is why Linux does not consult it; the driver now reports presence unknown for one.

## Step A6: both x86 machines, headphones and speaker (2026-10-10)

**The T630's speaker, on an image with nothing changed for volume:** 440, 880 and 1760 Hz for 5 s each,
0 underruns, every `alc256_init` coefficient read back the same. The operator found it loud enough this
time; nothing in the path changed, so the difference is in the listening, not the driver.

**The Wyse's headphones: HEARD.** `audio output headphone` -> `output now pin 0x21 (headphone) - verified`,
then a 5 s tone, 0 underruns and 59 interrupts, heard in the headphones. Its speaker was heard as well,
and loud. So the headphone amplifier (`0xc0` on a pin that can drive headphones) works on the ALC225
without Linux's `alc225_init`; that sequence stays not done, and is not needed for sound.

**The fixed-pin presence fix, on hardware:** the Wyse's `audio outputs` printed `speaker  cannot tell`
for the built-in speaker, where it used to claim nothing was plugged in.

**What this does not cover:** the Pi 2's jack has still not been heard.
