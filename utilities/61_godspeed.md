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
info, fetch, list and remove**, every answer verified on this machine against the registry's signed
metadata by the shared package crate's own checks (the CLI repository's `godspeed-package`). And (the same day)
**install, upgrade and uninstall** for a SERVICE: the supervisor checks the fetched copy from the disk again and
starts it, now and at every boot, with the send peers the operator approved and no privilege (section 4b; design 12).
And a `utility` package - a program that does one job and ends - installed the same way and run by its name
(section 4c). `osdev test godspeed` runs every
command against a real disk and the real registry over verified TLS through QEMU's user network, fetching
`godspeed/hello` (its registry checks are skipped, not passed, when the machine running the test is
offline). Packages are QEMU-verified only, x86; no hardware card yet. On hardware: `godspeed check` verified the registry and got `HTTP/1.1 200 OK` on all five
boards - the HP T630 and the Dell Wyse 5070 on 2026-10-16, and the Raspberry Pi 2, the Raspberry Pi 4 and
the VisionFive 2 Lite on 2026-10-17, each with its own hardware random source (RDRAND on the PCs, the Pis'
RNG and RNG200, the JH7110's TRNG).

## 1. The command

```
godspeed search <word> [from <n>]                  the packages the registry vouches for whose name holds the word, 40 a page
godspeed info <publisher/name>[@version]           its versions, then what it is: description, licence, release, what it ships and requests, who signed it
godspeed fetch <publisher/name>[@version]          verify a package end to end and fetch it here - fetched, not installed
godspeed fetch <publisher/name>[@version] offline  and write it here as one file, <name>-<version>.gsp, for a machine with no network
godspeed inspect <publisher/name>[@version]        every check installing makes, this machine's binary included - nothing kept
godspeed inspect <file>.gsp                        the same for a package file, with no network
godspeed list available [from <n>]                 every package the registry in use offers, 40 a page
godspeed list fetched                              the packages fetched here, with their sizes
godspeed list installed                            the packages installed, and whether each is running
godspeed list updates                              each fetched package against the registry it came from
godspeed verify [<publisher/name>[@version]]       check fetched copies again from the disk alone
godspeed remove <publisher/name>[@version]         delete a fetched copy, one version or every one
godspeed trust                                     what this machine has accepted from each registry, and the publishers' keys
godspeed trust reset <registry> [yes]              forget it, so the next command meets it again from root 1; asks [y/N]
godspeed trust add <publisher> <key> [yes]         trust a key yourself, for an unpublished package file; asks [y/N]
godspeed trust remove <publisher> <key>            stop trusting it: what it signed no longer starts
godspeed install <publisher/name>[,...] [yes]      start fetched packages, now and at every boot; asks [y/N] for each
godspeed install <publisher/name> grant <p>,...    the same with the peers named, and no question
godspeed install <file>.gsp [yes] [accept-old]     a package file, with no network (4d)
godspeed plan <publisher/name>[@version]           what installing it takes: what it requires, in the order to install (4f)
godspeed resolve <publisher/name> <range>          the newest release on the registry in a range, such as ^1.2.0 (4f)
godspeed upgrade <publisher/name>[,...] | all [yes] fetch the newest release of what is installed and install it; asks
godspeed uninstall <publisher/name>[,...] [yes]    stop them, and no longer start them at boot; asks [y/N] for each
godspeed uninstall <publisher/name>[,...] purge [yes]  and delete every fetched version too; asks [y/N], naming both
godspeed cleanup [yes]                             delete every fetched copy nothing uses, and interrupted fetches; asks [y/N]
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
| `timeout-secs` | 30 | how long a request may go with nothing moving on its connection, 1 to 100: a STALL, not a total, so a large download that keeps arriving is never cut off. Looking the name up and connecting keep their own limits, 10 s and 20 s, so a smaller value does not shorten those |
| `max-package-mib` | 64 | the largest package that will be downloaded, 1 to 4096 |
| `offline-max-age-days` | 30 | how old a package file's registry check may be before installing it asks, 0 to 3650 (4d) |

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

**`godspeed check <https-url>`** reads that one page instead of the registry, through every step above, to its
end, and throws it away: `page` replaces `registry`, `says` is left out, and a `speed` row says how fast it came,
`KiB/s` and the bytes and seconds, the handshake included. It is how this machine's download speed is measured
(`osdev test download`). The address may carry a query (`https://host/path?query`); it is still `https://` only.

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
| `search`, `list available` | a list, `package latest description`, a page of 40 from `from <n>` (default 0): each package's latest version and description, from its own index and manifest by the verified digests; the count, which are here, the command for the next page and the chain in the status note |
| `info` | a CARD (`key value note`): `title` (the release, the latest or not, when and by whom), `about` (its description), `licence`, `built for` (the release, beside this machine's), `ships` (its architectures, and whether this machine's is one), `requests`, `signed` (the key, shortened, and its standing), `versions`; the chain, through the contract, in the status note. Checked as `fetch` checks it |
| `fetch` | progress while it works (the chain's steps, then a bar as the binary downloads), then a CARD: `title` (fetched, or already fetched, and from where), `about` (the folder), `binary` (this machine's architecture and size), `built for`, `requests`, `signed`, `installed no`; the chain, through the binary, in the status note |
| `list fetched` | a list, `package version from size state`, `size` in bytes (a pipe can compare it; the console shows KiB); the count and total in the status note |
| `list installed` | none yet: the status note says why |
| `list updates` | a list, `package have latest status`: newer (with the command that fetches it), up to date, or no longer listed. Each is asked of the registry it came from, not the one in use |
| `verify` | a list, `package version status detail`, `status` `intact` or `DAMAGED`; the count in the status note. A damaged copy fails the command |
| `trust` | a CARD per registry: `title` (its name, in use or not), `root` (and its expiry), `timestamp`, `namespaces`, `pin` (and whether it is the configuration's), then a `key <publisher>/` row per key, whole, with its standing |
| `trust reset` | `forgotten`, or `nothing to forget` |
| `remove` | `removed` and the folder |

**`verify` works from the disk alone**: the signature over the manifest, the signing key against the one
recorded when it was fetched, the manifest against its folder, and the contract and the binary against the
manifest's digests and sizes. Whether the registry still vouches for the key needs the network; `list
updates` asks the registry, and a fresh `fetch` checks it again.

**How it looks**, in the style `dir` set: two spaces in, upper-case headers, a footer line. A LIST has
columns that name what it lists; a CARD, about one thing, has a title, a line about it, and the rest aligned
under it; failures and the settings (`config`, `registry`, `check`) are `item value note`. Piped, every
answer is the same records, the column names lower-case.

**A list is its entries and nothing else**, so `count` counts them; what a list's rows do not say about
themselves (the chain it was verified by, why it is empty) is the status row's note, which the console prints
under the table. **An empty list is not a failure**: the console says `0 packages`, the status is `ok`, and
piped, only the status row is left (`where item!=status | count` is then 0). A bare `godspeed search` and a
bare `godspeed list` are usage errors: each says what to add.

**Where a package is fetched to**: `/system/packages/<publisher>/<name>/<version>/`, holding `<arch>.elf` (this
machine's only), `manifest.toml`, `manifest.sig`, `contract.toml` and `origin` (the registry, when, the
architecture and the signing key). A fetch is written beside it as `<version>.partial` and renamed into place
only once every file is verified, so a half-fetched package is never listed. Fetching a version already
fetched leaves it as it is.

**A pin that changed** since this machine first reached a registry is refused - changing it is trusting
another registry - until the operator runs `godspeed trust reset <registry>` (which asks `[y/N]`; it removes
`/system/services/godspeed/trust/<registry>/`), after which the registry is met again from root 1. The
record also keeps the last namespaces document accepted, which is what `godspeed trust` lists the keys from.

**The snapshot is kept on the disk, not in memory**, so the registry's size does not decide whether a command
runs. It names every package (a line each), so it grows with the registry: it is streamed to
`/system/services/godspeed/trust/<registry>/snapshot.toml` as it arrives, sized by its `Content-Length`, hashed on
the way and verified from the digest (a signature covers the digest, not the bytes), each line checked as the
whole document would be, and only then put in place of the one kept. A command whose timestamp names the
snapshot already kept does not download it again - the chain note says `(kept)`. Lookups and listings read it
back a line at a time. A snapshot over 256 MiB (about two and a half million packages), or one that does not
state its size, is refused by name. What is not solved here: a NEW snapshot is still downloaded whole, once per
version, which at a large registry is a lot of bytes - the registry format's to fix (the CLI
repository's backlog, item 2).

**Limits, refused by name, never cut**: an index 8 KiB (about 40 releases), a manifest or a contract 4 KiB, a
binary `max-package-mib`, a snapshot line 320 bytes. They are on the stack, which is 256 KiB for every service and
already carries the TLS session.

**One connection** carries a command's requests, kept alive; a server that closes it part way is asked
again on a fresh one, up to three times, and a command writes its rows only after its last request, so
nothing is said twice.

## 4b. Installing

`godspeed install godspeed/hello` asks one question per package, with everything the package would be given in it:

```
godspeed/hello 0.1.0 asks for nothing (no peers, no privilege) and starts at every boot. Install it? [y/N]:
bankole/notes 0.2.0 asks for: fs, events (no privilege) and starts at every boot. Install it and grant them? [y/N]:
```

The peers are the send peers its contract requests, read from the fetched copy after it is checked from the disk.
`yes` as the last word answers every question, and approves only what the questions name; `grant <peer>,...` (or
`grant none`) gives some of them, for one package, with no question. A comma list installs several, each asked on
its own; one that fails does not stop the rest, and the command fails if any did.

**What happens on yes.** `godspeed-pm` writes the answer to `grant` in the package's folder, adds the package to
`/system/packages/installed`, and asks the supervisor to start it. The supervisor checks the fetched copy again
from the disk, by the same code `godspeed verify` runs, narrows the grant to what the signed contract requests, and
starts the bytes it checked, under the task name `publisher/name`. A start that fails takes the package off the list
again, so the list holds only what runs. At every boot the supervisor starts what the list names, checked the same
way; a package that dies is restarted from the disk up to 5 times a boot and then left down, with a line that says so.

**What it is given:** the approved peers, never `supervisor`, an endpoint if its contract declares `ipc_receive`, its
contract's memory limit up to 64 MiB, and **no privilege**, which is not asked and cannot be granted.

`godspeed upgrade <pkg>` (or `all`) looks up the newest release of each installed package on the registry it came
from, fetches and verifies it, and installs it in the old one's place with the same question, now naming the
version change: a new release may ask for more. One already newest is said to be up to date. `godspeed uninstall`
asks, stops it and takes it off the list; its fetched copy stays, and `godspeed remove` refuses an installed copy.

`list installed` is `package version state`, `state` being what the supervisor says: `running`, `NOT running`,
or for a utility, how to run it.

## 4c. Utilities

A package of kind `utility` (`godspeed new utility` in the CLI) is installed exactly as a service is - fetched,
checked, asked about once - but it is NOT started at install or at boot. It runs when its name is typed:

```
gsh> hello-utility Ada
hello, Ada
gsh> hello-utility Ada | match Ada | count
1
```

The shell asks the supervisor to start it, wired to send to the shell and to the peers its grant names; the
supervisor checks the copy from the disk again first, as it does for every start. The rest of the command line
is handed to it as its arguments, and what it writes is shown, or is the stream of a pipe, as any producer's is.
It ends itself with a status (`gs::program`, the `Exit` syscall); the shell says the status when it is not 0,
and the command fails then. A utility is never restarted and runs one at a time. Its name is the package's own
name (`hello-utility`), or the full `publisher/name` when two publishers' utilities share one, which is refused
rather than guessed between. A built-in command of the same name always wins: an installed utility can never
replace `dir`.

## 4d. Package files: installing with no network

A package travels as one file, `<name>-<version>.gsp` (`.gsp` for GodspeedOS Package), made where there is a
network and installed where there is not (`docs/developer-services-design.md` 15):

```
gsh> godspeed fetch godspeed/hello offline          on a machine that can reach the registry
gsh> godspeed install hello-0.3.0.gsp               on one that cannot, after copying it there
```

The developer CLI makes the same file on a workstation (`godspeed fetch godspeed/hello --offline`).

**Trusted through the registry.** The file holds the documents a fetch reads, under the paths the registry
serves them at, and is checked by the same code from the root this machine's image pins. Nothing about what
is checked differs; only "now" does: a file is checked as of the last moment its timestamp was valid, and
records nothing of the registry's view on this machine - except that a NEWER record of the publishers' keys
wins, whichever side has it, so a key this machine knows was revoked refuses the file.

**Its age is the one thing asked.** A file whose check is older than `offline-max-age-days`, or any file while
the clock is not set (the Pis and the VisionFive have no clock chip), is asked about before anything is kept:

```
godspeed/hello 0.3.0 its registry check ran out on 2026-10-19, 59 days ago. A key revoked since then would
not show. Install it anyway? [y/N]
```

`yes` on the line does NOT answer this - a script that says `yes` never accepts the risk by accident - and
`accept-old` does. The acceptance is recorded in the copy's `origin` and shown on its card. A bad signature, a
digest that does not match or a key known revoked is refused, with no question.

**Unpublished packages.** `godspeed package --local` in the CLI makes a file nobody has published: no registry
vouches for its key, so this machine installs it only if its operator trusts that key for that publisher,
once - `godspeed trust add <publisher> <key>`, which asks and says what it means, and which the CLI prints. It
is listed as coming from `(operator)`. Nothing tells this machine if that key leaks: `godspeed trust remove`
withdraws it, and what it signed no longer starts, since the supervisor checks the list at every start.

`godspeed inspect <file>.gsp` checks either kind and keeps nothing.

## 4e. Cleaning up

`godspeed uninstall` keeps the fetched copy, so a reinstall needs no network; `godspeed uninstall <pkg> purge`
deletes it too. What is left over time - copies `uninstall` kept, packages fetched and never installed, older
versions after an upgrade, and what an interrupted fetch left - `godspeed cleanup` lists and, after `[y/N]`,
deletes (`yes` answers it; piped, it only lists). It cleans only the package manager's own store,
`/system/packages/`, and its scratch folder: never the installed version of anything, never a package file
(`.gsp`) wherever it is, since those are the operator's, and never the registries' trust or the settings.

## 4f. Dependencies: a package that needs another

A package may REQUIRE others - a service it talks to, a driver it needs running - in its signed manifest
(`docs/developer-services-design.md` 16). A requirement grants nothing: the peers are still asked about, one
question for all of them. A built-in service (`fs`, `net-stack`) is never required; it comes with the OS.

`godspeed install` of such a package works out what it needs first. A requirement already installed in range is
kept; one fetched in range is planned; one neither is fetched - the newest release in range, found by `godspeed
resolve` - and then planned too. Then it asks once:

```
gsh> godspeed install godspeed/dep-user
godspeed/dep-user 0.1.0 needs 1 more package:
  godspeed/dep-store           0.1.0     a service  asks for: nothing   (needed by godspeed/dep-user)
  godspeed/dep-user            0.1.0     a service  asks for: godspeed/dep-store
Install all 2 and grant them? (no privilege) [y/N]:
```

On yes they install in that order, the dependency first; one that does not start stops the rest, and what was
installed before it stays installed. `yes` on the command line answers it, as for one package.

**Refused, with nothing installed:** a requirement no release satisfies, or that no registry has; one installed
outside its range (`godspeed upgrade` it first); two that no one version of a package satisfies; packages that
require each other; a chain of more than 4 packages, the one asked for included, or more than 8 besides it; a requirement that is a
utility or an app, which runs to end rather than to be needed.

**Kept whole afterwards.** `uninstall` refuses a package another installed one requires, naming it. An install
or upgrade that would take a package outside an installed dependent's range is refused, naming both. At every
boot the supervisor starts installed packages in dependency order, whatever order the list is in, and one whose
requirement is not running is not started, with a line saying which.

`godspeed plan <pkg>` shows what an install would take without doing it: one row per package, in the order to
install them, columns `package version kind asks state` - `state` is `install, needed by <pkg>` (fetched and
checked) or `fetch, needed by <pkg>` (not here yet; its version column is the range, and kind and asks are `-`),
and the package asked for is last, `install`.

## 5. Pipes

`godspeed search`, `info`, `list available | fetched | installed | updates`, `verify`, `trust`, `registry`, `config` and `check` are reports, so they pipe as records
(`item`, `value`, `note`), with a last row whose item is `status` and whose value is `ok` or `failed`:

```
godspeed search hello | where package=godspeed/hello
godspeed list available | where package=godspeed/hello | count
godspeed list fetched | where size>1000000
godspeed registry | where item=default
godspeed config | where note=file
godspeed check | where item=tls
```

`godspeed fetch`, `remove`, `install`, `uninstall` and `trust reset`, `godspeed config set` and `godspeed registry add`, `use` and
`remove` are actions and refuse to start a pipe, with the sentence naming the reports.

## 6. What it holds

Its supervisor row grants exactly four peers - the shell for its answer, `fs` for its settings, roots,
trust record and fetched packages, `net-stack` for the registry's connection, `time` for the date - and no
privilege of any kind. Packages added no peer and no privilege. Pinned in
`COMMANDMENTS.baseline.toml`. The TLS client is `sdk/tls`; the connection is `net-stack`'s op 23, a TCP
connection held as a capability (`gs::net::Net::connect`). The `[y/N]` before a root is trusted is asked by
the shell, which holds the console; the package manager is told only what to add.
