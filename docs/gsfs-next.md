# GSFS: the next layer - adversarial proof, capability rights, and time

> **Scope for `feat/gsfs`, opened 2026-09-16.** Four pieces of work, ordered so that each is
> independently verifiable and the riskiest assumptions are tested before anything is built on top of
> them. **Everything here is verifiable in QEMU**, which is the constraint this plan was written
> under; the hardware pass is a confirmation at the end, not a dependency along the way.

---

## 0. Why this order

GSFS is already robust, and that is what shapes the plan. Phases A-K (`docs/persistence.md` §6.11)
delivered the metadata journal, the backup superblock, an fsck that rebuilds the bitmap from the tree,
extent lists, opt-in data journaling, bounded I/O retry with port recovery, and a read-only scrub.
Eleven QEMU suites cover them. Snapshots, copy-on-write and mirroring stay deferred: each strains the
30-minute whiteboard rule (§26.11) and no real need pulls them (§26.2).

So the gaps are not "more storage features". They are:

1. **`fs` parses untrusted input and has no adversarial suite.** Every other subsystem has one
   (§22 F1-F8, A1-A15). `fs` takes client-supplied paths, offsets, sizes and names over IPC, and
   `NAME_MAX` alone is validated in four separate places - the shape that drifts.
2. **A file capability carries READ/WRITE and nothing else.** There is no way to hand out a right to
   APPEND without also handing out the right to rewrite history.
3. **There are no timestamps.** A directory entry is `{itype, size, first_block, block_count}`.
   Nothing records when a file changed, so `ls` cannot say.

The fuzz suite goes first because it can only find things, never break what works - and because a
feature built on an unproven parser inherits its bugs.

---

## 1. Phase M - the adversarial suite (`osdev test fs-fuzz`)

**No on-disk format change. No production code change unless a test finds a defect.**

Three attack surfaces, all driven from QEMU:

### 1a. Hostile arguments through the real client path

Driven from the shell over serial, so it exercises exactly the path a user reaches:

| case | what it probes |
| --- | --- |
| a component longer than `NAME_MAX` (38), and exactly 38 | the four separate length checks agreeing |
| a path longer than the `plen` byte can describe (255+) | truncation being refused, not silently clamped |
| an empty path, and `/` itself | the root's `loc.is_none()` guards |
| `.` and `..` as components | **currently unhandled anywhere in `fs`** - safe only by accident |
| NUL, control bytes and invalid UTF-8 in a name | `valid_name` |
| 64+ nested components | `MAX_TREE_DEPTH` |
| `read_at` at offsets near `u64::MAX`, lengths near `u32::MAX` | arithmetic overflow in offset + len |
| `write_at` far beyond EOF | sparse-write behaviour, stated either way |
| opening more than `MAX_OPEN` (64) files | handle exhaustion is refused, not wrapped |
| deleting a non-empty directory | `delete` vs `delete_tree` |

### 1b. Structural fuzz of the on-disk format

Host-side mutation before boot, in the shape `fs-corrupt` and `fs-scrub` already use: flip bits in a
directory block, an extent block, the bitmap, the journal, a record's `first_block` and
`block_count`. **The bar is not that the filesystem survives with the data intact - it is that it
REFUSES LOUDLY and never panics, never hangs, and never serves a wrong answer as a right one.**

### 1c. Protocol fuzz

Malformed IPC directly at the `fs` endpoint: truncated payloads, an op byte no arm claims, a `plen`
that overruns the message, a badged file-cap invocation naming a resource `fs` never minted. The
existing `probe` service and `sdk/rust/src/adversarial.rs` are the sanctioned way to reach the raw
ABI (§18.1); this rides them rather than adding a second mechanism.

### Already found, before the suite exists

**`fs` does not enforce its own tree-acyclicity invariant. The only guard is in a client.**

`move_path` walks the source, walks the destination's parent, checks the destination does not
already exist, then `dir_add`s and `dir_remove`s. Nothing checks whether the destination lies UNDER
the source. Moving `/a` to `/a/b` would therefore write an entry inside `/a` whose `first_block` is
`/a`'s own, then detach `/a` from the root: a cycle, unreachable from the tree.

The damage that would do is worth stating, because it is not a tidy error. **`drives check` rebuilds
the free bitmap by walking the tree** (§6.11 Phase G). Blocks that are still occupied but no longer
reachable are marked FREE and handed to the next allocation, which overwrites live data.
`MAX_TREE_DEPTH` (64) keeps a walk from hanging, so it presents as a leak that becomes corruption
rather than as a wedge.

**It is not reachable from the prompt today, and the first version of this document wrongly said it
was.** `cmd_move` in the shell refuses both `dst == src` and `dst` beneath `src`
(`services/shell/src/main.rs`, "cannot move into itself"). That guard is real and it works.

**It is in the wrong place.** A check in the caller is a convention; only a check in the owner is an
enforcement. `fs` owns the tree and every invariant the tree has - the bitmap rebuild above depends
on acyclicity, and `fs` is what depends on it. Today that invariant holds because one client happens
to be careful, and nothing tells the next client - a script, another service, a refactor of this one -
that it is carrying an obligation it never agreed to. That is the shape invariant 1 exists to refuse:
authority and enforcement in different places.

So the fix is the same either way - an ancestor check in `move_path`, bounded by `MAX_TREE_DEPTH`
like every other walk - and the shell's guard stays, because catching it early gives a better message
than a service error. What changes is that `fs` stops depending on being asked nicely.

**How it gets tested.** Not from the shell, which correctly refuses to issue it. This is what §1c is
for: the protocol path, where a client can send exactly the request the shell declines to.

## 2. Phase N - append-only capabilities  (BUILT)

**No on-disk format change, and no kernel change.** Append-only is a property of the RESOURCE, which
is where §7.10 puts the meaning of a delegated capability: `fs` mints an ordinary WRITE capability,
the kernel validates it exactly as it validates any other, and `fs` records against that
`ResourceId` that writes may only move forward.

### Two things this section originally got wrong

Both were found by building it, and both are worth keeping because the reasoning that produced them
looked sound.

**1. There is no spare kernel right, and taking one is worse than having none.** The plan said "add
an `APPEND` right", in the shape of §7.4. The kernel's rights are fixed - bits 0 to 5 are READ,
WRITE, SEND, RECV, GRANT, REVOKE (`kernel/src/capability/rights.rs`) - so `1 << 2` is SEND. Minting
with it asks the kernel for something else entirely, and measured, every invoke failed including the
one that should have succeeded. The bit `OPEN_APPEND_ONLY` uses is masked off before the mint and
never reaches the kernel.

**2. "A write is admitted only at `offset == size`" cannot work, for two independent reasons.**
`write_at` demands BLOCK-ALIGNED offsets, so "exactly at the end" is usually not even expressible.
And a log is written into an extent allocated up front by `write_new`, so `size` is the FINAL size
from the first moment - an end-of-file test would refuse every write a log ever makes.

### What it actually is: a high-water mark

Each open resource remembers one byte past the furthest write made through it. An append-only holder
may not write below that mark, and the mark only ever moves forward - a refused or failed write does
not move it, so a rejection cannot lock a holder out of ground it never covered.

**What this guarantees, exactly:** within the life of one capability, a holder can never write below
anything it has already written. A log cannot be gone back over and edited.

**What it does NOT guarantee**, recorded so it is not over-read: the first write may land anywhere,
so it does not protect content that existed before the capability was minted; and closing and
re-opening starts a fresh mark. Both are bounded by who may call OPEN at all, which is a separate
authority.

**`recorder` is the waiting consumer.** It streams a capture file and today must hold full `WRITE` -
the authority to rewrite or truncate the very history it is recording - so a log's integrity rests
on the writer being well-behaved rather than on what it is able to do.

Verified by `osdev test file-cap` (13/0): a forward write is accepted, going back over written bytes
is DENIED, and a later forward write still works afterwards, which proves the refusal did not rewind
the mark.

### SEALED moved to Phase O  (BUILT there)

A file that can never be written again is a property of the FILE, not of a capability, so it has to
survive a reboot - which means on-disk state, which means the format work. It belongs with Phase O
and this section should not have claimed it needed no format change. See §3a.

## 3. Phase O - timestamps (GSFS0008 -> 0009)  (BUILT, except SEALED)

A directory entry has 64 bytes, of which the layout in use is: type @0, name_len @1, name @2..40,
size @40, first_block @48, block_count @56. **Bytes 40..48 hold the size and 56..64 the count, so the
free space is what the record does not yet name** - the phase begins by auditing the entry for
genuinely unused bytes rather than assuming any.

What is recorded, and why it is the minimum:

- **`mtime`** - when the content last changed. The one a person actually wants.
- **`ctime`** - when the record last changed (rename, move, size). Distinguishes "the file changed"
  from "the file was moved", which matters to a backup and to `drives check`.

**No `atime`.** Recording a read turns every read into a write, which on a journaled filesystem turns
every read into a transaction. The cost is real and the value is low; it is refused on purpose.

**The clock is now good enough for this, and was not before.** The `time` service owns the wall clock,
sets it from SNTP, and persists a floor across reboots (`/clock.last`). A file stamped on a machine
that has never seen the network gets the floor, not zero, and the floor only moves forward.

**Migration.** A 0008 volume mounts read-write with its timestamps reading as "unknown" rather than as
the epoch, because a wrong date is worse than an absent one. `drives check` stamps missing times with
the floor. Format bump is reformat-only, in the house pattern of 0005 -> 0008.

---

## 3a. SEALED - content frozen, permanently  (BUILT)

`seal <path>` freezes a file's bytes. It can still be read, listed, renamed, moved and deleted; it
can never be written again, and **there is no unseal** - a seal a holder can lift is a request
rather than a guarantee.

### Where the bit lives, and why every obvious place was wrong

The 64-byte record is full. Each candidate was examined and rejected on its failure mode, not on
taste:

| candidate | why not |
| --- | --- |
| a high bit of `name_len` | readers skip entries where `nl > NAME_MAX`, so a sealed file would VANISH from its own listing |
| a high bit of `itype` | `ITYPE_FILE \| 0x80` matches neither file nor directory - the entry becomes unclassifiable |
| growing the record to 128 bytes | halves how many entries a directory block holds, for one bit |

It rides **the top bit of the 64-bit `size`**, which is room no file can reach (2^63 bytes is eight
exabytes). Every size read goes through `rec_size`, which masks it off - and that is not tidiness:
`write_at` bounds a fragmented file's extent by its size, so the flag leaking into that arithmetic
would let a write run past the file's own blocks.

The cost is that a build which does not know this feature would display a nonsense size. So the
volume records **`FEAT_RO_COMPAT_SEALED`** the first time anything is sealed, and Phase L's policy
then makes such a build mount READ-ONLY: it can neither act on the wrong number nor change anything.
`ro_compat` rather than `incompat`, because refusing to mount a whole volume over one frozen file is
a punishment out of proportion, and read-only is the honest middle that policy exists to express.

### Enforced where it cannot be forgotten

The seal is carried on the `Entry`, and **every write path walks to an Entry**, so a write route
added later cannot forget to ask. `fs` also refuses to MINT a writable capability to a sealed file -
at `open`, not at each write, because a capability that looks writable and fails on use is a worse
answer than a plain refusal, and §7.3 says a right that cannot be honoured should not be granted.

### What it deliberately does not promise

- **Deletion still works.** A seal freezes content, not existence; deleting needs authority over the
  parent directory. An unremovable file is a way to fill a disk with rubbish nobody may clear - a
  denial of service bought with a guarantee nobody asked for.
- **Renaming and moving still work.** Archiving a sealed log is reasonable and changes no bytes.
- **It is not encryption.** A sealed file is as readable as any other.

### Surfaces

`ls long` shows `seal` in the TYPE column - a different kind of thing to have on a disk, not a
footnote beside `file`. In a PIPE, `ls` emits records, so it is a separate **`sealed` column** rather
than a new `type` value: making a sealed file's type read `seal` would silently drop it out of every
`where type=file` query anyone has already written.

`seal <path> yes` skips the `[y/N]` prompt, because a confirm reads the console and a script cannot
answer one. The warning still prints; `yes` buys automation, not silence.

Verified by `osdev test fs-time` (15/0): seal, refuse the write, **reboot**, refuse it again, and the
content is still the original.

## 4. Phase P - `ls`, made fully featured  (BUILT)

Timestamps exist to be seen. `ls` lists names by default, as it always did, and answers when and
how big on request.

| command | what it shows |
| --- | --- |
| `ls` | names, type, size - unchanged, and still the default |
| `ls long` | one entry per line with type, size and a MODIFIED column |
| `ls human` | sizes as KiB/MiB/GiB rather than raw bytes |

**Words, not flags** (`utilities/0_conventions.md` rule 4), in any order, mixable with a path:
`ls long human /projects` and `ls /projects human long` are the same command. Both orders complete
on Tab, which is why `ls` is in BOTH the leading and trailing subcommand tables - and a first-position
token matching no keyword falls through to path completion, so `ls /do<tab>` still works.

**The terse form stays the default, deliberately.** `ls` is read far more often than it is studied;
the common question is "what is in here", and a wall of columns answers one nobody asked.

### Two things this phase cost more than expected

- **Sizes did not line up, and the reason is a trap worth naming.** A Rust `Display` implementation
  SILENTLY IGNORES the width it is given unless it asks - `{:>10}` did nothing at all until the
  renderer was changed to build its text and call `f.pad`. A column of ragged numbers is not a
  column.
- **Widening the `LIST_DIR` reply by four bytes needed SIX consumers updated, and the first pass
  found three.** `files` went 222/0 to 203/19 and `selfcheck` failed 8 - each one a records pipe, a
  `find`, a `tree` or tab-completion stepping by the old stride and reading the next entry's name
  out of this one's timestamp. Every failure was caught by suites that were green beforehand, which
  is the entire argument for Phase M going first.

### `unknown` is an answer

`MODIFIED` reads `unknown` when the filesystem records no time for that entry - a 0008 file, or one
written before the machine knew the time. **Never 1970.** A date you can see is a date you will act
on, so a wrong one is worse than an absent one.

## 5. What this does NOT do

Recorded so it is not rediscovered as an omission (§26.7):

- **No snapshots, copy-on-write or RAID.** Still deferred, still for the reason §6.11 gives.
- **No `atime`.** Refused above, on cost.
- **No POSIX permission bits or ownership.** Authority here is a capability, not a mode bit on an
  inode; adding a second, weaker access-control mechanism beside the first would be exactly the
  "ambient authority" invariant 1 forbids. Access control stays in the rights the capability carries.
- **No symbolic or hard links.** Both create the possibility of a cycle in a tree whose acyclicity
  several invariants quietly assume - including the fsck that rebuilds the bitmap by walking it. A
  real need can pull them in later, with that assumption re-examined first.
- **The durability guarantee is untouched.** It stays backend-conditional exactly as §6.1 records: a
  backend that can be ordered is crash-recoverable, one that cannot is not, and QEMU cannot test the
  difference. Nothing here changes that, and nothing here should be read as having improved it.

---

## 6. How each phase is verified, all in QEMU

| phase | suite |
| --- | --- |
| M - adversarial | `osdev test fs-fuzz` (new), plus no regression across the eleven existing fs suites |
| N - rights | `osdev test file-cap` extended |
| O - timestamps | `osdev test fs-time` (new): stamp, survive a reboot, survive a restart, migrate a 0008 volume |
| P - `ls` | `osdev test shell` (the file section) and `selfcheck.gsh` |

A hardware pass on the five boards confirms at the end. It is not needed along the way, and this plan
is deliberately arranged so that it is not.
