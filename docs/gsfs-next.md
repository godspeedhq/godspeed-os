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

## 2. Phase N - append-only and sealed file capabilities

**No on-disk format change** - this is a rights question, and rights live in the capability, not on
the disk.

A file capability today carries `READ` and `WRITE`. Two additions:

- **`APPEND`** - a holder may extend a file and may not modify or truncate what is already there. A
  write is admitted only at `offset == size`.
- **`SEALED`** - a file whose content can never change again, enforced by refusing to mint any
  writable capability to it.

Both are exactly the shape §7.3 and §7.4 already describe: rights narrow on transfer and never widen,
and `fs` enforces `op <= right` under the badge the kernel validated. Nothing new is asked of the
kernel: it already routes a badged invocation carrying `(resource_id, right)`.

**`recorder` is the waiting consumer.** It streams a capture file and today must hold full `WRITE`,
so a compromised or confused recorder can rewrite history it should only be able to extend. With
`APPEND` it cannot, and a log becomes tamper-evident by construction rather than by trust.

Verified by extending `osdev test file-cap`, which already exercises non-escalation at both the kernel
and `fs` layers.

---

## 3. Phase O - timestamps (GSFS0008 -> 0009)

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

## 4. Phase P - `ls`, made fully featured

Timestamps exist to be seen. `ls` today lists names; this makes it a tool.

| command | what it shows |
| --- | --- |
| `ls` | names, as now - the default stays terse |
| `ls long` | type, size, mtime, one entry per line |
| `ls all` | include entries the terse form elides |
| `ls sort name\|size\|time` | explicit, because a default sort order that changes is a lie |
| `ls rev` | reverse the sort |
| `ls human` | sizes as KiB/MiB rather than bytes |

**Words, not flags** (`utilities/0_conventions.md` rule 4), so `ls long time` rather than `ls -lt`.
Every subcommand tab-completes (rule 9) and `ls` stays in the path-completing set, since its argument
IS a path. The wait stays `q`-escapable (rule 10) - it already goes through `fs_request_q`.

Sizes column-align with `font-variant-numeric`'s terminal equivalent: pad to a fixed width so digits
line up, because a column of right-ragged numbers is not a column.

---

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
