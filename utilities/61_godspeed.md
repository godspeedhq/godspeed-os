<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `godspeed` - the native package manager

Implementation shape: **a foreground program for one command** (`services/godspeed-pm`), started by the
shell, handed the command line, read back as one record table, and ended by the shell before the next
prompt, as `ask` is. Nothing of it stays running and nothing restarts it. The design, and everything it
does not do yet, is `docs/developer-services-design.md` section 10.

The command typed is `godspeed`. `godspeed-pm` is only the program's name, because the standard library's
crate is already called `godspeed`; it appears in `status` while a command runs and nowhere else.

## Status, as built and honest (2026-10-16)

Built on `feat/tls`: the settings and the registry check. Searching for, fetching and keeping packages are
**not built**: they need the registry's signed metadata format (the CLI repository's `godspeed-package`
crate), and a machine that could fetch a package still could not run it (10.1). QEMU only; no board has
run it yet.

## 1. The command

```
godspeed config                       every setting, its value, and where it came from
godspeed config set <key> <value>     change one setting, checked first
godspeed check                        reach the registry over verified TLS, one row per step
godspeed help                         what each does
godspeed version
```

A bare `godspeed` prints its usage and fails (rule 1). Every command reads the settings file first, so an
edit takes effect at the next command; there is nothing to reload.

## 2. The settings

`/system/services/godspeed/config`, one `key value` per line, `#` starting a comment. A missing file is
written with every default on first start, so the file always shows what is in effect, and `godspeed
config` says of each value whether it came from the `file` or is the `default` because the key is absent.

| key | default | what it is |
|---|---|---|
| `registry` | `https://godspeed-registry.bankole-422.workers.dev` | where packages are read. `https://` only: the transport is TLS |
| `trust-root` | `sha256:d769...146d` | the registry's signing root, pinned. **Nothing reads it yet**: it is for the metadata check |
| `tls-roots` | `/system/services/godspeed/roots/` | the certificate authorities TLS accepts, one DER file each. On first start the folder is given ISRG Root X2, the root the registry's chain reaches; a folder that exists is never refilled, so a root removed stays removed |
| `timeout-secs` | 30 | how long one request may take, 1 to 100 |
| `max-package-mib` | 64 | the largest package that will be downloaded, 1 to 4096. Nothing downloads yet |

A file with a problem - an unknown key, a key set twice, a value that does not check - is never half used:
every command names the line and refuses until it is fixed.

`godspeed config set` checks the value before writing (a URL that parses as `https://`, a folder ending in
`/`, a number in range), keeps every other line of the file as it was, and refuses a change that would
leave the file unreadable. `trust-root` is **not** set here: it decides which registry is trusted, so it is
changed in the file, by hand, on purpose. `edit` works on the file too.

## 3. `godspeed check`

One row per step, stopping at the first that fails and saying why:

| item | value | note |
|---|---|---|
| `registry` | the address | `file` or `default` |
| `clock` | the date a certificate is checked against | its source: hardware clock, network time, or a floor from the last boot. No clock, no check: `date sync` first |
| `roots` | how many | the folder they were read from |
| `address` | the registry's IPv4 address | its name |
| `tls` | `verified` | TLS 1.3; the certificate chain reaches a trusted root and names the server |
| `http` | the status line | how many bytes came back |
| `says` | the first line of the registry's answer | |

A failure names the step and the reason in words a person can act on: an expired certificate (or a clock
that is ahead), one not valid yet (or a clock behind), a chain that reaches no trusted root, a certificate
for another name, no hardware random source, no network. TLS is never skipped: there is no `insecure` mode.

## 4. Pipes

`godspeed config` and `godspeed check` are reports, so they pipe as records (`item`, `value`, `note`), with
a last row whose item is `status` and whose value is `ok` or `failed`:

```
godspeed config | where note=file
godspeed check | where item=tls
```

`godspeed config set` is an action and refuses to start a pipe, with the sentence naming the reports.

## 5. What it holds

Its supervisor row grants exactly four peers - the shell for its answer, `fs` for its settings and roots,
`net-stack` for the registry's connection, `time` for the date - and no privilege of any kind. Pinned in
`COMMANDMENTS.baseline.toml`. The TLS client is `sdk/tls`; the connection is `net-stack`'s op 23, a TCP
connection held as a capability (`gs::net::Net::connect`).
