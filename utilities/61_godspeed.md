<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `godspeed` - the native package manager

Implementation shape: **a foreground program for one command** (`services/godspeed-pm`), started by the
shell, handed the command line, read back as one record table, and ended by the shell before the next
prompt, as `ask` is. Nothing of it stays running and nothing restarts it. The design, and everything it
does not do yet, is `docs/developer-services-design.md` section 10.

The command typed is `godspeed`. `godspeed-pm` is only the program's name, because the standard library's
crate is already called `godspeed`; it appears in `status` while a command runs and nowhere else.

## Status, as built and honest (2026-10-16)

Built on `feat/tls`: the registries, the settings and the registry check. Searching for, fetching and
keeping packages are **not built**: they need the registry's signed metadata format (the CLI repository's
`godspeed-package` crate), and a machine that could fetch a package still could not run it (10.1).
`osdev test godspeed` runs every command against a real disk and reaches the real registry over verified
TLS through QEMU's user network (its registry checks are skipped, not passed, when the machine running the
test is offline). On hardware: `godspeed check` verified the registry and got `HTTP/1.1 200 OK` on all five
boards - the HP T630 and the Dell Wyse 5070 on 2026-10-16, and the Raspberry Pi 2, the Raspberry Pi 4 and
the VisionFive 2 Lite on 2026-10-17, each with its own hardware random source (RDRAND on the PCs, the Pis'
RNG and RNG200, the JH7110's TRNG).

## 1. The command

```
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

## 5. Pipes

`godspeed registry`, `godspeed config` and `godspeed check` are reports, so they pipe as records (`item`,
`value`, `note`), with a last row whose item is `status` and whose value is `ok` or `failed`:

```
godspeed registry | where note~in use
godspeed config | where note=file
godspeed check | where item=tls
```

`godspeed config set` and `godspeed registry add`, `use` and `remove` are actions and refuse to start a
pipe, with the sentence naming the reports.

## 6. What it holds

Its supervisor row grants exactly four peers - the shell for its answer, `fs` for its settings and roots,
`net-stack` for the registry's connection, `time` for the date - and no privilege of any kind. Pinned in
`COMMANDMENTS.baseline.toml`. The TLS client is `sdk/tls`; the connection is `net-stack`'s op 23, a TCP
connection held as a capability (`gs::net::Net::connect`). The `[y/N]` before a root is trusted is asked by
the shell, which holds the console; the package manager is told only what to add.
