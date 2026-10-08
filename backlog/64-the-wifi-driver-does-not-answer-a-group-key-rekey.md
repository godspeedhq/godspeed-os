# 64. The wifi driver does not answer a group-key rekey, so the link drops at the access point's rekey interval

**Status: BUILT in full 2026-09-30 - the group-key half (now `godspeed_wifi::supplicant::group_rekey`, moved out of `frames`) and, that evening, the pairwise
half (`frames::pairwise_rekey`, driving the same `join::Handshake` the join uses). OPEN only for the evidence:
neither has yet been seen on hardware, because the access point decides when to rekey. CLOSED when one of
each has run with the link staying up.**
**Opened:** 2026-09-29, with phase 5 of `docs/wifi.md` (the frame path).

## What happens

A WPA2 access point replaces the group temporal key on a timer - commonly every hour, on some routers
every day, on some never. It does so with a two-message group-key handshake: an EAPOL-Key frame to the
station (`GROUP | KEYACK | KEYMIC | SECURE | ENCRYPTED`, the new GTK wrapped in its key data), which the
station must answer with a MIC'd acknowledgement. A station that does not answer is deauthenticated after
the access point's retry budget.

`services/wifi-driver` runs the four-way handshake at join (`join.rs`) and then keeps only the outcome:
the pairwise and group keys are installed in the firmware and the PTK is dropped. After the join, an
EAPOL-Key frame that arrives is COUNTED by the frame pull (`frames::Pulled::rekey`) and logged once:

```
wifi-driver: an EAPOL-Key frame arrived AFTER the join - the access point is rekeying the group key,
which this driver does not answer yet; the link drops at its rekey interval and `wifi join` brings it back
```

and the link is then lost at the access point's discretion. The driver sees the drop (a `LINK` event
without its up bit, or a deauthentication) and forgets the join loudly, so `wifi status` says not joined
and `net` says the link is down; nothing hangs and nothing pretends. `wifi join <ssid>` rejoins with the
key it holds.

## What was built (2026-09-30)

The join keeps `join::Keys` - the KCK, the KEK, the last replay counter and our address - for the life of
the association, zeroed on leave, radio off, a dropped link and at the start of the next join. The frame
pull hands any EAPOL-Key frame that arrives after the join to `frames::group_rekey`, which follows
`ieee80211_recv_rsn_group_msg1` step by step: not pairwise, `KEYMIC | KEYACK`, replay above the last
accepted, MIC under the KCK, key data `ENCRYPTED` and unwrapped under the KEK, the GTK KDE found, the key
installed FIRST (an acknowledgement for a key the firmware refused would tell the access point to use a
key we do not have), then `ieee80211_send_group_msg2`'s frame: `KEYMIC | SECURE`, the replay copied, no
key data, signed. Every refusal is logged with its reason. `docs/wifi.md` 42.

## What closing it takes

- ~~The group half~~ - built, above. To CLOSE it: one rekey observed on hardware, the log showing
  `group key N re-installed and acknowledged (replay R) - the access point rekeyed` and the link staying
  up past it.
- ~~The PTK rekey~~ - built the same evening: the four-way handshake became `join::Handshake`, a struct
  fed one key frame at a time, and `frames::pairwise_rekey` drives it from the pull with the PMK the
  association was made with (kept in `join::Keys`), reading the frames that follow for up to two seconds
  and queueing any data frames among them. Success replaces the keys in place; the log says `pairwise
  rekey complete - new pairwise and group keys installed, the link continues`.

## The first soak, 2026-10-04: the link died of IDLENESS before any rekey

The handlers had never run because no session had stayed joined long enough: the longest of 68 logs since
the driver was built stayed joined 16.5 minutes, and access points commonly rekey hourly. So a Pi 4 was
joined at boot (`a92808f6`) and left idle at the prompt to wait for one.

It did not get there. **Six minutes after the join the access point dropped the station** - `wifi-driver:
the access point dropped the link (event 12 - DISASSOC_IND, reason 4)` at 00:50:18 against `JOINED` at
00:44:10. 802.11 reason 4 is "disassociated due to inactivity": the access point decided this station was
gone. It is NOT a rekey failure - that would be reason 16, the group-key handshake timing out - and no
EAPOL-Key frame arrived before it. The driver then stayed unjoined (`wifi join` returns) until the board
was switched off at 01:28; the machine itself ran normally throughout.

So the rekey handlers are still unexercised, and there is a NEW prerequisite in front of them: **an idle
link has to survive past the access point's inactivity timer** (about five minutes on this one). Next: find
what the access point expected from an idle station and did not get - the firmware's own keep-alive (the
Broadcom firmware has a periodic keep-alive frame facility, which Linux's brcmfmac does not configure by
default), or simply no traffic at all from `net-stack` on an idle link - and whether the driver should
rejoin on its own after an inactivity drop rather than wait for `wifi join`. Then the soak again.

**What the references do, read 2026-10-04.** Both SET the power-save mode explicitly and this driver
never has (`ctrl::interface_up` defers it): OpenBSD's `bwfm_init` uses fast power-save for every station,
Linux's `brcmfmac` uses `PM_FAST` when power saving is on (its default). Linux's null-frame keep-alive
(`mkeep_alive`) is configured only for suspend, so it is not what keeps a Linux station associated - a
Linux machine is simply never silent for five minutes. Reason 4 after ~300 s idle is the shape of an access
point (hostapd's `ap_max_inactivity` defaults to 300 s) polling a silent station with a null frame and
getting no acknowledgement.

**The next soak, one change, prediction first.** The driver now logs the firmware's power-save mode after
every join (`ctrl::report_power_mode`, read only - nothing is set). Join, then keep a trickle of traffic
going at the prompt:

    loop { ping count 1 192.168.11.1; if !wait 60 { break } }

Prediction, if inactivity is the whole story: the link outlives the six-minute mark and the soak reaches the
rekey. If it drops anyway with reason 4, traffic is not what the access point wants and the mode the log
printed is the next thing to change - to fast power-save, as both references do.

## Why it is recorded and not done in the same change

Phase 5 is the frame path, one change per flash, with a prediction that can be wrong. A rekey answer is
a second protocol on the same frames with its own failure modes, and the first thing to learn is what
this access point's interval IS - the log line above will say when it fires. It should be done before
the wifi link is relied on for anything longer than a session.
