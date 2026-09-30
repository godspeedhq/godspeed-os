# 64. The wifi driver does not answer a group-key rekey, so the link drops at the access point's rekey interval

**Status: BUILT in full 2026-09-30 - the group-key half (`frames::group_rekey`) and, that evening, the pairwise
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

## Why it is recorded and not done in the same change

Phase 5 is the frame path, one change per flash, with a prediction that can be wrong. A rekey answer is
a second protocol on the same frames with its own failure modes, and the first thing to learn is what
this access point's interval IS - the log line above will say when it fires. It should be done before
the wifi link is relied on for anything longer than a session.
