<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `wifi` - join and inspect a wireless network

Version reported by `wifi version`. Implementation shape: **shell built-in, every verb** (`cmd_wifi` in
`services/shell/src/main.rs`), each a question put to whichever radio service is running (`RADIOS`: `wifi-driver`, then `wifi-usb`) - see
section 7,
where the reason for `scan` and `join` is a constraint rather than a preference.

## Status, as built and honest (2026-09-29)

*(Since then: the VisionFive 2 Lite's AIC8800 (`docs/wifi-aic8800.md`, V0-V6) and the Pi 2's USB dongle
(`docs/wifi-usb.md`, R4-R12c) answer the same verbs through the same serve loop. What follows is the Pi 4
as of this date.)*

**The radio scans and lists on the Raspberry Pi 4, and has since 2026-09-28.** The shell asks the
`wifi-driver` over IPC, the radio sweeps, and one record per network prints in the order this file
specifies - `ssid signal band security`, with `security` read from the beacon (an RSN element is WPA2, a
Microsoft-OUI vendor element is WPA, the Privacy bit alone is WEP). Eleven networks on the first run, and
a second run straight after scanned again on the same driver session. About three seconds each, ending
when the firmware says the scan is complete rather than when a timer runs out.

**Hardware-verified 2026-09-29 13:05, Raspberry Pi 4, one boot, first try.** Everything sections 2 to 4 and
6 describe ran at the prompt and did what the sections say, except the WPA2 join itself, which this
firmware cannot complete without the host handshake (below). The key derivation matched all four published
vectors at boot (`stage 0`). The cache lives in the driver's memory - not in a file, so it needs no `fs`,
dies with the driver, and never writes a network name to the card. (That was 2026-09-29. Keys, and the
names they belong to, have been on disk in `/wifi.keys` since 2026-09-30 - section 6.)

What every verb does, and what was seen:

- `wifi scan` - **verified.** Live numbered rows under `scanning  [q] quit  [b] background` and the header;
  4 networks in 3 s; the picker took a number, echoed the row, and joined through the same path as
  `connect`. `q` mid-sweep and `b` were not exercised on this boot.
- `wifi list` - **verified**, with `saved` in NOTE for the network whose key was held.
- `wifi status`, `wifi info` - **verified** in the not-associated state (`network  none`, the scan facts,
  the `addressing` line). The associated state cannot be reached until the handshake exists.
- `wifi debug`, `wifi debug trace`, `wifi debug firmware` - **verified.** The trace held the boot scan, the
  prompt's scan, the join's six control exchanges and the handshake frames, on a clock that read real
  milliseconds. `events`, `stats`, `transport` alone were not typed; their rows appeared under bare `debug`.
- `wifi join <ssid>` - **verified to the point this firmware allows**: asked the passphrase once, derived
  the key into slot 0, associated, read six copies of the access point's handshake message 1 about a second
  apart, and reported the deauthentication (reason 15) as the driver's inability, not the passphrase's - the state
  before the handshake was built; the not-met list below has the build and what its first boot must show. A
  second `connect` of the same name **asked nothing** and joined with the held key. `wifi stored` named it.
- `wifi leave`, `wifi radio on|off`, `wifi forget` - built as sections 2 and 6 say; `radio off` and `on`
  typed 2026-09-30 15:05 (off dropped the link, on brought the radio back and a join after it worked).
- `wifi help`, `wifi <verb> help`, `wifi version`, tab completion, and a row in `help`.
- Absence is told apart from a wedge (section 5): no live `wifi-driver` means no radio; a live one that will
  not answer says that after a bounded wait, never a guess.

- **The frame path, both ways - hardware-verified 2026-09-30, 08:50.** Cable out, `ping` over the radio
  (36 of 38, the two lost being the switch itself, then 0 lost); cable in, the address change, a new
  lease, `ping` over the cable; out again, over the radio with no new `wifi join`; twice round. `net`
  said `link  up via wifi (the cable is out)` and `link  up via the cable`. The operator: "Works
  beautifully". The first boot that morning had proved the same path through DHCP and then gone deaf to a
  leaked reply cap (`docs/wifi.md` 41).

**Things this file specifies that are NOT met yet, said here rather than discovered:**

- **Both rekeys are answered (built 2026-09-30, `docs/wifi.md` 42) and neither has yet been seen on
  hardware** - they happen when the access point decides, commonly hourly, and cannot be provoked. The
  group-key rekey re-installs the group key and acknowledges; a pairwise rekey (the access point restarting
  the four-way handshake) is run to completion by the same handshake the join uses and replaces both keys.
  If either fails the log names the step and `wifi join` recovers the link (`backlog/64`).

- **The WPA2 handshake joins - hardware-verified 2026-09-29, 20:01** (`joined` first try, `already
  joined` twice, the keys installed; the previous evening's boot had verified it through message 4 and
  been refused at the key install by two bytes of struct padding, `docs/wifi.md` 40). Kept in this list
  only for what remains of it: the driver answers
  message 1 with message 2 (its nonce, the RSN element, MIC'd with the confirmation key derived by
  PRF-384 from the PMK), verifies message 3 (ANonce, MIC, AES-key-unwraps the group key), sends message 4,
  and installs the pairwise and group keys through `wsec_key` - every step from OpenBSD's net80211 and
  bwfm, named at the step in `join.rs`. Two primitives were added with published vectors (AES-128, FIPS 197
  C.1; key unwrap, RFC 3394 4.1) and self-test at boot with the others. A wrong passphrase is decided by
  the one pattern that means it: message 1 repeated after two answers, or a deauthentication after an
  answer. What the first boot must show: `message 2 of 4 sent`, `message 3 verified`, `JOINED`, and then
  `joined <name>` at the prompt; with a wrong passphrase, `not joined - incorrect passphrase`. **One
  weakness, recorded not hidden:** the aarch64 kernel exposed no hardware RNG at first, so the station's
  nonce was hashed from the cycle counter, the access point's nonce and our address, and the driver's log
  said so every time. *Closed 2026-09-30: the kernel reads the Pi 4's RNG200 (`docs/wifi.md` 40); the
  fallback and its line stay for a block that answers nothing.*
- **Glommed superframes are read, by the descriptor's chunk lengths - hardware-verified 2026-09-29 15:58.**
  The boot at 13:05 showed the events riding inside channel-3 superframes; the boot at 13:57, with a walker
  that assumed the sub-frames were back to back, showed `ASSOC` for the first time (sub-frame 0) and then
  sub-frame 1 failing to validate at +229, +92 and +652 - the sub-frames are PADDED. The descriptor frame
  before every superframe lists the chunk each sub-frame occupies, padding included, and `ctrl::subframes`
  walks by it. The boot at 15:58 showed `join event 16 (LINK) flags 0x0001` and `ASSOCIATED` for the first
  time in this port's life, `rx_glom_sub 40` against 24 superframes, and no `does not validate` line; the
  trace shows `RX GLOM len=384` carrying `ASSOC` and `LINK`, then `RX GLOM len=256` carrying `JOIN` and
  `SET_SSID`. This item is closed and kept here because of what it cost (`docs/wifi.md` §39).
- ~~The passphrase prompt cannot be abandoned.~~ *Closed 2026-09-30: Esc or `^Q` ends the entry, zeroes
  what was typed, and prints `passphrase entry abandoned - nothing was sent`.*
- The BSSID and band in the `joined` sentence (section 4b) are not yet read from the association event;
  the sentence names the network only. `wifi info` reads both live, so they are one command away.
- An access point that drops the station is noticed by the frame pull - the next time the stack asks for
  frames, which is every hundred milliseconds while there is a link - and by `wifi status` or `wifi
  info` reading the link. It is NOT noticed while the radio is standing by behind a cable that is in,
  because nobody is pulling then; the driver still does not watch `LINK` events on its own.

One limitation of the record format, recorded rather than left for a pipe to find: SSIDs may contain
spaces, and `ssid` is the first field, so a positional filter on the second field will misread such a row.
This file puts `ssid` first; the fix, if wanted, is a design decision about field order and not a bug in
the scan.

**Why this file moved here, which is a small lesson about the gates.** It was written as
`utilities/56_wifi.md` and Commandment X refused it: a spec under `utilities/` asserts the shell
answers that verb, and it did not. So it lived in `docs/` as a design note. The moment the verb was
implemented **the same check fired in the opposite direction** - the shell answers `wifi` and no spec
in `utilities/` describes it, which is complexity discoverable only by reading source (§26.11). The
pair makes this directory mean exactly one thing, and the document's home is decided by whether the
verb answers rather than by anyone's preference.

---

## 1. What it is for

One question, asked three ways: what networks are there, which am I on, and join that one. Wireless
differs from `net` in exactly one respect that matters to a user - a wireless link must be *joined*
before it carries a frame, and joining needs a secret. Everything after association is `net`'s job and
this utility says nothing about it.

`wifi` is therefore the *link* verb. `net` remains the *network* verb: addresses, DNS, leases, ARP.
There is deliberately no overlap - `wifi status` reports radio and association, never an IP address,
because an IP address has one owner and duplicating it here would make two answers possible.

## 2. Subcommands

| Subcommand | What it does |
|---|---|
| `wifi` | with no args: usage, as every utility (`0_conventions.md` rule 1). It used to alias `status`, a second way to say one thing |
| `wifi scan` | ask the radio to sweep. Rows appear as they are heard; when the sweep ends the rows become a numbered picker. `q` stops the sweep, `b` leaves it running and returns the prompt |
| `wifi list` | print the last complete scan: SSID, signal, band, security. Instant, records only, never scans |
| `wifi join <ssid>` | join a network by name. Asks the passphrase once if one is needed and none is held; `already joined` if you are on it |
| `wifi leave` (says `left <name>`, or `nothing to leave - not joined`) | leave the current network. The radio stays up |
| `wifi status` | the human answer: radio, network and band, signal, security, time joined, last scan. Read live. Section 4f |
| `wifi info` | the link in detail: bssid, band, channel, signal, security, time joined, scan facts - and where addressing lives. Section 4f |
| `wifi debug [events\|stats\|firmware\|transport\|trace]` | the driver's own account of itself: counters, the firmware's words, the last 64 frames. Section 4g |
| `wifi forget <ssid>` | drop the held key for that network. Does not leave the network |
| `wifi stored` | the networks a key is held for, one per line. Names, never secrets. Sixty-four at most - section 6 |
| `wifi radio on` / `wifi radio off` | the FIRMWARE's radio switch: the chip stays powered. `off` disconnects first and says so. `on` converges from whichever state the radio is in - soft off, it is the two-second switch below; after `off hard`, it is the cold start described in that row; and with the radio DOWN (its firmware trapped at start, or never ran) it is the hard on: the driver is restarted (the respawn adopts a live firmware or power-cycles a dead one) and watched to `radio on succeeded - joined <name>`, and a chip that comes up warm is reported, not retried: ONE attempt, then the prompt and ``radio on failed - the chip came up warm (...); `wifi radio powercycle` tries once more``. `on` never answers "no control over the radio's power" unless the kernel refused. `on` then REJOINS the network last joined this boot, with the key it holds and without asking, and says `joined <name>` (asked for by the operator 2026-09-30); a `wifi leave` before the `off` cancels that, and a `wifi forget` of the name leaves nothing to rejoin with. Asking for the state it is already in says `radio already on` / `radio already off`, and sends the radio nothing. Both BLOCK and offer no key - no `[q]` since 2026-10-01, because the request is a kernel `Call` and a shell blocked in one cannot read the console - for about two seconds, and up to fifteen when `on` rejoins. `off` is VERIFIED: the driver asks the firmware back (`WLC_GET_UP`) and the shell prints `... - verified: ...` when it reads down, says unverified when it could not ask, and `... FAILED ...` when the firmware still reads up (`docs/wifi.md` 49; not yet run on hardware) |
| `wifi radio off hard` | the CHIP's power, cut and left cut. BLOCKS for about three seconds with NO key, until the driver has left the network, cut the power and the kernel has read the pin back low; the driver then checks the cut on its own bus (a CMD52 50 ms later that nothing should answer) and the shell prints `... - verified: ...`, says unverified when the check could not be made, or `... FAILED ...` when the chip still answered (`docs/wifi.md` 49; not yet run on hardware). There is no `q`, because this cannot be stopped once asked, and no `b` either: it offered `[b] background` until 2026-10-01, but the request is a kernel `Call` and a shell blocked in one cannot read the console, so the key could never have been seen in time. The driver stays alive to answer: `wifi status` says `radio off (hard - the chip is powered down; wifi radio on powers it up)`, every other request answers that the chip is powered down. `wifi radio on` from this state restores the power, prints `radio powered up - starting the driver on the cold chip`, restarts the driver, and watches the cold path like `powercycle` does, ending `radio on succeeded - joined <name>`. About twenty seconds; the soft `off`/`on` stay the two-second switch. `on` after `off hard` can still come up WARM - it did at 09:44 and 14:43 on 2026-10-01 - and then, after its one attempt, returns to the prompt saying `wifi radio powercycle` tries once more; `off hard` cuts and verifies the power, it does not produce a cold chip on demand (`docs/wifi.md` 47, 48). **On a USB dongle (`wifi-usb`, `docs/wifi-usb.md` 18) the power is its port's and is not touched.** `off hard` there is the chip's own power-down, Linux's `rtl8192cu_power_off`: the network left, the firmware asked to stop and stopped from the host if it does not answer, the chip suspended. Its check is that no firmware is marked running. `on` and `powercycle` restart the driver, whose bring-up powers the chip on and uploads its firmware. A dongle that no longer answers on USB cannot be powered down this way; the driver says so, and only unplugging it helps |
| `wifi radio powercycle` | the CHIP's power, not the firmware's radio switch: the driver cuts and restores it through the kernel's `DevicePower` (its own grant, renewable - CLAUDE.md 12.3), and the shell then restarts the driver, which comes up from power-on and rejoins from `/wifi.keys`. The power request itself is bounded at fifteen seconds before the watch begins. Then it says `radio powered down for 2.0 s - restarting the driver on the cold chip` (with ` (the network is left)` before the dash when it was joined), and BLOCKS with `[q] quit  [b] background`, printing each change of state as the driver answers its status question - `waiting for the driver`, `radio coming up`, `radio up, joining` - and ends in one of two lines: `powercycle succeeded - joined <name>`, or `powercycle failed - <why>` (the driver did not answer, this machine has no control over the radio's power, the driver could not be restarted, or the radio did not rejoin within 90 s and where it got to). VERIFIED: success is a join younger than the watch itself, so a stale answer from before the restart cannot pass for one; and a driver that comes back with its radio down means the chip came up warm - its firmware trapped at start - and the shell reports it and stops: ONE cycle per invocation (attempts on the same chip were not independent, so a retry only hid the cause; `docs/wifi.md` 52), re-runnable by hand. The driver waits 300 ms after power-on before its first command (`POWER_ON_SETTLE_MS`; five seconds was tried in section 52, changed nothing, and was reverted). BOUNDED at the operator's word: a warm chip ends `powercycle failed - the chip came up warm (its firmware trapped at start; ...)`, returns the prompt, and can be run again - nothing needs a reboot. The driver parks its SDIO host for the whole off window. The "warm starts" this paragraph once expected were a slow host - the Arm cores at their minimum clock - and with the `power` service's lease every power cycle has come up cold (`docs/wifi.md` 55-57). `q` leaves the watch. `q` and `b` both return to the prompt and say the power cycle continues in the driver: once the power is cut there is nothing to stop (`docs/wifi.md` 47) |
| `wifi hardware` | SPECIFIED, NOT BUILT (section 11): the radios this machine has, one row each - its name, chip, bus, state, network and whether it is the one in use. A report: it pipes |
| `wifi hardware use <radio>` | SPECIFIED, NOT BUILT (section 11): choose the radio every other `wifi` verb talks to and that carries the frames when the cable is out - `onboard`, `usb`, or `usb-1a0d` when there is more than one dongle. An action: it does not pipe |
| `wifi help` | usage, with one real example per row |
| `wifi version` | version number plus the collective copyright line |

**`scan` and `list` are two verbs, not two spellings.** Rule 3 forbids synonyms, and these are not one:
`scan` is an ACTION whose value is its effect - the radio sweeps and the driver's cache is replaced -
and `list` is a REPORT whose value is its output. The split exists for two mechanical reasons, not
taste. First, a verb that both waits and produces puts its status line into the pipe: with one verb,
`wifi list | count` was off by one because `scanning  [q] quit` went down the pipe with the records.
Second, a picker's numbers are only safe when the list under them cannot change, and a list that
changes only when somebody explicitly runs `scan` is exactly that.

**`scan` is a candidate for the background for the same reason `copy` is** (`55_background.md` §3):
the test is "is this command's value its EFFECT or its OUTPUT", and a scan's value is entirely the
filled cache. Its output is a different command, run later. The driver is already the job service -
it holds no console capability, does the work, and holds the result - so backgrounding a scan costs
no new service and no `jobs` row. The state lives in `wifi status` instead.

**`join` and `leave`, not `connect` and `disconnect` - renamed 2026-09-29 at the operator's call.** `join`
is what the 802.11 layer calls it (a station joins a BSS) and what the driver's log has said since the first
association; `connect` is the socket word and stays with `tcp`. `leave` is its natural opposite. Rule 3
forbids aliases, so `wifi connect` answers ``try `wifi join <ssid>` `` and `wifi disconnect` answers
``try `wifi leave` ``, the way `ls` answers `try dir`.

**The outcome is one confident word, or `not joined - <why>`.** `joined Maple-House`; `already joined
Maple-House` (checked live against the firmware, not remembered - nothing is sent); `not joined - incorrect
passphrase`; `not joined - no network named X in range`; `not joined - aborted`. "Incorrect passphrase" is
not a guess: it is the one situation in which the access point receives our message 2 and, its MIC failing
to verify, repeats message 1 and gives up - nothing else produces that pattern. Before the handshake was
built (2026-09-29) the WPA2 case ended in `not joined - X began the WPA2 handshake, which this driver did
not answer`; that reply stays in the table so a shell can still name it, and no current driver produces it.

`wifi join` takes **no** passphrase argument, in any position, and that is a security decision
rather than an ergonomic one: an argument would be recorded in the recall ring and written to
`/.gsh_history`, where an up-arrow recovers it. The passphrase is only ever read through the shell's
invisible-entry path (`input secret`, `docs/scripting.md` §8), which is already excluded from both.

## 3. Output is a pipeable structure (rule 12)

`wifi list` is a producer. In a pipe it is RECORDS - one per network, with the columns `network`, `band`,
`signal` (the word), `dbm` (the reading, a signed number), `security` and `note` - so the record stages
work on it with no special case:

```
wifi list | where security=WPA2
wifi list | sort reverse dbm          strongest first
wifi list | where dbm>-60
wifi list | select network dbm
wifi list | max dbm                   the strongest signal heard
wifi list | count
wifi list | to json
wifi list | write /networks.txt
```

On the screen, bare `wifi list` prints the fixed-width lines below; the records are the same facts, decoded
once for both. `match`, `first` and `last` are text stages and refuse a record stream with a pointer to
`where` - the same rule as `dir` and `observe now`. (It was text in a pipe for one morning, 2026-10-02,
and `wifi list | match WPA2` worked then; `where security=WPA2` is its record form. `docs/wifi.md` 58.)

**The reports pipe; the actions do not.** `list`, `stored`, `status`, `info`, `debug` and `version` can start
a pipe. `scan`, `join`, `leave`, `forget` and `radio ...` are actions, and refuse with a sentence that names
the reports: their output is a conversation, not data - `join` reads a passphrase from the console, and in
a pipe its prompt would disappear while it waited. A report that fails (no scan yet, a scan running, the
radio down) says why on the CONSOLE and stops the pipe, and an empty scan gives a pipe no rows at all, so
`wifi list | count` says 0 rather than counting the sentence. (Until 2026-10-02 this section described
pipes the shell refused: `wifi` was not on its list of producers - `docs/wifi.md` 58.)

Fields, in order: network, band, signal, security, note. Signal is the **dBm, a raw fact** (rule 7), with a
word beside it that is a STATED RULE over the number, so a reader can check it: -50 dBm or stronger is
`excellent`, to -60 `good`, to -70 `fair`, weaker is `weak`. The number is always printed; a reader given
only the word could not recover it.

**The columns are fixed-width, and they can be because the widest is known.** An SSID is at most 32
bytes - that is the size of the field in the beacon, so a longer one cannot exist - and every other
field has a bounded vocabulary. So the layout is NETWORK padded to 32, BAND to 6 (`2.4GHz` or `5GHz`),
SIGNAL as a word padded to 9 then the dBm right-aligned to 4 (the header carries the unit once),
SECURITY to 8, then NOTE - `joined` for the network the radio is on, `saved` for one whose key the
driver holds, else blank - two spaces between columns, 78 columns with the number:

```
gsh> wifi list
Maple-House                       5GHz    excellent  -41  WPA2      joined
Maple-House                       2.4GHz  excellent  -47  WPA2
(hidden)                          5GHz    fair       -63  WPA2
BT-Hub6-K7QR                      2.4GHz  weak       -71  WPA2/WPA  saved
Riverside Tenant WiFi Guest Netw  2.4GHz  weak       -74  open
xfinitywifi                       2.4GHz  weak       -79  open
SKY7F2B1                          5GHz    weak       -80  WPA2
PrinterDirect-4A                  2.4GHz  weak       -82  WEP
(unprintable)                     2.4GHz  weak       -85  WPA2
a                                 5GHz    weak       -86  WPA
Free_Cafe_WiFi                    2.4GHz  weak       -88  open
```

Nothing is ever truncated: the fifth row is exactly 32 bytes and fills its column edge to edge. A
network that does not announce its name prints `(hidden)`; a byte outside printable ASCII prints as a
dot rather than reaching the terminal as a control code. `security` is one of `open`, `WEP`, `WPA`, `WPA2`,
`WPA2/WPA`, read from the beacon. Two rows with the same name are two access points - the same
network on 2.4 GHz and 5 GHz is the usual case, and both are listed because both were heard.

Rows are in the order the radio heard them, and `list` prints the cache in that order every time. A
sorted view is one pipe away (`wifi list | sort reverse dbm`), and keeping the producer unsorted is what lets the
picker's numbers in section 4 stay put.

**`wifi list` never scans, and says so when there is nothing to print:**

| State of the cache | What `wifi list` prints |
|---|---|
| a completed scan | its records, as above |
| no scan since boot | `no scan yet - run wifi scan` (an error, not an empty list) |
| a scan is running | `scanning - 9 heard so far; wifi list when it finishes` (an error, not the OLD cache) |
| the radio is down | one of the four radio-down lines, each naming why (section 4f and section 5) |

The two error rows are Commandment III: the cache is a derived view of the air at a past moment, and a
derived view is never served as current when it is not. An empty list would say "no networks" about a
radio nobody asked; the old cache during a sweep would hand a pipe stale rows with nothing marking them
stale. An error goes to nobody's pipe.

`wifi stored` is a producer of one field, `ssid`. It exists so that "which networks do I have a
password for" is answerable without a way to ask "what is the password", which is not a supported
question at any privilege level.

## 4. `wifi scan`: a live surface that ends in a picker (rules 10, 10a, 11)

A scan takes real time on real hardware - the radio sweeps channels, about three seconds. `wifi scan`
is the surface for that wait, and it earns its keep by being the one place a network can be chosen by
number.

### 4a. What it looks like

Rows appear as the radio hears them, numbered from 1, appended and **never re-sorted while the surface
is live** - a number that moves under a finger is the one bug a picker must not have. When the firmware
reports the sweep complete, the status line becomes the prompt:

```
gsh> wifi scan
scanning  [q] quit  [b] background
    NETWORK                           BAND    SIGNAL (dBm)    SECURITY  NOTE
 1  Maple-House                       5GHz    excellent  -41  WPA2      joined
 2  Maple-House                       2.4GHz  excellent  -47  WPA2
 3  (hidden)                          5GHz    fair       -63  WPA2
 4  BT-Hub6-K7QR                      2.4GHz  weak       -71  WPA2/WPA  saved
 5  Riverside Tenant WiFi Guest Netw  2.4GHz  weak       -74  open
 6  xfinitywifi                       2.4GHz  weak       -79  open
 7  SKY7F2B1                          5GHz    weak       -80  WPA2
 8  PrinterDirect-4A                  2.4GHz  weak       -82  WEP
 9  (unprintable)                     2.4GHz  weak       -85  WPA2
10  a                                 5GHz    weak       -86  WPA
11  Free_Cafe_WiFi                    2.4GHz  weak       -88  open
11 networks in 3 s
join: type a number and Enter, [q] quit:
```

The header row and the numbers exist only here. `wifi list` prints the same columns without either,
so the eye lines up between the two and a pipe never sees a number it did not ask for.

Digits are not text in this surface, so `q` and Esc are free to mean leave (rule 10a). Numbers go to
two digits because the driver holds up to 32 results, hence Enter rather than a single keypress. A
number outside the list says so and asks again.

### 4b. Picking one

A pick names a NETWORK, and the radio chooses the access point. Two rows with the same SSID (rows 1
and 2 above) are one network on two bands; picking either joins that network, and the firmware
associates with whichever access point it prefers - usually, not always, the stronger. So the pick
echoes the row, and the join reports what was actually joined:

```
join: 1
 1  Maple-House                       5GHz    excellent  -41  WPA2      joined
passphrase (not shown):
joining Maple-House  [q] quit
joined Maple-House on 5GHz, bssid 02:1a:7e:c4:09:51 - the link is up and the handshake completed
  (for the address, type net)
```

The BSSID and band in the last line come from the association event, not from the row - the row said
which network was asked for, the event says which access point answered, and where they differ the
event is the truth. `wifi status` repeats it.

An `open` network skips the passphrase prompt. A `WEP` network is refused at the pick with `WEP is
not supported` (section 9) rather than asked for a key it will not use. A `(hidden)` row **cannot be
joined from the picker**: the join request carries the SSID and no BSSID (`sdk/wifi/src/wire.rs`), a
hidden row's SSID is empty, and the shell refuses an empty one with `wifi: the name or passphrase is out
of range - nothing was sent`. `wifi join` cannot reach it either, since it needs the name.

The passphrase prompt is a typing surface, so `q` is a letter there. Esc or `^Q` abandons it (rule
10a), and `wifi status` then reports not associated. A prompt abandoned sends nothing.

### 4c. The two keys while scanning

**`q` stops the SWEEP, not just the shell's interest in it** (rule 11). The driver is told to abort
the scan and the radio stops. The rows already on screen were a partial hearing of the room and are
**not kept**: the cache holds the last complete scan, and a half-scan replacing it would be a view that
lies about how much of the room it saw. It says so:

```
scan stopped - 4 heard, not kept; the last complete scan (11 networks, 180 s ago) stands
```

**`b` returns the prompt and leaves the radio sweeping.** The same key `foreground` uses for the same
meaning (`55_background.md` §2). Nothing is printed to the console after that - a detached scan has
no console, exactly as a detached copy has none - and the results land in the cache when the sweep
ends. The picker never appears; `wifi list` and `wifi join <ssid>` are the way in:

```
gsh> wifi scan
scanning  [q] quit  [b] background
    NETWORK                           BAND    SIGNAL (dBm)    SECURITY  NOTE
 1  Maple-House                       5GHz    excellent  -41  WPA2      joined
 2  Maple-House                       2.4GHz  excellent  -47  WPA2
scan continues in the driver - wifi list when it finishes, wifi status meanwhile
gsh> wifi status
radio up, not associated
scan running - 6 heard so far
gsh> wifi list
scanning - 9 heard so far; wifi list when it finishes
gsh> wifi list
Maple-House                       5GHz    excellent  -41  WPA2      joined
...
```

A `wifi scan` while one is running attaches to it - it shows the rows heard so far and continues
live - rather than starting a second, because the radio has one sweep in it at a time.

### 4d. The cache and its age

The driver owns the cache: it already holds the scan result, and it is the one place that survives
between shell commands. It is bounded at 32 networks, replaced only by a completed `wifi scan`, and
lost with the driver - a respawned driver starts with no cache and `wifi list` says `no scan yet`.

Its age is always available. `wifi scan` ends with `11 networks in 2.8 s`; `wifi status` carries
`last scan: 42 s ago, 11 networks`. `wifi list` itself stays records-only so pipes stay clean - the
age is one command away, never mixed into the data.

### 4f. Three views of one link: `status`, `info`, `debug`

The operator drew the line: `status` is the human operational answer, `info` the detailed connection and
device facts, `debug` the driver's own account for whoever is fixing it. All three are labelled lines, so
they pipe (`wifi status | match signal`), and every link fact is READ from the firmware when asked -
`BSSID`, `RSSI`, `chanspec` - not remembered from the last join.

```
gsh> wifi status
radio      on
network    Maple-House  5GHz
signal     excellent  -41 dBm
security   WPA2
joined     180 s ago
last scan  42 s ago, 11 networks
```

The word before the dBm is the same stated rule as the scan rows (section 3): -50 dBm or stronger
`excellent`, to -60 `good`, to -70 `fair`, weaker `weak`. Not associated: `network    none (not
associated)` and no signal, security or joined lines. Radio off by the soft
switch: `radio      off (soft - the firmware's switch; the chip stays powered; wifi radio on turns it back on)` and the same. The last
line is one of `scan       running - N heard so far`, `last scan  N s ago, M networks`, or `last scan
none - run wifi scan`.

Two radio states carry their own line instead of the table above. After `wifi radio off hard`, `wifi status`
says `radio off (hard - the chip is powered down; wifi radio on powers it up)`. With the radio DOWN - the
driver is alive but its radio is not up, at boot or after a respawn - the driver says why (byte 1 of its
`RADIO_DOWN` reply, `docs/wifi.md` 49) and `wifi status` prints exactly one of:

```
wifi: the radio is down - its firmware trapped at start (the chip came up warm); `wifi radio powercycle` cuts its power and tries again
wifi: the radio is down - the driver found no working radio on its bus; `wifi radio powercycle` restores the chip's power and tries again
wifi: the radio is down - the driver's bring-up stopped before it was up (the serial log names the stage); `wifi radio powercycle` tries again
wifi: this board's radio is there, but its driver is not written yet (the AIC8800 - docs/wifi-aic8800.md); nothing here can bring it up
```

The fourth is reason 4 (`DOWN_NOT_BUILT`), the VisionFive 2 Lite's radio, which this driver identifies and
cannot yet run. `wifi radio on` and `wifi radio powercycle` print the same sentence there and restart
nothing, since a restart would identify the same chip and stop at the same place. `wifi radio off hard` does
cut its power, and `wifi radio on` after it restores the power, restarts the driver and ends with this
sentence. Only reason 1 is ever reported as a chip that came up warm. A reason the shell does not know (0, or one added later) prints the generic `wifi: the radio
is down; `wifi radio powercycle` tries again`.

The earlier single line, "did not come up at boot", is gone: it was wrong after a respawn and named no cause.

```
gsh> wifi info
radio       on
network     Maple-House
bssid       02:1a:7e:c4:09:51
band        5GHz
channel     44
signal      excellent  -41 dBm
security    WPA2
joined      180 s ago
last scan   42 s ago
networks    11
addressing  type net (an IP address has one owner, and it is not this command)
```

`info` deliberately ends where `net` begins. Section 1 gives an IP address exactly one owner so that two
answers are never possible; the last line says where the other half of "am I online" lives rather than
printing a copy of it.

One exception to the live read, stated because it is deliberate: while a sweep runs the link is NOT read -
a control exchange takes frames off the bus and skips the ones that are not its reply, which mid-sweep
would be the scan's own results - so both views report the driver's memory and say the sweep is running,
which is the fact that matters then. An access point that has dropped the station is noticed on the next
`status` or `info` after the sweep ends.

### 4g. `wifi debug` - the driver's account of itself

Five views, each a word (rule 4). Bare `wifi debug` prints `stats`, `transport` and `events` together.
Every number is a raw count the driver keeps in its `Session` (rule 7); nothing is a verdict.

| View | What it prints |
|---|---|
| `wifi debug stats` | the control channel: requests sent, accepted, refused (and the last refusal's command and status), unanswered; the session's age on the driver's own clock |
| `wifi debug transport` | the SDIO side: function and block sizes, bytes each way, frames read by channel (control, event, data, glommed, flow-control, other), and the frames read during a control wait and lost to the scan |
| `wifi debug events` | how many of each firmware event this driver names has arrived, and the last one's code and status |
| `wifi debug firmware` | the chip, the image and its provenance, the running firmware's own version string and capability words (asked of it now, not remembered), its MAC, and the supplicant fact from `docs/wifi.md` §37 |
| `wifi debug trace` | the last 64 frames on the bus, oldest first |

```
gsh> wifi debug trace
        ms  frame     id     what
    12.184  TX CTRL   id=8   cmd=263 len=108
    12.192  RX CTRL   id=8   cmd=263 status=0 len=44
    12.431  RX EVENT         event=69 status=8 len=572
    12.612  RX EVENT         event=69 status=8 len=572
    14.004  RX EVENT         event=69 status=0 len=24
```

`RX GDESC` is a glom descriptor (it lists the lengths of the superframe that follows), `RX GLOM` the
superframe itself, and a trailing `*` marks an event or data frame taken out of one.

The trace is a fixed ring of 64 entries in the driver (§26.6.1), the oldest overwritten - two full scans'
worth. Timestamps are milliseconds since the driver's session began, by its own cycle counter; where the
kernel gave it no rate they read `0.000` everywhere rather than inventing a clock. Every reader of frames
feeds it - the control waits, the sweep, the join - so it is the whole traffic and not one path's view,
which is what makes it the instrument this port was built without: the frames a control exchange read and
skipped mid-sweep appear in it, and are counted under `skipped`.

### 4e. What this cost the driver, said plainly

`scan::collect` used to block the driver until the firmware reported the sweep complete, which is why
rule 11 was unmet and why `b` and `wifi list`-during-a-scan could not exist. All three needed one change,
made on 2026-09-29: the scan became a state (`scan::step`) that the driver's serve loop advances one frame
at a time, answering `list` (with the "scanning" error), `status` (with the count so far) and `abort`
(the firmware's own abort - a one-channel scan of channel -1, read from Linux before it was written)
between frames. That was the remaining debt of phase 3, and it is paid.

`wifi join` blocks through association and is escapable with `q`. Escaping it mid-handshake leaves
the radio not associated, and `wifi status` says so - a half-joined state is never reported as joined.

## 5. Failure is loud and says which half failed

The distinction a user needs is *whose fault it is*, and there are four different answers that all
look like "no internet":

| Situation | What `wifi` says |
|---|---|
| No `wifi-driver` running (no radio on this machine, or none driven yet) | `no wireless radio on this machine`, then ``(no `wifi-driver` is running - this machine has no radio, or none is driven yet)`` - and it is not an error to ask |
| Driver running, not answering | `wifi: the radio driver is not answering`, after a bounded wait - or, when it still owes answers to earlier requests, `wifi: not sent - the radio driver still owes N answer(s) ...` |
| Driver running, firmware trapped at start | `wifi: the radio is down - its firmware trapped at start (the chip came up warm); `wifi radio powercycle` cuts its power and tries again` |
| Driver running, no working radio on its bus | `wifi: the radio is down - the driver found no working radio on its bus; `wifi radio powercycle` restores the chip's power and tries again` |
| Driver running, bring-up stopped early | `wifi: the radio is down - the driver's bring-up stopped before it was up (the serial log names the stage); `wifi radio powercycle` tries again` |
| Driver running, the radio's driver not written yet (the VisionFive 2 Lite's AIC8800) | `wifi: this board's radio is there, but its driver is not written yet (the AIC8800 - docs/wifi-aic8800.md); nothing here can bring it up` |
| Chip powered down by `wifi radio off hard` | `radio off (hard - the chip is powered down; wifi radio on powers it up)` |
| Radio switched off by `wifi radio off` | `radio off (soft - the firmware's switch; the chip stays powered; wifi radio on turns it back on)` |
| SSID not found in a scan | `not joined - no network named <ssid> in range` - naming what was searched for |
| Wrong passphrase | `not joined - incorrect passphrase` - never "connection failed", which hides it |
| Associated, no lease | association reported as good; `net` owns the lease and says its own piece |

The wrong-passphrase row is the one worth being careful about. A wrong passphrase and a missing DHCP server both
end with "no network", and conflating them costs a user an hour. `wifi` reports association; `net`
reports addressing; neither editorialises about the other.

And the rule above the rules applies throughout: a radio that is absent, wedged or unplugged makes
`wifi` **return with a loud unavailable**, never hang. That is Commandment VIII at the command layer -
wait on the driver's reply or on the loud fact of its death, never on a timer.

## 6. Where the passphrase goes: a bounded table in the driver's memory, holding KEYS

Decided 2026-09-29, and it supersedes the keyring design `docs/wifi.md` §6 sketched:

- The shell prompts with invisible entry and hands the passphrase to the **driver**, which does not keep
  it. The moment it arrives the driver derives the **pairwise master key** from it and the network name
  (`PMK = PBKDF2-HMAC-SHA1(passphrase, ssid, 4096, 32)`, IEEE 802.11-2020 §12.7.1.2 - a few
  milliseconds) and the passphrase is gone. The key is the only thing ever needed again: to answer the
  handshake, to rejoin when the access point drops us, to roam. So the key is what is kept - **and it is
  kept only once it has joined.** A join that ends `not joined` keeps nothing, whatever the reason, and the
  next `wifi join <ssid>` asks for the passphrase again. A key that joined once and is later refused as
  incorrect (the network's passphrase changed) is dropped the same way. Asked for by the operator on
  2026-09-29 after the first handshake run: the driver kept the key on arrival, so every failed attempt had
  to be followed by `wifi forget` before the prompt would come back.
- **Sixty-four slots** of `(ssid, key)`, about 70 bytes each, in the driver's memory. Home, the cafe, the
  office and every hotspot after them are typed once and rejoined by name. When all sixty-four are held,
  the one JOINED LONGEST AGO is replaced. The number is a bound (CLAUDE.md §26.6) chosen so that nobody
  reaches it, not a fit to the 16 MiB the driver may use - a table that grew to fill what was available
  would be the elastic growth §26.6.1 says to resist, and its limit would be readable nowhere.
- **On disk since 2026-09-30, in `/wifi.keys`, by the operator's decision** - *"save passphrase on
  filesystem so that when the machine starts up, if there's wifi, it auto joins ... and ready to go"*,
  with the exposure named and accepted: *"if I were to unplug the usbstick and put it on another machine,
  that machine will have access to wifi passphrase (I'm ok with that for now)."* What the file holds is the
  DERIVED KEY of each network, never the passphrase text: it joins the network exactly as the passphrase
  would, so the card's holder can join it, but it does not reveal the passphrase itself, which people reuse
  elsewhere. Network names are in plain text. Nothing is encrypted at rest - there is no per-machine secret
  to encrypt with, and pretending otherwise would be a silent substitution (CLAUDE.md §26.4).
  The in-memory table stays the working set: the file is loaded once when the radio comes up and written
  after every change to the table (a join that added or re-ordered a key, a `forget`); where `fs` is absent
  or mid-restart the driver retries a bounded number of times, says so, and runs on the table alone, as it
  did before. At most 48 networks are saved, most recently used first. Open networks hold no key and are not
  saved. The previous decision - the table dying with the driver, *"better that than the kernel crashing"* -
  still governs the CRASH case: nothing above the kernel must survive, and a respawned driver reads the file
  back rather than carrying anything across its own death.
- **At boot the radio joins the network it last joined**, from the file, without being asked, and the cable
  still wins for the link. The log says `joining the network last joined, from /wifi.keys`, then the same
  lines a `wifi join` prints; if that network is out of range or refuses, `not joined` and the prompt is
  yours. `wifi radio on` after `radio off` does the same from memory (section 2).
- **`wifi join <ssid>` asks for a passphrase only when one is needed.** The shell sends the name
  alone first; the driver joins with the stored key if the slot holds that name, joins open if the last
  sweep heard the network as open, and otherwise answers *needs a passphrase* - at which point, and only
  then, the shell asks. A network joined once this boot is rejoined by name alone; one that has only
  been TRIED is asked for again.
- `wifi stored` prints the held names, one per line; `wifi forget <ssid>` zeroes that entry. Neither can print a key.
- The primitives that derive the key are checked against their published vectors at every boot (FIPS
  180-1, RFC 2202, RFC 6070, IEEE 802.11 Annex J.4.2). A wrong hash would be refused by every access
  point in a way indistinguishable from a wrong passphrase, so if the check fails, passphrases are
  refused with the reason rather than accepted into silence.

One residual, recorded here rather than left for a reader to work out: a USB keyboard driver sees the
passphrase as it is typed. That is the SEC-2 residual - `CONSOLE_PUSH` holders are inside the shell's
trust perimeter because keystrokes *are* commands - and it is inherent to being a keyboard, not a gap
in this design.

## 7. Built-in or service, and why it is both

`0_conventions.md` §2 says to prefer a standalone service whenever a command would otherwise run
beside dangerous authority. **That default is not followed here today.** Every verb is a shell built-in
routed through `cmd_wifi`, and every one asks the driver over the same send cap (`wifi_ask`), so
`list`, `status` and `stored` hold exactly the authority `join`, `forget` and `radio` do. Nothing
separates the read-only verbs from the others by construction; a read-only view with a narrower cap
would be a standalone service, and none has been built.

**`join` could not follow the default anyway, for a mechanical reason.** There is one console input ring with one
reader slot (`docs/console-service.md` §5a), and the shell is the reader. A spawned service cannot
prompt for a passphrase: it would have to take the input ring, and while it held it the shell could
not field the take/release messages. So the invisible-entry prompt has to happen **in the shell**,
which makes `join` a built-in. **`scan` follows it for the same reason**: the picker reads keys.

That is not a compromise so much as the right place anyway - the shell is where authority is decided
(CLAUDE.md Appendix D.4), and handing over a secret is exactly that decision. What the built-in must
not do is keep the secret: it reads it, hands it to the driver - which derives the key and drops the
passphrase (section 6) - and drops its own copy.

## 8. Tab completion (rule 9)

`wifi` completes from the command table; its subcommands from `SUBCMD_FIRST` in
`services/shell/src/main.rs`, which is also what `doc_command_check.py` reads - so this document
cannot come to show a subcommand the shell does not offer. Two subcommands complete one level further,
from `SUBCMD_SECOND`: `radio` to `on`, `off` and `powercycle` (`off hard` is typed, not completed), and
`debug` to `events`, `stats`, `firmware`, `transport` and `trace`. An SSID argument completes from `wifi stored`, never from the last scan, because
completing from a scan would leak the names of networks in range into a shell's history.

## 9. Not in scope

Following `docs/wifi.md` §9, and for the same reasons: no enterprise or 802.1X, no WPA3, no WEP ever
(a WEP row is listed because it was heard, and refused at the pick), no access-point mode, no manual
channel selection, no joining a hidden network in the first version (the join request carries only an
SSID, so neither the picker nor `wifi join <ssid>` can reach one), and no signal-strength monitor - `observe` is where live views
live, and a second one here would be the duplication rule 7 exists to prevent.

## 10. Which link carries the frames: the cable always wins

Decided by the operator on 2026-09-29, after the first joined boot: *"cable always wins. unplug the
cable, switch to wifi automatically."*

- **`wifi join` makes the radio available; the cable decides.** While the ethernet cable reports a link,
  every frame goes over it. Pull the cable and, within about a second, the frames go over the radio - if
  it is joined. Plug it back and they return to the cable. Nothing is typed for either switch.
- **`wifi leave` and `radio off` take the radio out of that choice**; with the cable out too, the link is
  down and `net` says so.
- **`net` names the carrier**: `link  up via the cable`, `link  up via wifi (the cable is out)`, or
  `link  down - no cable, and the radio is not joined`. `wifi status` says whether the radio is JOINED,
  which is a different fact: a joined radio with the cable in is standing by.
- **A switch re-configures the network.** The radio has its own address, so the stack sees a different
  link and asks for a lease again (`docs/wifi.md` 41). A `ping` in flight across the switch loses its
  replies for a second or two; that is the switch, not a fault.
- **Where the rule lives.** In `nic-driver`, which is the link front end on every board (`docs/wifi.md`
  2) - one comparison, `Carrier` in its genet backend. `net-stack` never learns there are two links, and
  the `wifi` utility never learns there is a cable.

## 11. Which radio: `wifi hardware` - SPECIFIED, NOT BUILT (2026-10-06)

Agreed with the operator on 2026-10-06, while the USB dongle was being brought to `xhci` (`docs/wifi-usb.md`
section 7, U2). Until then every machine has had at most one radio, and the shell takes the first of its
radio services that is running (`RADIOS`: `wifi-driver`, then `wifi-usb`). A Pi 4 or a VisionFive with
the dongle plugged in has two, and that rule would leave the dongle unreachable.

**`wifi hardware`** is a report, one row per radio, and like every report it pipes (rule 12): records with
the columns `radio`, `chip`, `bus`, `state`, `network` and `in_use`.

```
radio      chip         bus                 state    network     in use
onboard    CYW43455     SDIO                joined   home-5g     *
usb-1a0d   RTL8188CUS   USB xhci port 3     on       -
usb-77e2   RTL8188CUS   USB xhci port 4     off      -
```

**How a radio is named - by what it is, not where it is** (CLAUDE.md invariant 11, identity is stable and
location is not):
- **`onboard`** for the board's own radio.
- **`usb`** for a dongle, when there is one.
- **`usb-` and the last two bytes of its MAC address** (`usb-1a0d`) for each dongle when there is more
  than one. The address is in the dongle's own efuse, so the name follows the dongle into any socket, on
  any board, across any reboot. Numbering them in the order they came up would swap after a replug, and
  naming them by socket would make one dongle a different radio when it moved. The socket is still shown,
  in `bus`, so a reader can tell which stick is which.

**`wifi hardware use <radio>`** chooses the radio the other `wifi` verbs address and that carries the
frames when the cable is out. It is an action and does not pipe.
- **The choice lives in `nic-driver`, not in the shell.** The radio that carries the traffic is the one
  `nic-driver`'s bridge talks to (section 10), so a choice held by the shell alone would let `wifi status`
  describe one radio while the frames went through the other. `use` tells `nic-driver`; the shell asks
  `nic-driver` which radio is in use and follows its answer. One fact, one owner.
- **The cable still always wins** (section 10). `use` chooses among radios; it does not choose the radio
  over the cable.
- **Refused, with a sentence:**
  - a name that is not in the report: `no radio called 'usb-9999' - wifi hardware lists them`;
  - plain `usb` when there are two dongles: `there are two USB radios - usb-1a0d or usb-77e2`.
- **The default** is today's order: the onboard radio, else the dongle - so on a machine with no onboard
  radio (the PCs) the dongle is the default.
- **A choice is saved, by its owner.** `nic-driver` holds the choice, so `nic-driver` keeps it, in
  `/wifi.radio`, as `wifi-driver` keeps `/wifi.keys`, and reads it back at boot and on every respawn. That
  gives `nic-driver` `fs` as a peer, a grant pinned with this reason. The shell saving it and telling
  `nic-driver` at each start was the alternative, and it splits one fact across two services.
- **The file holds the full name** (`usb-1a0d`), even while the report shows plain `usb` because there is
  one dongle, so a second dongle plugged in later does not make the saved choice ambiguous.
- **A saved choice is a preference, not a requirement.** When the chosen radio is not there (the dongle is
  unplugged), the default order carries the link and `wifi hardware` says so on the chosen radio's row;
  when it comes back, the link returns to it. Unplugging a dongle must not leave a machine without its
  other radio, and plugging it back must not need choosing again. `wifi hardware use onboard` (or the
  radio the default would pick) clears the file rather than writing the default into it.

**What `use` does, walked through on a Pi 4 with its onboard radio joined and a dongle plugged in.**
1. The dongle is already up before anyone types `use`: `wifi-usb` runs while its dongle is attached (on the Pi 2
   the supervisor starts it when the host reports the dongle), and plugging the dongle in is what binds it and brings the chip up (a few seconds). `use` on a radio still coming up waits for
   it, bounded, and says so.
2. `use usb` records the choice in `nic-driver` and `/wifi.radio`.
3. `nic-driver` asks the radio in use what it is joined to, and asks the chosen one to join that network.
   The key is read from `/wifi.keys`, which both radio drivers share, so nothing is typed.
4. Only once the chosen radio reports JOINED do the frames move to it - so a join that fails loses
   nothing: `use` says why, and the other radio keeps the link.
5. The new radio has its own address, so the stack asks for a lease again: the same second or two a ping
   loses across a cable-to-radio switch (section 10).
6. The other radio then leaves the network and idles, powered, so `use` back is steps 3 to 5 again.

**What that needs, beyond the report and the choice:**
- **Only the radio in use auto-joins.** Today each radio driver rejoins the last network from
  `/wifi.keys` at boot; with two, both would join the same access point. A radio auto-joins only when
  `nic-driver` says it is the one in use.
- **`/wifi.keys` gets two writers.** Each driver rewrites the file from its own table, so one radio's save
  could drop a key the other added. A save re-reads the file and merges before writing.
- **`nic-driver` asks a radio what it is joined to**, which it does not do today.

**More than one dongle is named now and built later.** Today each USB host binds one radio, and one
`wifi-usb` drives one dongle. Two need either a `wifi-usb` per dongle or one serving several, plus a host
that binds more than one. The names and the report are designed for many so they need no change then;
the support itself waits until two are plugged in (CLAUDE.md 26.2).

**Tab completion** (rule 9): `hardware`, then `use`, then the names in the report.
