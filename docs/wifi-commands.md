<!-- SPDX-License-Identifier: GPL-2.0-only -->
# The `wifi` command surface (design; the verb does not exist yet)

**Status: DESIGN, nothing built.** `docs/wifi.md` is the driver and credential design; this is the
command surface, written first so the shape is settled before any SDIO register is touched. Every
invocation below is a PROPOSAL, and none is written as a `gsh>` prompt, because a prompt in this
repository asserts that something was typed and worked.

**Why this is in `docs/` and not in `utilities/`, which is where a command surface normally lives.**
It was first written as a numbered spec under `utilities/`, and the enforcement layer refused it
within a minute:
`X-user-vocabulary` (Commandment X) reported that `wifi` has a utility spec and the shell answers no
such verb. That is not pedantry - it is the invariant that makes the directory worth trusting. A spec
under `utilities/` asserts the verb EXISTS, which is what lets `doc_command_check` read the shell's own
completion table and hold every document to it. The check offers three ways out: implement it, delete
it, or title it "not provided" as `utilities/14_poweroff.md` does. None fits a verb we fully intend to
build, and the fourth option - adding `wifi` to `utility_vocab_debt` - is exactly the baseline abuse
`CONTRIBUTING.md` names as weakening a gate. So the document was in the wrong place, not wrong.
`docs/tcp-design.md` and `utilities/48_tcp.md` are the same split: design here, surface there once it
answers. This file moves to `utilities/` the day the shell does.

Version reported by `wifi version`. Implementation shape: **shell built-in for `connect`, standalone
service for everything else** - see section 7, where the reason is a constraint rather than a
preference.

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
| `wifi` | with no args: the association summary, the same as `wifi status` |
| `wifi list` | scan and list the networks in range: SSID, signal, band, security |
| `wifi connect <ssid>` | join a network. Prompts for the passphrase if one is needed and none is stored |
| `wifi disconnect` | leave the current network. The radio stays up |
| `wifi status` | radio state, current SSID, signal, security, association time |
| `wifi forget <ssid>` | delete a stored credential. Does not disconnect |
| `wifi stored` | list the SSIDs a credential is held for. Names only, never secrets |
| `wifi radio on` / `wifi radio off` | power the radio. `off` disconnects first and says so |
| `wifi help` | usage, with one real example per row |
| `wifi version` | version number plus the collective copyright line |

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

`wifi stored` is a producer of one field, `ssid`. It exists so that "which networks do I have a
password for" is answerable without a way to ask "what is the password", which is not a supported
question at any privilege level.

## 4. Scanning blocks, so it is escapable (rules 10 and 11)

A scan takes real time on real hardware - the radio sweeps channels. `wifi list` is therefore
escapable with `q`, and per rule 11 quitting stops the SCAN, not just the shell's interest in it: the
service is told to abandon it, so the radio is not left sweeping for a listener that has gone.

`wifi connect` blocks through association and is escapable the same way. Escaping it mid-handshake
leaves the radio not associated, and `wifi status` says so - a half-joined state is never reported as
joined.

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

## 6. Where the passphrase goes

The short version, because this is the command surface and `docs/wifi.md` §6 carries the argument:

- The shell prompts with invisible entry and hands the secret **to the keyring service**, which is the
  only holder. It does not pass through this utility's output, is never logged, and is never echoed.
- `wifi connect` then asks the radio to join using a **capability to that credential**, not the bytes.
- On a machine with a filesystem the credential is stored **capability-protected, and the boot log
  says plainly that it is not encrypted at rest**. On a diskless machine it is a session credential
  that dies with the keyring, and the failure when it does is "retype it", loudly.
- `wifi forget` revokes it. Every outstanding capability to it goes stale by generation bump; nothing
  has to be hunted down.

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
which makes `connect` a built-in.

That is not a compromise so much as the right place anyway - the shell is where authority is decided
(CLAUDE.md Appendix D.4), and handing over a secret is exactly that decision. What the built-in must
not do is keep the secret: it reads it, sends it to the keyring, and drops it.

## 8. Tab completion (rule 9)

`wifi` will complete from the command table; its subcommands from `SUBCMD_FIRST` in
`services/shell/src/main.rs`, which is also what `doc_command_check.py` reads - so this document
cannot come to show a subcommand the shell does not offer. `radio` completes one level further, to
`on` and `off`. An SSID argument completes from `wifi stored`, never from the last scan, because
completing from a scan would leak the names of networks in range into a shell's history.

## 9. Not in scope

Following `docs/wifi.md` §9, and for the same reasons: no enterprise or 802.1X, no WPA3, no WEP ever,
no access-point mode, no manual channel selection, no hidden-SSID entry in the first version, and no
signal-strength monitor - `observe` is where live views live, and a second one here would be the
duplication rule 7 exists to prevent.
