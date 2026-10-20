# Utility: `download` - a file from a web server, saved here

**Status:** Built 2026-10-18, QEMU-verified (`osdev test download`). No hardware card yet.

    download <link>
    download <link> to <file>

Fetch one file over `https://` or plain `http://` and save it to this machine's disk. With no `to`, it is saved
in the current folder, named from the link's last part (`https://example.com/a/file.zip` becomes `file.zip`); a
link that ends in `/` is saved as `index.html`. `to <file>` names it instead, relative to the current folder or
absolute.

## What it checks, and what it says it did not

- **`https://`** is checked as everything on this machine is: TLS 1.3, the certificate chain to a root in
  `/system/services/godspeed/roots/` (the same roots `godspeed` uses - one trust store, not two), and the
  server's name. A link that fails a check is not downloaded, and the answer says which check failed.
- **`http://`** is taken, and the answer says `NOT checked`: nothing proves the bytes are the ones the server
  sent, and anyone between here and there could have changed them.
- **Redirects** are followed, up to five. A link that began `https://` and is redirected to `http://` is
  refused: a link that looked checked never quietly becomes one that is not. Typing the `http://` link yourself
  is the way to take it.

## The file

Written as `<file>.partial` and renamed only when its last byte is in, so a file at `<file>` is always a whole
one. A download that fails deletes its partial. One that is ended with `q` leaves it, and the next download to
the same file deletes it first. An existing file is never replaced: delete it, or name another with `to`.

## While it runs

The same progress as `godspeed fetch`: one line of words - the percentage, the size so far of the whole
(`2.0 / 9.5 MiB`), the speed so far (`KiB/s` or `MiB/s`, the units the sizes use), about how long is left
(`about 45 s left`, `2 min 5 s`, `1 h 3 min`; `done in ...` at the end) and `[q] quit` - and the bar on its own
line under them, as wide as the console. Nothing ends a download that keeps arriving: the bound is
`timeout-secs` (`godspeed config`) with NOTHING moving, never a total.

## Output

A card when it is done:

| key | value | note |
|---|---|---|
| `saved` | the file | its size |
| `from` | the link it came from | `as typed`, or how many redirects from the one typed |
| `checked` | `https`, or `NOT checked` | what that means |
| `speed` | KiB/s or MiB/s | the bytes and the time, the connection included |
| `time` | where the time went: `network`, `decrypt`, `disk` and `screen`, in ms | how many reads of `net-stack` it took, and how many found nothing yet and waited. `https://` only; the network figure includes the handshake |

## What it does not do, recorded rather than hidden

- **A server must say how large the file is** (`Content-Length`). `fs` sizes a file before it is written and has
  no append, so a page built as it is sent, with no length, is refused with that reason. Static files nearly
  always say.
- **Certificates must be ECDSA** (P-256 or P-384), which is all this machine's TLS checks. Many sites still use
  RSA certificates, and a download from one is refused, saying so. RSA is a change to the TLS library.
- **No resume and no retry yet.** A download that breaks starts again from the beginning when it is run again.
  A connection does not survive the machine's address changing - pulling the cable when the radio is on another
  network, for one - on any system; resuming is what will make that cheap, and it is the next step.
- **HTTP and HTTPS only:** no FTP and no other protocol (agreed 2026-10-18).
- **Not a pipe.** It is an action; its value is the file. `read` the file to pipe it.

## Where it runs

In the package manager's program (`services/godspeed-pm/src/download.rs`), which already holds what it needs and
no more: the network, the disk, the clock and the trusted roots. It touches nothing of the package store and is
given no authority the package manager did not already have. `wget` and `curl` answer `try download`.
