# 33. A directory listing stops at one block, and says nothing

**Status: CLOSED. The cursor is built; a directory of any size is listed in full.**

The two halves of this entry closed separately, and the gap between them is the lesson:

1. **The silence** closed first - a truncated listing said so, loudly. That converted a silent wrong
   answer into a loud partial one, which is better and is still not an answer.
2. **The ceiling** is closed now. `LIST_DIR` carries a resume position in both directions: the
   request says which entry to start at, the reply says where to continue. A caller loops until the
   directory is exhausted. Verified by `osdev test files` (45 files in one directory: listed,
   searched, copied and piped in full) and `osdev test fs-fuzz` (30 files, first and last entry both
   present, no truncation banner).

**What the fix actually cost, because the headline underplays it.** The reply format gained four
bytes and `fs` gained a resume argument - that part is small. The cost was in the EIGHT callers, each
of which had its own hand-written copy of the reply parse opening with a bare `let mut i = 3usize`.
They now share one `DirCursor`, and the header size is a named constant in both crates rather than a
literal in ten places. Three of those callers - `find`, `tree`, `copy` - had `continue` statements
that targeted the directory-walk loop; wrapping each in a page loop silently re-pointed them at the
new inner loop, which would have re-asked the same directory forever. They are `break 'pages` now,
labelled so the target is stated rather than inferred.

**What a multi-page walk guarantees** is written down at `fs`'s `list_dir` rather than assumed: an
entry present and unmoved for the whole walk is returned exactly once (records never move between
slots - `delete` marks in place and does not compact); an entry deleted mid-walk appears only if its
slot was already passed; an entry created mid-walk can be missed entirely, because it may land in a
freed slot behind the cursor. That is what POSIX `readdir` gives, and it is the honest limit: a
snapshot would need either a lock held across client round trips - letting a dead client wedge the
filesystem - or a copy of the directory, which is unbounded.

---

*What follows is the entry as written when it was open.*

Found by a code comment citing `backlog/33` when no such file existed. `scripts/doc_refs.py` checks
documents, not code comments, so a dangling reference in a `///` block is caught by nothing.

## What happens

`Fs::list_dir` builds its reply into a single `[u8; BLOCK]` - 512 bytes - and stops adding entries
when the next one will not fit:

```rust
if w + 1 + nl + 1 + 8 + 4 + 1 > BLOCK { break; }
```

Per entry the wire cost is `name_len(1) + name + is_dir(1) + size(8) + mtime(4) + flags(1)`, so
`name + 15`. With the two header bytes that gives roughly:

| average name | entries that fit |
|---|---|
| 4 characters | ~26 |
| 8 characters | ~22 |
| 12 characters | ~19 |
| 20 characters | ~14 |

**A directory with more entries than that listed incompletely, and `count` reported only what fit.**
So `dir` on a directory of thirty files printed twenty of them and said `(20 entries)` as though that
were the whole truth. There was no marker, no warning, and nothing in the reply a client could have
used to tell the difference.

## Why this is worse than a limit

A ceiling is a limitation. **This was a wrong answer.** The operator asks what is in a directory and
is told something false, with no indication that it is false - which is the exact shape invariant 12
exists to forbid, and §26.7's "a failure must stay visible" one layer up.

It is not confined to `dir`, either. Every consumer of `OP_LIST_DIR` inherits it: `find`, `tree`,
`delete recursive`, tab completion, and the records pipe. A `find` that walks a truncated listing
silently fails to visit files that exist. `delete /x recursive` walks the same reply - it deletes
what it can see, reports success, and leaves the rest.

**And the free-bitmap rebuild walks the tree.** `drives check` recomputes free space from what it can
reach; entries it cannot see are entries whose blocks are not marked used. That is the leak-turning-
into-corruption path `22_move.md` already describes for a different cause.

## What is fixed now

The reply carries a **`truncated` flag**, and every text consumer says so:

```
/big  (20 entries, TRUNCATED - there are more than this listing can carry)
```

The flag rides in the reply's header rather than as a sentinel entry, so a consumer that does not
know about it is unaffected, and one that does cannot miss it. `find`, `tree` and `delete recursive`
report it too, because "I did not see everything" changes what their answers mean.

That converts a silent wrong answer into a loud partial one. It does NOT make the listing complete.

## What is still open: the ceiling itself

The fix is a **continuation cursor**: `OP_LIST_DIR` takes a starting position and returns the next
one, and a client loops until the position comes back empty. That is the standard answer (it is what
`getdents` does) and it is the right one here.

It is real work rather than a constant, which is why it is recorded rather than done:

- The cursor must be stable across a directory that is being MUTATED between calls. A position
  expressed as "block index, slot index" is simple and can skip or repeat an entry if something is
  deleted mid-walk. A position expressed as "after this name" is stable but needs an ordering the
  format does not currently define (entries live wherever there was a free slot).
- Every consumer becomes a loop. `find` and `tree` already recurse; they gain an inner loop each.
- It interacts with the request/reply pattern: a multi-call walk is no longer one atomic question,
  so a client must tolerate the directory changing under it, and say what its answer means when it
  does.

**Related, and cheaper than it looks:** moving to 2 KiB blocks (see the note below) raises the
ceiling from ~20 entries to ~130 without any protocol change. That does not remove the need for a
cursor - correctness cannot rest on a directory being small enough - but it changes how often anyone
meets the limit while the cursor is being designed.

## Why 2 KiB and not ext4's 4 KiB

Worth writing down because the obvious borrow does not fit. `MAX_PAYLOAD` is 4096 bytes (§8.5, one
page), and a block travels to `block-driver` inside a message carrying a correlation tag and a status
byte. A 4 KiB block reply is **4,098 bytes - two over the limit.** Raising the ceiling is a
constitutional change and grows every endpoint queue (16 deep), which is far too much to spend here.

2 KiB fits with room to spare, and gives four times fewer block reads, CRC checks and IPC round trips
per directory scan. Under §6.15 it is an `incompat` feature bit: 512-byte volumes keep working, and a
build that predates it refuses to mount a 2 KiB volume rather than misreading it.

## The other lesson: a dangling reference in a code comment is caught by nothing

`doc_refs.py` scans `docs/`. This entry was cited from `services/fs/src/main.rs` and did not exist,
and the citation read as though the limitation had been recorded - which is the specific failure mode
§26.7 warns about, since a recorded limitation is one the next person can plan around and an
unrecorded one is discovered by whoever trusts the document. Extending the checker to code comments
is a small change and is worth doing.
