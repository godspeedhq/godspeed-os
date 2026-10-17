<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `godspeed` - the native package manager

Implementation shape: **a foreground program for one command** (`services/godspeed-pm`), started by the
shell, handed the command line, read back as one record table, and ended by the shell before the next
prompt, as `ask` is. Nothing of it stays running and nothing restarts it. The design, and everything it
does not do yet, is `docs/developer-services-design.md` section 10.

The command typed is `godspeed`. `godspeed-pm` is only the program's name, because the standard library's
crate is already called `godspeed`; it appears in `status` while a command runs and nowhere else.

## Status, as built and honest (2026-10-17)

Built: the registries, the settings, the registry check, and (2026-10-17, branch `feat/packages`) **search,
info, fetch, list and uninstall**, every answer verified on this machine against the registry's signed
metadata by the shared package crate's own checks (the CLI repository's `godspeed-package`). **Install is
not built**: a fetched package is KEPT, verified, under `/system/packages/`, and cannot run, because the
supervisor starts only the programs built into the image (design 9.2). `osdev test godspeed` runs every
command against a real disk and the real registry over verified TLS through QEMU's user network, fetching
`godspeed/hello` (its registry checks are skipped, not passed, when the machine running the test is
offline). Packages are QEMU-verified only, x86; no hardware card yet. On hardware: `godspeed check` verified the registry and got `HTTP/1.1 200 OK` on all five
boards - the HP T630 and the Dell Wyse 5070 on 2026-10-16, and the Raspberry Pi 2, the Raspberry Pi 4 and
the VisionFive 2 Lite on 2026-10-17, each with its own hardware random source (RDRAND on the PCs, the Pis'
RNG and RNG200, the JH7110's TRNG).

## 1. The command

```
godspeed search [<word>]                           the packages the registry vouches for, or those whose name holds the word
godspeed info <publisher/name>                     one package's published versions
godspeed fetch <publisher/name>[@version]          verify a package end to end and keep it here - kept, not installed
godspeed list                                      the packages kept here
godspeed uninstall <publisher/name>[@version]      remove a kept copy, one version or every one
godspeed install                                   not built: says why
godspeed registry                                  the registries this machine knows; one is in use
godspeed registry add <name> <url> <root> [yes]    know another; asks [y/N] before trusting its root
godspeed registry use <name>                       search and install from that one from now on
godspeed registry remove <name>                    forget one (not the one in use)
godspeed config [<key>]                            the plain settings, or one, and where each came from
godspeed config set <key> <value>                  change one setting, checked first
godspeed check                                     reach the registry in use over verified TLS, one row per step
godspeed help                                      what each does
godspeed version
```

A bare `godspeed` prints its usage and fails (rule 1). Every command reads the settings file first, so an
edit takes effect at the next command; there is nothing to reload.

## 2. Registries

A machine knows up to eight registries, and **exactly one is in use**: the one searched, and the one a new
install comes from. There is no priority between registries because only one is ever asked, so a package
is never found in two places at once.

A registry is three things, kept together on one line of the file:

- a **name** - what `use` and `remove` call it: 1 to 32 lower-case letters, digits and hyphens;
- an **address** - `https://` only, because the transport is TLS;
- a **signing root** - `sha256:` and 64 hex digits, the root its metadata must be signed by. Each registry
  has its own, so switching registries switches which root is checked and never trusts one registry with
  another's root. **Nothing checks it yet**: it is for the metadata check, which is not built.

`godspeed registry add` shows the root and asks `[y/N]` before trusting it, because trusting a root is
trusting whoever holds its key. `yes` as the last word answers in advance, as for `seal` and `chaos
max-carnage`; the line naming the root still prints. A registry is added known, not in use, and the answer
says `godspeed registry use <name>` to switch. `remove` refuses the one in use: switch first, so something
is always in use.

**An installed package will remember the registry it came from**, and an update will fetch from there,
not from whichever registry is in use at the time. Otherwise switching registries would quietly change
where installed packages update from, and a package with the same name on another registry, signed by
another key, could replace one. If that registry has been removed, the update says so rather than trying
another. This is the rule installing will follow; nothing installs yet.

`godspeed registry` is one row per registry: `item` its name, `value` its address, `note` the root it is
signed by and, for the one in use, `in use`.

## 3. The settings

`/system/services/godspeed/config`, `#` starting a comment:

```
registry default https://godspeed-registry.bankole-422.workers.dev sha256:d769...146d
use default
tls-roots /system/services/godspeed/roots/
timeout-secs 30
max-package-mib 64
```

A missing file is written with these defaults on first start, so the file always shows what is in effect,
and `godspeed config` says of each plain setting whether it came from the `file` or is the `default`
because the key is absent.

| key | default | what it is |
|---|---|---|
| `tls-roots` | `/system/services/godspeed/roots/` | the certificate authorities TLS accepts, one DER file each. On first start the folder is given ISRG Root X2, the root the registry's chain reaches; a folder that exists is never refilled, so a root removed stays removed |
| `timeout-secs` | 30 | how long one request may take, 1 to 100, counted from its start. Looking the name up and connecting keep their own limits, 10 s and 20 s, so a smaller value does not shorten those |
| `max-package-mib` | 64 | the largest package that will be downloaded, 1 to 4096. Nothing downloads yet |

A registry is not a plain setting: `godspeed config set registry` is refused with the command that manages
registries, and so is `trust-root`.

A file with a problem - an unknown key, a key set twice, a value that does not check, a `use` naming no
registry, several registries and no `use` line - is never half used: every command names the line and
refuses until it is fixed. Which registry is in use is never guessed.

**A file written before registries had names** - `registry <url>`, and perhaps `trust-root <root>` on its
own line - reads as one registry called `default`, in use. The first registry command that changes the
file rewrites it in the form above; every other line is kept as it was.

`godspeed config set` checks the value before writing (a folder ending in `/`, a number in range), keeps
every other line of the file as it was, and refuses a change that would leave the file unreadable. `edit`
works on the file too.

## 4. `godspeed check`

One row per step, stopping at the first that fails and saying why:

| item | value | note |
|---|---|---|
| `registry` | the address of the registry in use | its name, and `(in use)` |
| `clock` | the date a certificate is checked against | its source: hardware clock, network time, or a floor from the last boot. No clock, no check: `date sync` first |
| `roots` | how many | the folder they were read from |
| `address` | the registry's IPv4 address | its name |
| `tls` | `verified` | TLS 1.3; the certificate chain reaches a trusted root and names the server |
| `http` | the status line | how many bytes came back |
| `says` | the first line of the registry's answer | |

A failure names the step and the reason in words a person can act on: an expired certificate (or a clock
that is ahead), one not valid yet (or a clock behind), a chain that reaches no trusted root, a certificate
for another name, no hardware random source, no network. TLS is never skipped: there is no `insecure` mode.

## 4a. Packages

**What is checked, in order** (`services/godspeed-pm/src/chain.rs` and `packages.rs`, the checks themselves
the shared crate's):

1. **The root.** On first contact, root 1 (the registry's trust/root/1.toml) must have the SHA-256 pinned on the registry's line
   in the settings, and be signed by its own root keys. Every root since must be signed by the one before it
   and by itself. What this machine reached is kept in `/system/services/godspeed/trust/<registry>/`: the
   root, the pin it was reached under, and the newest timestamp and namespaces versions seen.
2. **Timestamp, snapshot, namespaces**, each signed by its role's keys, unexpired, and not older than what
   this machine already accepted (a rollback is refused).
3. **The package's index**, exactly the bytes the snapshot names for it.
4. **The manifest**, by the digest the index names; **its signature**, by a key the signed namespaces vouch
   for the publisher at the release's publication time (a revoked key never; a retired one only for what was
   published before it retired).
5. **The contract and this machine's binary**, each by the digest and size the manifest names. The binary
   is streamed to disk and checked as it arrives; a file that does not match is deleted, never kept.

| command | rows |
|---|---|
| `search` | `verified` (the chain's versions, and whether this was first contact), then a `package` row per name |
| `info` | `verified`, then a `version` row per release: when, by whom, and which is the latest |
| `fetch` | `verified`, `package`, `signed` (the key, active or retired), `binary` (this machine's architecture and its size), `requests` (what the contract asks for; nothing is granted), `kept` (the folder), `installed no` |
| `list` | a row per kept `publisher/name@version`, with the registry it came from |
| `uninstall` | `removed` and the folder |

**Where a package is kept**: `/system/packages/<publisher>/<name>/<version>/`, holding `<arch>.elf` (this
machine's only), `manifest.toml`, `manifest.sig`, `contract.toml` and `origin` (the registry, when, the
architecture and the signing key). A fetch is written beside it as `<version>.partial` and renamed into place
only once every file is verified, so a half-fetched package is never listed. Fetching a version already kept
leaves it as it is.

**A pin that changed** since this machine first reached a registry is refused - changing it is trusting
another registry - until the operator removes `/system/services/godspeed/trust/<registry>/`, after which the
registry is met again from root 1.

**Limits, refused by name, never cut**: the snapshot 16 KiB (about 150 packages), an index 8 KiB (about 40
releases), a manifest or a contract 4 KiB, a binary `max-package-mib`. They are on the stack, which is
256 KiB for every service and already carries the TLS session.

**One connection** carries a command's requests, kept alive; a server that closes it part way is asked
again on a fresh one, up to three times, and a command writes its rows only after its last request, so
nothing is said twice.

## 5. Pipes

`godspeed search`, `info`, `list`, `registry`, `config` and `check` are reports, so they pipe as records
(`item`, `value`, `note`), with a last row whose item is `status` and whose value is `ok` or `failed`:

```
godspeed search | where item=package
godspeed search | where item=package | count
godspeed registry | where item=default
godspeed config | where note=file
godspeed check | where item=tls
```

`godspeed fetch`, `uninstall` and `install`, `godspeed config set` and `godspeed registry add`, `use` and
`remove` are actions and refuse to start a pipe, with the sentence naming the reports.

## 6. What it holds

Its supervisor row grants exactly four peers - the shell for its answer, `fs` for its settings, roots,
trust record and kept packages, `net-stack` for the registry's connection, `time` for the date - and no
privilege of any kind. Packages added no peer and no privilege. Pinned in
`COMMANDMENTS.baseline.toml`. The TLS client is `sdk/tls`; the connection is `net-stack`'s op 23, a TCP
connection held as a capability (`gs::net::Net::connect`). The `[y/N]` before a root is trusted is asked by
the shell, which holds the console; the package manager is told only what to add.
