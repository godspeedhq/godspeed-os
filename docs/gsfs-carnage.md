# GSFS maximum carnage - the guarantees, written down, then attacked

**Status: scoped, not built. Section 2 (the outcome table) is the deliverable everything else
depends on, and it is built from the code as it stands, not from intent.**

The invariant this whole programme serves, stated first because every test below is an instance
of it:

> **A failed operation must never be reported as successful, and recovery must never silently turn
> known corruption into apparently valid data. Not every interrupted operation must be atomic, but
> its guarantees must be explicit and testable.**

That second sentence is the useful half. GSFS does not make every operation atomic and should not
pretend to: a streaming write of a 4 MB file is not one transaction and cannot be. What it must do
is say exactly which outcomes are permitted after an interruption, so that an outcome outside that
set is a bug rather than a debate.

## 1. What is already covered, so it is not rebuilt

Fourteen suites, ~11 minutes, all green (`osdev test fs-all`, `backlog/32`):

| suite | what it actually attacks |
|---|---|
| `fs-corrupt` | metadata damaged host-side: bad superblock CRC, bad directory CRC, bad magic. 14 checks |
| `fs-hostile` | genuinely malicious disks: a directory that contains itself, a name carrying `ESC [ 2J`, a `name_len` past the record |
| `fs-journal` | a committed-but-unfinished transaction is replayed; an invalid commit record is rejected |
| `fs-djournal` | the data-journal variant: a chunk commits atomically or not at all |
| `fs-ioretry` | block commands forced to FAIL through a real injection hook in the AHCI driver (`io-error-test`) |
| `fs-restart` | `fs` killed and respawned; the volume re-mounts and the data is there |
| `fs-time` | a full machine REBOOT, then the bytes and the dates are re-read from disk |
| `fs-fuzz` | 599 malformed protocol requests, every one answered rather than crashed |
| `fs-check` / `fs-scrub` | the bitmap rebuilt from the tree; a read-only CRC sweep that repairs nothing |
| `fs-compat` | unknown `compat` / `ro_compat` / `incompat` bits drive mount, mount-read-only, refuse |
| `fs-frag` / `fs-large` / `file-cap` | extent lists, multi-megabyte streaming, the capability surface |

**What none of them do is leave the disk in a state the running system did not intend.** Killing
`fs` leaves the image byte-identical to the instant before; the journal suites CONSTRUCT a
post-crash image host-side rather than interrupting a live write; the I/O injection fails a command
cleanly rather than half-performing it. So every crash tested so far is a crash we authored. That is
the gap.

## 2. The permitted-outcome table

This is what a reference model has to be told, and it has never been written down. Built by reading
the dispatch in `services/fs`, not by reading the design notes.

**The journal is a METADATA redo journal.** Structural blocks are staged into a 32 KiB region, a
commit record naming them is made durable, and only then does any home block move. So the unit of
atomicity is *the set of directory/bitmap/superblock blocks one operation touches* - and file DATA
is outside it unless the caller explicitly asks for the journaled variant.

| operation | journaled | permitted outcomes after an interruption |
|---|---|---|
| `write` (whole file) | yes | the old file complete, or the new file complete. Never a mix of the two extents, never an entry pointing at blocks that were never written |
| `write-new` (pre-allocate) | yes | the file does not exist, or it exists at its full declared size with undefined content |
| `write-at` (streaming chunk) | **no** | **any prefix of the chunks may have landed.** The metadata does not change, so the file's size and extent are unaffected; the CONTENT is a mix of old and new at block granularity |
| `write-at` journaled variant | yes | that one chunk landed entirely or not at all |
| `mkdir` / `mkdir -p` | yes | none of the directories exist, or all of them do |
| `rename` | yes | the old name, or the new name. Never both, never neither |
| `move` | yes | in the source directory, or in the destination. Never both, never neither |
| `delete` | yes | present with its blocks allocated, or absent with its blocks free. Never absent-and-allocated (a leak) or present-and-free (corruption) |
| `delete-tree` | **per entry** | **a PREFIX of the tree may be gone.** Each entry's own removal is atomic; the walk across them is not. This is the one operation whose partial outcome is a visible, intended state |
| `seal` | yes | sealed, or not sealed. The `ro_compat` bit and the flag commit together |
| `label` | yes | old label or new |

**And the detection boundary, which is the subtle one.** Every block carries a CRC, so:

- **A torn block** - the device wrote part of a 512-byte sector - **is DETECTED.** The CRC sits at
  the end of the block (@448 for a directory record region, @508 for data and for the times region),
  so a partial write fails it and the block is refused loudly on read.
- **A torn SEQUENCE - block 3 of a write landed and block 4 did not - is NOT detected, and cannot
  be.** Both blocks are individually valid; nothing records that they were meant to arrive together.
  For metadata that is what the journal is for. For file data it is the permitted outcome in the
  table above, and it is the honest statement of what a power cut costs.

This is consistent with what `CLAUDE.md` 6.1 already says about backend-conditional durability, and
it is the first time it has been said per operation.

## 3. The workstreams, in the order they should be done

### 3.1 Torn writes (the real gap)

The injection hook already exists and is already gated by a build feature: `services/block-driver`
can force the next N read/write commands to fail. What it cannot do is write the first N bytes and
then report failure, which is what a power cut looks like from above.

Two flavours, and they are not the same test:

- **Sub-sector tear** - write 256 of 512 bytes. Expect DETECTION on the next read: the block's CRC
  fails and the failure is loud. This is a test of the checksums, and it should pass today.
- **Inter-block tear** - write blocks 1 and 2 of a five-block sequence, then fail. Expect the
  operation's row in the table above, and nothing else. For a journaled op this is a test of the
  journal; for `write-at` it is a test that the *metadata* is untouched while the content is mixed.

**Do the injected version, not a SIGKILL of QEMU.** A SIGKILL is a more realistic power cut and a
worse oracle: it is not reproducible, so a failure cannot be bisected and a pass proves only that
this particular timing was survivable. The injected version is deterministic, can be swept across
every block index of an operation, and can be replayed exactly when it finds something. Keep the
SIGKILL form as a second opinion, run rarely, never as a gate.

### 3.2 Kill `fs` mid-operation

`fs-restart` kills at a quiescent point. The interesting kills are between the staged blocks and the
commit record, and between the commit record and the home-block writes - precisely the two windows
the journal exists for. Needs a way to ask `fs` to die at a named point; a build feature in the same
shape as `io-error-test` is the obvious route, since it keeps the hook out of the shipping binary.

Note what this does NOT test that 3.1 does: killing the service leaves the disk consistent with
every write that was issued. Only 3.1 can produce a disk that no sequence of completed writes could
have produced.

### 3.3 Cross-ISA interchange

Write an image under x86-64, then read AND MODIFY it under riscv64 and aarch64 in QEMU, and bring it
back. Every field is little-endian by construction, so the expectation is that it passes - and an
expectation is not a result. Cheap, completely untested, and the kind of thing that is discovered by
a user rather than by us if it is wrong.

### 3.4 The reference model

A few hundred lines HOST-side in `osdev`, where the suites already live: a plain Rust model of the
tree (names, sizes, bytes), a generator of random operation sequences, and a differential run that
applies each sequence to both and compares the whole tree afterwards. Host-side matters - the model
wants a `HashMap` and a `Vec`, and 26.6.1's no-heap rule is about what runs on the machine.

Then the same thing with failures injected, where the comparison is not equality but membership: the
result must be one of the outcomes section 2 permits for the operation that was interrupted. **This
is why section 2 is first.** Without it the model has no oracle and every difference is an argument.

### 3.5 Persistence by bytes, not by cache

Partly covered - `fs-time` already reboots the machine and re-reads - but not systematically, and
never with a flush boundary under test. Write, flush, shut down, remount, compare actual bytes. The
interesting variant is the one where the flush is REFUSED, which is a real backend (the Pi 2's stick
refuses `SYNCHRONIZE CACHE` outright; `CLAUDE.md` 6.1 records it): the guarantee narrows and `fs`
must say so rather than imply the wider one.

## 4. What QEMU can and cannot do here

It can do almost all of this, which is the point of doing it now. The injection is in our own driver,
the model is host-side, the reboots are real reboots, and the cross-ISA work is three QEMU targets we
already build for.

What it cannot do is a real power cut on a real device with a real write cache, and that is exactly
where the guarantee is backend-conditional anyway. The hardware pass answers a different question -
does THIS device honour the barrier - and it needs the five boards, not a test suite.

## 5. Not in scope

- Making `write-at` atomic by default. The journaled variant exists for callers that need it; making
  it the default would put every streaming byte through the journal region, which is 32 KiB.
- Repairing a torn sequence automatically. Detection is the guarantee; silent repair of data whose
  correct value is unknown is the second half of the invariant at the top of this file.
