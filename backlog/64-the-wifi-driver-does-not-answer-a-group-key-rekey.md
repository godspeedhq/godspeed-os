# 64. The wifi driver does not answer a group-key rekey, so the link drops at the access point's rekey interval

**Status: OPEN - recorded with phase 5 rather than closed (CLAUDE.md 26.7).**
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

## What closing it takes

- Keep the KCK (and the KEK) after the join, per joined network, in the driver's memory alongside the
  PMK - about 32 bytes more per credential slot.
- In `frames::pull`, recognise the group-key message 1 (`info` with `GROUP` set and `PAIRWISE` clear,
  `KEYACK | KEYMIC | ENCRYPTED`), verify its MIC with the KCK, unwrap its key data with the KEK, find the
  GTK KDE (`eapol::find_gtk` already does this for message 3 of the four-way), install it with
  `ctrl::install_key` at its key id with `PRIMARY_KEY`, and send message 2 (`GROUP | KEYMIC | SECURE`,
  empty, MIC'd) with `eapol::build_key_frame`. OpenBSD's `ieee80211_recv_group_msg1` and
  `ieee80211_send_group_msg2` (`net80211/ieee80211_pae_input.c`, `ieee80211_pae_output.c`) are the
  reference, and every primitive it needs is already in `eapol.rs` and `crypto.rs`.
- The PTK rekey (a full four-way handshake initiated by the access point after the join) is the same
  work one step larger: `join.rs`'s message-1 path run from the pull rather than from a join.

## Why it is recorded and not done in the same change

Phase 5 is the frame path, one change per flash, with a prediction that can be wrong. A rekey answer is
a second protocol on the same frames with its own failure modes, and the first thing to learn is what
this access point's interval IS - the log line above will say when it fires. It should be done before
the wifi link is relied on for anything longer than a session.
