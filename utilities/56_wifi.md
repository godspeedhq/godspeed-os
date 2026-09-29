<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `wifi` - join and inspect a wireless network

Version reported by `wifi version`. Implementation shape: **shell built-in for `scan` and `connect`,
standalone service for everything else** - see section 7, where the reason is a constraint rather than
a preference.

## Status, as built and honest (2026-09-29)

**The radio scans and lists on the Raspberry Pi 4, and has since 2026-09-28.** The shell asks the
`wifi-driver` over IPC, the radio sweeps, and one record per network prints in the order this file
specifies - `ssid signal band security`, with `security` read from the beacon (an RSN element is WPA2, a
Microsoft-OUI vendor element is WPA, the Privacy bit alone is WEP). Eleven networks on the first run, and
a second run straight after scanned again on the same driver session. About three seconds each, ending
when the firmware says the scan is complete rather than when a timer runs out.

**The `scan` / `list` split is BUILT (2026-09-29) and not yet run on hardware.** Sections 2 to 4 are the
spec; the shell and the driver now answer them: `wifi scan` starts a sweep the driver advances one frame
at a time from its serve loop, prints rows as they are heard with `scanning  [q] quit  [b] background`
above them, and ends in the numbered picker; `wifi list` prints the driver's cache and refuses (with the
two sentences in section 3) while a sweep runs or when none has run. The cache lives in the driver's
memory - not in a file, so it needs no `fs`, dies with the driver (a respawned driver says `no scan yet`),
and never writes a neighbour's network name to the card. The first boot of this build is the test; every
"works" below that is not marked hardware-verified is a build that compiles and passes the gate, no more.

What every verb does today:

- `wifi scan` - built. Start, live numbered rows, `q` (stops the SWEEP with the firmware's own abort - a
  one-channel scan of channel -1, the form Linux uses - and discards the partial), `b` (returns the
  prompt, the sweep finishes into the cache), the picker (a number and Enter; `open` joins at once, `WEP`
  is refused, anything else asks the passphrase). Unverified on hardware.
- `wifi list` - built to section 3: cache only, never scans. **The scan itself is hardware-verified**
  (2026-09-28, as the old blocking `wifi list`); the cache path is not yet.
- `wifi status` - built to section 4f: radio, network/band/BSSID, signal, security, time joined - read
  from the firmware on each call (`GET_BSSID`, `GET_RSSI`, `chanspec`) except mid-sweep - and the sweep
  state or the last scan's age. Bare `wifi` prints usage. Unverified on hardware.
- `wifi stored`, `wifi forget <ssid>` - built to section 6: the one slot, holding a derived key and never
  a passphrase. `wifi connect` asks for a passphrase only when the driver says one is needed. The key
  derivation self-tests against four published vectors at boot (`stage 0` in the driver's log). Unverified.
- `wifi disconnect`, `wifi radio on|off` - built as `bwfm_newstate` and `bwfm_stop` do them (`DISASSOC`
  with no payload; `DOWN 1`, and `UP` again through the same chain boot uses). Unverified on hardware.
- `wifi connect <ssid>` - built, and on a WPA2 network **cannot complete on this firmware as built**:
  `sup_wpa` is refused in every form, so this firmware (7.45.265) has no supplicant and the host must run
  the 4-way handshake (`docs/wifi.md` §37). Today it associates, reads the access point's first handshake
  message, and reports honestly that it cannot answer it (reply 15). An OPEN network has no handshake, and
  joining one is expected to work - that is the first join this build can complete. Unverified.
- `wifi help`, `wifi version`, tab completion including `radio on|off`, and a row in `help`.
- Absence is told apart from a wedge (section 5): no live `wifi-driver` means no radio; a live one that will
  not answer says that after a bounded wait, never a guess.

**Things this file specifies that are NOT met yet, said here rather than discovered:**

- The WPA2 join (the host supplicant), so `connect` and the picker's passphrase path end in reply 15.
- The passphrase prompt cannot be abandoned. `read_input_line` ignores every control byte, so Esc and
  `^Q` do nothing and the only ways out are Enter (which sends what was typed) or a passphrase too short
  to send. Section 4 says Esc or `^Q` leaves; that is a change to the reader.
- The BSSID and band in the `joined` sentence (section 4b) are not yet read from the association event;
  the sentence names the network only. `wifi status` reads both live, so they are one command away.
- An access point that drops the station is noticed only when `wifi status` next reads the link (section
  4f), not the moment it happens; the driver does not yet watch `LINK` events outside a join or a sweep.

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
| `wifi connect <ssid>` | join a network by name. Prompts for the passphrase if one is needed and none is stored |
| `wifi disconnect` | leave the current network. The radio stays up |
| `wifi status` | what is true NOW, read from the firmware: radio, network, band, BSSID, signal, security, time joined - and the sweep or the last scan's age. Section 4f |
| `wifi forget <ssid>` | drop the held key for that network. Does not disconnect |
| `wifi stored` | the network a key is held for. A name, never a secret. One slot - section 6 |
| `wifi radio on` / `wifi radio off` | power the radio. `off` disconnects first and says so |
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

`wifi connect` takes **no** passphrase argument, in any position, and that is a security decision
rather than an ergonomic one: an argument would be recorded in the recall ring and written to
`/.gsh_history`, where an up-arrow recovers it. The passphrase is only ever read through the shell's
invisible-entry path (`input secret`, `docs/scripting.md` §8), which is already excluded from both.

## 3. Output is a pipeable structure (rule 12)

`wifi list` is a producer. One record per network, so the existing pipes work with no special case:

```
wifi list | match WPA2
wifi list | count
wifi list | write /networks.txt
```

Fields, in order: `ssid`, `signal`, `band`, `security`. Signal is reported as **dBm, a raw fact**, not
as bars or as a "good/fair/poor" verdict - rule 7. A reader who wants bars can derive them; a reader
given bars cannot recover dBm.

**The columns are fixed-width, and they can be because the widest is known.** An SSID is at most 32
bytes - that is the size of the field in the beacon, so a longer one cannot exist - and every other
field has a bounded vocabulary. So the layout is `ssid` padded to 32, `signal` right-aligned to 8
(`-41 dBm`, room for `-100 dBm`), `band` padded to 6 (`2.4GHz` or `5GHz`), then `security`, two spaces between columns:

```
gsh> wifi list
Maple-House                        -41 dBm  5GHz    WPA2
Maple-House                        -47 dBm  2.4GHz  WPA2
(hidden)                           -63 dBm  5GHz    WPA2
BT-Hub6-K7QR                       -71 dBm  2.4GHz  WPA2/WPA
Riverside Tenant WiFi Guest Netw   -74 dBm  2.4GHz  open
xfinitywifi                        -79 dBm  2.4GHz  open
SKY7F2B1                           -80 dBm  5GHz    WPA2
PrinterDirect-4A                   -82 dBm  2.4GHz  WEP
(unprintable)                      -85 dBm  2.4GHz  WPA2
a                                  -86 dBm  5GHz    WPA
Free_Cafe_WiFi                     -88 dBm  2.4GHz  open
```

Nothing is ever truncated: the fifth row is exactly 32 bytes and fills its column edge to edge. A
network that does not announce its name prints `(hidden)`; a byte outside printable ASCII prints as a
dot rather than reaching the terminal as a control code. `security` is one of `open`, `WEP`, `WPA`, `WPA2`,
`WPA2/WPA`, read from the beacon. Two rows with the same name are two access points - the same
network on 2.4 GHz and 5 GHz is the usual case, and both are listed because both were heard.

Rows are in the order the radio heard them, and `list` prints the cache in that order every time. A
sorted view is one pipe away (`wifi list | sort`), and keeping the producer unsorted is what lets the
picker's numbers in section 4 stay put.

**`wifi list` never scans, and says so when there is nothing to print:**

| State of the cache | What `wifi list` prints |
|---|---|
| a completed scan | its records, as above |
| no scan since boot | `no scan yet - run wifi scan` (an error, not an empty list) |
| a scan is running | `scanning - 9 heard so far; wifi list when it finishes` (an error, not the OLD cache) |
| the radio never came up | `wifi: the radio is not up - ...` (section 5) |

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
    ssid                                signal  band    security
 1  Maple-House                        -41 dBm  5GHz    WPA2
 2  Maple-House                        -47 dBm  2.4GHz  WPA2
 3  (hidden)                           -63 dBm  5GHz    WPA2
 4  BT-Hub6-K7QR                       -71 dBm  2.4GHz  WPA2/WPA
 5  Riverside Tenant WiFi Guest Netw   -74 dBm  2.4GHz  open
 6  xfinitywifi                        -79 dBm  2.4GHz  open
 7  SKY7F2B1                           -80 dBm  5GHz    WPA2
 8  PrinterDirect-4A                   -82 dBm  2.4GHz  WEP
 9  (unprintable)                      -85 dBm  2.4GHz  WPA2
10  a                                  -86 dBm  5GHz    WPA
11  Free_Cafe_WiFi                     -88 dBm  2.4GHz  open
11 networks in 2.8 s
join: type a number and Enter, [q] quit
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
 1  Maple-House                        -41 dBm  5GHz    WPA2
passphrase (not shown):
joining Maple-House  [q] quit
joined Maple-House on 5GHz, bssid 02:1a:7e:c4:09:51 - the link is up and the handshake completed
  (addressing is `net`'s to report: `net status`)
```

The BSSID and band in the last line come from the association event, not from the row - the row said
which network was asked for, the event says which access point answered, and where they differ the
event is the truth. `wifi status` repeats it.

An `open` network skips the passphrase prompt. A `WEP` network is refused at the pick with `WEP is
not supported` (section 9) rather than asked for a key it will not use. A `(hidden)` row can be picked
- the radio knows the BSSID even though the name was withheld - but joining by name with `wifi
connect` cannot reach it, and that is the one thing the picker can do that the name path cannot.

The passphrase prompt is a typing surface, so `q` is a letter there. Esc or `^Q` abandons it (rule
10a), and `wifi status` then reports not associated. A prompt abandoned sends nothing.

### 4c. The two keys while scanning

**`q` stops the SWEEP, not just the shell's interest in it** (rule 11). The driver is told to abort
the scan and the radio stops. The rows already on screen were a partial hearing of the room and are
**not kept**: the cache holds the last complete scan, and a half-scan replacing it would be a view that
lies about how much of the room it saw. It says so:

```
scan stopped - 4 heard, not kept; the last complete scan (11 networks, 3 min ago) stands
```

**`b` returns the prompt and leaves the radio sweeping.** The same key `foreground` uses for the same
meaning (`55_background.md` §2). Nothing is printed to the console after that - a detached scan has
no console, exactly as a detached copy has none - and the results land in the cache when the sweep
ends. The picker never appears; `wifi list` and `wifi connect <ssid>` are the way in:

```
gsh> wifi scan
scanning  [q] quit  [b] background
    ssid                                signal  band    security
 1  Maple-House                        -41 dBm  5GHz    WPA2
 2  Maple-House                        -47 dBm  2.4GHz  WPA2
scan continues in the driver - wifi list when it finishes, wifi status meanwhile
gsh> wifi status
radio up, not associated
scan running - 6 heard so far
gsh> wifi list
scanning - 9 heard so far; wifi list when it finishes
gsh> wifi list
Maple-House                        -41 dBm  5GHz    WPA2
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

### 4f. `wifi status` is the now-view

Labelled lines, so it pipes (`wifi status | match signal`), and every link fact on it is READ from the
firmware when asked - `BSSID`, `RSSI`, `chanspec` - not remembered from the last join:

```
gsh> wifi status
radio      on
network    Maple-House  5GHz  bssid 02:1a:7e:c4:09:51
signal     -41 dBm  excellent
security   WPA2
joined     3 min ago
last scan  42 s ago, 11 networks
```

The word after the dBm is the one place this utility says more than the number, at the operator's request,
and it is a stated rule rather than a mood so a reader can check it: -50 dBm or stronger is `excellent`,
to -60 `good`, to -70 `fair`, weaker is `weak`. The number stays first and stays the fact (rule 7); the
scan rows carry no word, because a pipe should not have to strip an adjective.

Not associated: `network    none (not associated)`, and no signal, security or joined lines. Radio off:
`radio      off` and the same. The scan line is one of `scan       running - N heard so far`,
`last scan  N s ago, M networks`, or `last scan  none - run wifi scan`.

One exception, stated because it is deliberate: while a sweep runs the link is NOT read - a control exchange
takes frames off the bus and skips the ones that are not its reply, which mid-sweep would be the scan's own
results - so the status reports the driver's memory and says the sweep is running, which is the fact that
matters then. An access point that has dropped the station is noticed on the next `wifi status` after the
sweep ends, and the remembered join is cleared then.

Addressing is `net`'s (section 1). Nothing here is an IP address, and nothing here will be.

### 4e. What this costs the driver, said plainly

Today `scan::collect` blocks the driver until the firmware reports the sweep complete, which is why
rule 11 is unmet and why `b` and `wifi list`-during-a-scan cannot exist yet. All three need the same
one change: the scan becomes a state the driver's serve loop advances one frame at a time, so that
between frames it can answer `list` (with the "scanning" error), `status` (with the count so far) and
`abort` (which sends the firmware's escan abort action - read from the reference before it is written,
not assumed). That is the remaining debt of phase 3, and this section is what pays it off.

`wifi connect` blocks through association and is escapable with `q`. Escaping it mid-handshake leaves
the radio not associated, and `wifi status` says so - a half-joined state is never reported as joined.

## 5. Failure is loud and says which half failed

The distinction a user needs is *whose fault it is*, and there are four different answers that all
look like "no internet":

| Situation | What `wifi` says |
|---|---|
| No radio on this machine | `no wireless hardware on this machine` - and it is not an error to ask |
| Radio present, driver not running | `wifi: the radio driver is not answering` (peer is dead; not a timeout guess) |
| SSID not found in a scan | `no network named <ssid> in range` - naming what was searched for |
| Wrong passphrase | `<ssid> refused the passphrase` - never "connection failed", which hides it |
| Associated, no lease | association reported as good; `net` owns the lease and says its own piece |

The fourth row is the one worth being careful about. A wrong passphrase and a missing DHCP server both
end with "no network", and conflating them costs a user an hour. `wifi` reports association; `net`
reports addressing; neither editorialises about the other.

And the rule above the rules applies throughout: a radio that is absent, wedged or unplugged makes
`wifi` **return with a loud unavailable**, never hang. That is Commandment VIII at the command layer -
wait on the driver's reply or on the loud fact of its death, never on a timer.

## 6. Where the passphrase goes: one slot, in the driver's memory, holding the KEY

Decided 2026-09-29, and it supersedes the keyring design `docs/wifi.md` §6 sketched:

- The shell prompts with invisible entry and hands the passphrase to the **driver**, which does not keep
  it. The moment it arrives the driver derives the **pairwise master key** from it and the network name
  (`PMK = PBKDF2-HMAC-SHA1(passphrase, ssid, 4096, 32)`, IEEE 802.11-2020 §12.7.1.2 - a few
  milliseconds) and the passphrase is gone. The key is the only thing ever needed again: to answer the
  handshake, to rejoin when the access point drops us, to roam. So the key is what is kept.
- **One slot**: `(ssid, key)`, about 70 bytes, in the driver's memory. Joining a second network replaces
  the first - the cafe replaces home, and home is typed once more on return. That is the honest cost of
  one slot without a disk; widening it is a constant, and is not done until wanted.
- **Not on disk, on purpose.** No `fs` under a join; no "not encrypted at rest" caveat; no network name of
  the operator's written to a card. The slot dies with the driver - a reboot or a driver respawn empties
  it - and `wifi connect` simply asks again. Persistence across reboots is a separate decision that can
  layer on top; nothing here forecloses it.
- **`wifi connect <ssid>` asks for a passphrase only when one is needed.** The shell sends the name
  alone first; the driver joins with the stored key if the slot holds that name, joins open if the last
  sweep heard the network as open, and otherwise answers *needs a passphrase* - at which point, and only
  then, the shell asks. A network joined once this boot is rejoined by name alone.
- `wifi stored` prints the slot's name; `wifi forget <ssid>` zeroes the slot. Neither can print a key.
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
beside dangerous authority. That is the right default here and it is followed for every read-only
verb: `list`, `status`, `stored` hold an introspection-shaped cap to the radio service and nothing
more, so they cannot join, forget or power anything *by construction*.

**`connect` cannot follow it, for a mechanical reason.** There is one console input ring with one
reader slot (`docs/console-service.md` §5a), and the shell is the reader. A spawned service cannot
prompt for a passphrase: it would have to take the input ring, and while it held it the shell could
not field the take/release messages. So the invisible-entry prompt has to happen **in the shell**,
which makes `connect` a built-in. **`scan` follows it for the same reason**: the picker reads keys.

That is not a compromise so much as the right place anyway - the shell is where authority is decided
(CLAUDE.md Appendix D.4), and handing over a secret is exactly that decision. What the built-in must
not do is keep the secret: it reads it, sends it to the keyring, and drops it.

## 8. Tab completion (rule 9)

`wifi` completes from the command table; its subcommands from `SUBCMD_FIRST` in
`services/shell/src/main.rs`, which is also what `doc_command_check.py` reads - so this document
cannot come to show a subcommand the shell does not offer. `radio` completes one level further, to
`on` and `off`. An SSID argument completes from `wifi stored`, never from the last scan, because
completing from a scan would leak the names of networks in range into a shell's history.

## 9. Not in scope

Following `docs/wifi.md` §9, and for the same reasons: no enterprise or 802.1X, no WPA3, no WEP ever
(a WEP row is listed because it was heard, and refused at the pick), no access-point mode, no manual
channel selection, no hidden-SSID entry by name in the first version (the picker reaches one by
BSSID; `wifi connect <ssid>` does not), and no signal-strength monitor - `observe` is where live views
live, and a second one here would be the duplication rule 7 exists to prevent.
