# Audio

**Status: steps A1-A3 and the first half of A4 built and run in QEMU (2026-10-03), on branch
`feat/audio`. The driver resets an Intel High Definition Audio controller, finds its codec and output
path, moves codec commands onto the CORB and RIRB, and serves a tagged request protocol; the shell's
`audio` sets the volume, mutes, powers the codec down and up and plays tones (`utilities/57_audio.md`),
each checked against the WAV QEMU wrote; the volume and the mute survive a reboot in `/audio.settings`.
Interrupt-driven, IOMMU-confined, restartable. Not yet: `outputs`, `play`, `debug`, system sounds, the
shortcuts; not run on hardware (A6).**

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
| Pi 2 / Pi 4 | 3.5 mm jack driven by PWM, fed by the BCM DMA engine (section "The Pis and the VisionFive") | not HDA. Needs the GPIO pinmux and the clock manager, both SHARED SoC blocks; no QEMU model. A kernel proposal first |
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
| **A4** | A request protocol (tagged, defined once and shared with the shell), the `audio` utility as specified below, and `/audio.settings` | QEMU - **protocol, the first verbs (status, info, volume, mute, unmute, on, off, off hard, tone), `/audio.settings` and `osdev test audio` built**; outputs, debug and system sounds to come |
| **A5** | `audio play <path>`: the shell reads the WAV and streams chunks; the driver answers each with the free space left; underruns write silence and are counted | QEMU - **built** |
| A6 | The T630: the kernel fixes below, the AMD snoop bit, the ALC255's real path walk with EAPD, a person listening | T630 |
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
`output`, `play`, `debug`, `system sounds`, `/audio.settings` and the shortcuts. What follows was written
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
is a separate fact (only they are confined). `take_task_hw_bdf` reads the record and resets it in one
step, and a failed spawn (`cleanup_partial_spawn` in `kernel/src/task/mod.rs`) takes it too, since a
spawn can fail after recording its device. `audio-driver` joined both death lists in the kernel, the
supervisor's `MANAGED` roster and its death loop (`services/supervisor/src/main.rs`).

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

## The Pis and the VisionFive (researched 2026-10-03, nothing built)

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
