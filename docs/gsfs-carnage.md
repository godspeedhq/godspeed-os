# GSFS maximum carnage - the guarantees, written down, then attacked

**Status, as of 2026-09-22.** `osdev test fs-all` runs **33 suites**, all passing together (~60 min, each in its own process). That sweep is the point: two of them were sitting RED with nothing watching, and BOTH were faults in the TEST rather than the filesystem - `fs-tear-detect` assumed a precondition instead of establishing it, and `fs-tear`'s probe did not recognise one of the two answers its own oracle calls legal. A suite that rots quietly is what `backlog/32` exists to prevent.
operations, 75 tear points, 35 exercising journal recovery), resource exhaustion (`fs-full` 14/0),
power cuts aimed and random (`fs-window` 8/0, `fs-churn` 8/0), and the **independent oracle**
(`fs-model`, §3.2), the block layer (§3.7 - `fs-blockchaos` for the completion stream,
`fs-blockdeath` for the driver dying mid-request; hot-unplug is not), and the duplicate-destructive-op
gap (§3.5, `fs-dupop`), and **cross-ISA** (§3.11, `cross_isa.py` 12/0 - one volume carried
x86-64 -> riscv64 -> x86-64). NOT RUN: the rest of §3.5 (two clients on one path) and the
remaining rows of §3.3.

**Mostly QEMU-validated.** The hardware results are recorded in §4 and §3.12. The one that matters
landed on 2026-09-18: a power cut during `churn` fell INSIDE the commit-to-checkpoint window on a
Dell Wyse, the next mount replayed four blocks, and content verification found no torn file - so
**journal recovery is proven on silicon**, on one board, on a backend that attests durability.

*(This line used to read "the rest are NOT RUN", which stopped being true the moment `fs-full` and
the two power-cut suites landed and was not updated. A status line is the first thing anybody reads
and the last thing anybody edits.)*

The mission, and every gate below is an instance of it:

> **Try to falsify GSFS's correctness, persistence, recovery and isolation guarantees. Do not treat a
> successful command, a passing unit test, or a surviving service as proof that data is correct.**
>
> **A failed operation must never be reported as successful, and recovery must never silently turn
> known corruption into apparently valid data. Not every interrupted operation must be atomic, but
> its guarantees must be explicit and testable.**

That last sentence is the load-bearing one. GSFS does not make every operation atomic and should not
pretend to: a streaming write of a 4 MB file is not one transaction and cannot be. What it must do is
say exactly which outcomes are permitted after an interruption, so that an outcome outside that set
is a bug rather than a debate. Section 2 is that statement, and it did not exist until this file.

## 1. What is already covered, so it is not rebuilt

Fifteen suites, all green in QEMU. `osdev test fs-all` runs every one of them; it was ~11 minutes
before `fs-tear` joined and roughly doubles with it, because `fs-tear` boots QEMU once per tear point:

| suite | what it actually attacks |
|---|---|
| `fs-tear` | Every prefix of an operation's writes, recorded from the real driver and booted. 9 operations, 220 tear points, plus a control proving the oracle can reject. Section 3.1 |
| `fs-rtear` | **NEW.** The SECOND-ORDER tear: cut the RECOVERY itself, at every sector the replay writes. Section 3.8 |
| `fs-reuse` | **NEW.** A file capability minted before an `fs` restart must reach nothing after it - including the file that now occupies its blocks. Section 3.6 |
| `fs-corrupt` | metadata damaged host-side: bad superblock CRC, bad directory CRC, bad magic. 14 checks |
| `fs-hostile` | genuinely malicious disks: a directory that contains itself, a name carrying `ESC [ 2J`, a `name_len` past the record |
| `fs-journal` | a committed-but-unfinished transaction is replayed; an invalid commit record is rejected |
| `fs-djournal` | the data-journal variant: a chunk commits atomically or not at all |
| `fs-ioretry` | block commands forced to FAIL through a real injection hook in the AHCI driver |
| `fs-restart` | `fs` killed and respawned; the volume re-mounts and the data is there |
| `fs-time` | a full machine REBOOT, then the bytes and the dates are re-read from disk |
| `fs-fuzz` | 599 malformed protocol requests, every one answered rather than crashed |
| `fs-check` / `fs-scrub` | the bitmap rebuilt from the tree; a read-only CRC sweep that repairs nothing |
| `fs-compat` | unknown `compat` / `ro_compat` / `incompat` bits drive mount, mount-read-only, refuse |
| `fs-frag` / `fs-large` / `file-cap` | extent lists, multi-megabyte streaming, the capability surface |

**Until `fs-tear`, none of them left the disk in a state the running system did not intend.** Killing
`fs` leaves the image byte-identical to the instant before; the journal suites CONSTRUCT a post-crash
image host-side; the I/O injection fails a command cleanly rather than half-performing it. Every
crash tested was a crash we authored. That is the gap `fs-tear` closes for one operation and leaves
open for the rest.

## 2. The permitted-outcome table

This is what a reference model has to be told. Built by reading the dispatch in `services/fs`, not
the design notes.

**The journal is a METADATA redo journal.** Structural blocks are staged into a 32 KiB region, a
commit record naming them is made durable, and only then does any home block move. So the unit of
atomicity is *the set of directory/bitmap/superblock blocks one operation touches* - and file DATA is
outside it unless the caller explicitly asks for the journaled variant.

| operation | journaled | permitted outcomes after an interruption |
|---|---|---|
| `write` (whole file) | yes | the old file complete, or the new file complete. Never a mix of the two extents, never an entry pointing at blocks that were never written |
| `write-new` (pre-allocate) | yes | the file does not exist, or it exists at its full declared size with undefined content |
| `write-at` (streaming chunk) | **no** | **any prefix of the chunks may have landed.** The metadata does not change, so size and extent are unaffected; the CONTENT is a mix of old and new at block granularity |
| `write-at` journaled variant | yes | that one chunk landed entirely or not at all |
| `mkdir` / `mkdir -p` | yes | none of the directories exist, or all of them do |
| `rename` | yes | the old name, or the new name. Never both, never neither |
| `move` | yes | in the source directory, or in the destination. Never both, never neither |
| `delete` | yes | present with its blocks allocated, absent with them free, or **absent with them still marked used (a LEAK)**. Never present-and-free |
| `delete-tree` | **per entry** | **a PREFIX of the tree may be gone.** Each entry's removal is atomic; the walk across them is not. The one operation whose partial outcome is a visible, intended state |
| `seal` | yes | sealed, or not sealed. The `ro_compat` bit and the flag commit together |
| `label` | yes | old label or new |

**The detection boundary**, which is the subtle part and the thing that makes "accepted / completed /
durable" three different words:

- **A torn block** - the device wrote part of a 512-byte sector - **is DETECTED.** The CRC sits at
  the end of the block (@448 for a directory's record region, @508 for data and for the times
  region), so a partial write fails it and the block is refused loudly on read.
- **A torn SEQUENCE - block 3 landed and block 4 did not - is NOT detected, and cannot be.** Both
  blocks are individually valid; nothing records that they were meant to arrive together. For
  metadata that is what the journal is for. For file data it is the permitted outcome above, and it
  is the honest cost of a power cut.
- **Acknowledged is not durable.** A write the driver acknowledged has reached the device, not
  necessarily the medium. `CLAUDE.md` 6.1 already records that this guarantee is backend-conditional
  and that one shipping backend (the Pi 2's USB stick) refuses `SYNCHRONIZE CACHE` outright.

## 3. The gates

Ordered so the unknown comes first rather than the easy.

### 3.1 Torn writes - BUILT, PASSES in QEMU (`osdev test fs-tear`)

**The first design here was an injector, and it was the wrong instrument.** The reasoning that
replaced it is the useful part. Extending the existing `io-error-test` hook to write part of a
transfer and then fail would work, but it tears where somebody CHOSE, it perturbs the path under
test, and it answers "did this tear survive" rather than "does any tear survive".

**Record and replay is strictly better and changes nothing about the I/O path.**

1. A `write-tap` build of `block-driver` logs every sector it writes - order, LBA, content - after
   the write has already succeeded. **A tap, not a valve.**
2. The harness boots a known image, runs ONE operation, and captures the log. It now holds the exact
   ordered sequence of writes, with content - which a before/after byte diff cannot give, because a
   block may be written twice in one operation (staged into the journal, then again at home).
3. For each `k`, it builds `A_k` = the pristine image with writes 1..=k applied in order. **`A_k` is
   not a model of a power cut. It is exactly the disk state one produces.**
4. Boot each `A_k`, mount, and check the outcome is in the permitted set - and nothing else.

**Results - nine operations, 220 tear points, every one inside the permitted set:**

| operation | tear points | outcome |
|---|---|---|
| overwrite a whole file | 22 | wholly `ORIGINAL` or wholly `NEWNEWNEW`, never a mix |
| rename | 15 | the old name or the new name, never both, never neither |
| move across directories | 17 | in the source or in the destination, never both, never neither |
| delete | 21 | gone, or still there, or gone-with-a-leak. Never a block marked free while a live file uses it |
| **label** | 17 | the old label or the new one. The smallest transaction there is - one superblock field - and everything else rests on that block being readable |
| **mkdir -p** | 27 | none of the three directories, or all of them. THREE entries in one commit, which is where a prefix first becomes possible: `rename` and `move` have two |
| **seal** | 19 | sealed or not sealed. The probe is a WRITE, because the flag is only visible in what the filesystem permits |
| **delete-tree** | 58 | a prefix of the tree may be gone; what may never happen is a block marked free while a survivor still references it |
| **write-new** | 24 | the extent is allocated BEFORE the entry exists, so a tear between the two would leave blocks held by nothing. None did |

**FIVE OF THESE WERE ADDED 2026-09-22, and two of the five are the ones that cross an allocation
boundary.** `delete-tree` frees blocks across a walk the journal does not make atomic, and
`write-new` allocates an extent before the directory entry that will own it. Both are the shape
`fs-metafull` found violated on the REFUSAL path the same day - a block stranded on every refused
create - and neither strands anything on the CRASH path, at any of their 82 cut points.

**Two rows of section 2 are not separately reachable, and are recorded rather than faked.**
`write-at` and its journaled variant have no shell verb that issues `OP_WRITE_AT` alone - `copy`
issues `write_new` and then a run of `write_at`, which is what the `write-new` case already cuts.
Section 2 also declares the unjournaled form's permitted outcome to be ANY prefix of the chunks with
metadata unchanged, so there is no exclusive pair to test; the invariant that does exist (size and
extent unaffected) is what `write-new`'s fsck oracle checks across its 24 points. Inventing a shell
verb to reach one opcode would be a test shaping the product rather than the reverse.

Each case is a row in a table rather than a copy of the harness: a setup whose last command writes
nothing (that is the boundary marker), the one operation to tear, a probe, and the two mutually
exclusive outcomes the permitted-outcome table allows. Evidence:
`build/tests/fs_tear_serial_<case>.log` holds each recording, written on every run and not only on
failure; a tear point outside the permitted set keeps its image at
`build/tests/fs_tear_<case>_k<N>.img` so it can be booted again while it is being fixed.

**And the oracle is proved able to REJECT**, because 54 passes are worth nothing until the thing
doing the judging has been seen to fail. A control boots the pristine disk and probes for the move
case, where neither permitted outcome can hold (there is no destination directory at all); the oracle
must reject it, and the suite fails if it does not. Getting the `move` oracle right took two attempts
for exactly this reason: the first version looked for the destination directory, which exists both
before and after the move, so it could never have distinguished them.

One trap recorded, because the obvious answer is wrong. An oracle marker must not be a substring of
its own opposite, nor present in both states. `zdir` failed the second test; probing INSIDE the
destination for `(empty)` versus the file name passes both.

The recording holds 522 sectors in all and the operation accounts for 22 of them, which looks wrong
until you know why: the suite builds `fs` with its `selftest` feature, and that self-test writes a
set of files at every boot. Those writes are real, they are in the replay, and a tear point inside
them is a legitimate state - they are simply not the operation under test. Named here so the ratio is
not mistaken for a boot that writes 400 sectors.

Where the boundary of the operation comes from is worth recording, because the obvious answer is
wrong: `fs` logs "request op N answered" only when a request FAILS, by design, so there is no log
line marking a successful write. The sweep is bounded instead by the tap high-water mark at the
moment the preceding (read-only) command finished - measured, not assumed, since the number of boot
writes is not a constant.

**Still to do here:** the remaining rows of section 2. `delete` is the next one worth having and
needs a different oracle from the three above - present-and-allocated versus absent-and-free is not a
question `dir` can answer, because a LEAK (absent but still allocated) looks identical to a clean
delete from the directory side. The check is the free accounting: `drives list` reports the
superblock's stored free count and `drives check` recomputes it by walking the tree, so a
disagreement between them within one boot is exactly the leak. Then `write-at` (whose permitted
outcome is a prefix rather than an exclusive pair, so the oracle shape changes), `mkdir -p`,
`delete-tree` and `seal`.

Also the sub-sector variant (`A_k` plus write `k+1` applied to only its first half), which should be
DETECTED by the CRC - the lower-value half, because `fs-corrupt` already probes that mechanism.

### The delete case: three wrong conclusions, then a real finding

Worth recording in full, because the answer was not visible from any amount of reasoning and each
wrong turn was corrected by going and looking.

Adding `delete` produced three consecutive FAILs - three reproducible tear points out of 21, in the
one operation whose failure mode is invisible in a listing. About as much as a result can do to look
like a real defect.

**Wrong conclusion 1: a timeout.** The capture held no `check:` line of any kind, neither the good
form nor the bad, which is the signature of no answer rather than a wrong one. So the oracle gained a
declared ANSWER marker - text that proves the probe replied at all, independent of what it replied -
and reports a TIMEOUT plainly instead of a verdict on the filesystem. That fix was right and is kept.
It did not change the result.

**Wrong conclusion 2: the filesystem is fine.** Booting the kept image by hand printed `nothing was
repaired` and `filesystem is consistent`. Two observations of the same disk disagreeing means one of
them is not seeing what it thinks, so the next step was the raw bytes rather than the summary:

```
check: the free count already agreed with the tree - nothing was repairedbtap 21 74 0 31373839...
```

The phrase is there with a `btap` line spliced into it and no newline between. **The write tap's own
serial volume - eight lines per sector - was triggering the kernel's known log splice, and the
oracle's phrase match broke wherever the splice landed inside the phrase.** The instrument was
corrupting the evidence it was gathering. `services/fs` states the rule that was broken, about its own
metrics: *"an observer that changes the thing it observes is not an observer."*

The fix is structural rather than a looser match: **two images.** A tapped one to RECORD with, where
the log volume is the entire point, and a plain one to REPLAY on, where it is pure noise. The replays
are considerably faster for it as well, and that is most of the suite's wall clock.

**And with the noise gone, a real finding underneath it:**

```
fs: check - free count DISAGREED with the tree: superblock said 32283 free, the tree says 32284
    (1 block(s) were held as used but are unreachable - a leak). Repaired.
```

**Wrong conclusion 3, and this one was mine from the start: that the leak is a defect.** It is not,
and the permitted-outcome table above was wrong to forbid it. The free count is a DERIVED VIEW of the
tree - §26.4's "stored, but not a second truth", reconciled when it drifts - `drives check` rebuilds
it from the tree, and `delete_tree` says outright that a crash mid-reclaim "only leaks blocks (nothing
references them) - never corruption". A leak costs space until the next fsck and costs nothing else.

The direction that is genuinely forbidden is the opposite: a block marked FREE while a live file still
references it. That one is silent and then fatal, because the next allocation hands the block out and
a write destroys data something still points at. So the oracle is now `Forbids("marked free but are IN
USE")`, which encodes the dangerous state directly rather than demanding perfect accounting.

**What this cost and what it bought.** It cost four runs of a twelve-minute suite and a hand-boot. It
bought: a harness that can tell a timeout from a verdict, an instrument that no longer perturbs its
own measurement, a corrected row in the outcome table, and the knowledge that a one-block leak is
reachable after a torn delete - which is now a documented property rather than a surprise waiting for
somebody with a power cut.

**And it is only knowable because fsck reports its repairs.** That change was made earlier the same
day for an unrelated reason - a silent repair is a §26.7 violation - and it is what made this visible
at all. Before it, `drives check` would have corrected the leak and said nothing, and this test could
not have been written.

**And not a SIGKILL of QEMU.** It is the more realistic power cut and the worse oracle: not
reproducible, so a failure cannot be bisected, and a pass proves only that one timing was survivable.
Worth an occasional second opinion; never the gate.

### 3.2 An independent oracle - BUILT, PASSES in QEMU (`osdev test fs-model`)

A small abstract model of files, directories, names and contents, host-side in `osdev` where the
suites already live and where a `BTreeMap` is allowed (26.6.1 governs what runs on the machine).
`osdev/src/fs_model.rs` is the model; `run_fs_model` drives both.

**It must not reuse GSFS allocation, traversal, rename or recovery logic.** A model that shares the
implementation reproduces its bugs and agrees with them, which is worse than no model because it
produces a green tick. This is the single most important constraint on this gate - so every rule in
the model is read off `utilities/*.md`, the page a PERSON is given, and nothing is derived from
`services/fs`.

**Two comparisons, and the second is the one that matters.**

1. Per operation, **Ok versus Err** - via `result`, the shell's own outcome channel, which prints
   exactly `Ok` or `Err(<name>)`. Deliberately NOT the error variant: which of the four a refusal
   picks is shell implementation detail, and a model that predicted it would be coupled to the thing
   it exists to be independent of.
2. At the end, **the whole volume**: every file's bytes read back, every directory's name set listed,
   both compared to the model exactly - then `drives check`, because a sequence can leave every name
   and byte correct and the free bitmap wrong.

**Result: 8 seeds, roughly 1,500 operations, no disagreement** - after the one it found, below. The
default seed is FIXED (`0x5EED0001`), because a suite that picks a new sequence every run is one
whose green means something different each time and whose red may not reproduce. Explore with
`osdev test fs-model:<seed>:<ops>`; a failure found by exploring becomes a second fixed entry.

#### What it found on its first run: a rule that lived only in the code

`seal` on an already-sealed file. The model predicted a refusal - a fair reading of "there is no
unseal" - and GSFS returned Ok, three times in one sequence.

**Neither was a bug.** `fs` carries `if e.sealed { return Ok(()); } // idempotent: already frozen`,
a deliberate decision with a comment on it. `utilities/50_seal.md` said nothing about the case at
all: not "already sealed", not "re-seal", not "idempotent". So the rule existed, was intentional,
and was unreachable by anybody who had not read that function.

That is precisely the class of defect this gate exists for and the reason it had to be written from
the SPEC. A test written against the implementation would have encoded Ok without noticing it had
never been documented. `50_seal.md` states the rule now, with the argument for it (seal asserts an
invariant rather than performing an event; an error should mean something went wrong; and an
interrupted seal must be able to finish rather than be refused).

#### What this gate does NOT cover, stated plainly

- **No interruption.** The comparison is equality. Under injected faults it must become MEMBERSHIP
  of the permitted set in §2 - which is why that table had to exist first, and is the obvious next
  step now that both halves exist.
- **Seven of the twelve operations.** Covered: `mkdir`, `write`, `delete`, `rename`, `seal`, `read`,
  `dir`. Not: `move`, streaming `write-at`, `delete-tree`, `mkdir -p`, `label`, `copy`.
- **Small contents and a small namespace.** Nine paths over two levels, and contents short enough to
  ride one IPC message - so the streaming path and the extent-list path are untouched here
  (`fs-large` and `fs-frag` cover those, against assertions about GSFS).
- **No concurrency**, which §3.5 explains is narrower than it sounds anyway.

### 3.3 Crash at every persistence boundary - PARTIAL (`fs-cache` 8/0, `fs-lyingflush` 7/0)

3.1 does this for three operations, exhaustively. The remaining work is the other operations and the
reordered/delayed/failed variants.

**BUILT: the volatile-write-cache model**, which was the item on this list that mattered most.

Every other power-cut suite cuts a QEMU disk that already holds every acknowledged write. That is
not how a drive behaves, and it is not what the guarantee is conditioned on: `CLAUDE.md` §6.1 makes
crash recovery explicitly **backend-conditional** - it holds where the device attests durability and
does not where the device will not honour a flush. So `fs-window` and `fs-churn` test the favourable
half only, and **could have been passing for a reason that evaporates on hardware.**

Two drives are modelled, and the difference between them is the whole point:

| suite | the drive | what must hold |
|---|---|---|
| `fs-cache` | honours the barrier: a write is acknowledged into guest RAM and reaches the medium at `OP_FLUSH` | the full guarantee - mounts, `0 bad`, no dangerous bitmap drift, barriered data intact |
| `fs-lyingflush` | **accepts the barrier and commits nothing** - the Pi 2's USB stick exactly, which refuses `SYNCHRONIZE CACHE` | only what §6.1 still promises: damage is DETECTED and named, never silently believed, and no live block is marked free |

The cache is guest RAM, so cutting the machine loses precisely what a real cache would lose with no
host-side cooperation. Reads are served from it, because a real drive cache does - without that,
`fs` would read back stale blocks it had just written and any damage would be an artefact of the
model rather than a property of the filesystem. Eviction writes through and SAYS SO: an injector
that quietly grows stronger than it claims is worse than none.

**What `fs-lyingflush` must NOT assert is `0 bad`.** §6.1 withholds recovery for that medium and says
a power loss may require a reformat; asserting the guarantee anyway would be the test contradicting
the constitution. What it asserts instead is the part that survives - metadata stays CRC-checked, so
damage is detected rather than believed, and the one unrecoverable drift (a live block marked free)
never happens.

**Honest limit: the `data_crc` refusal has not yet been made to fire.** `fs` recomputes a CRC over
the staged payload and refuses to apply a transaction the device did not durably write - the defence
added after this filesystem "has been destroyed repeatedly to prove it". Reaching it needs the cut to
land in a specific window, and both runs so far fell outside one (reported by the suite, not
assumed). The state is now REACHABLE, which it was not before; making it reliable is further work.

**Killing `fs` is a service-restart test, not a power-loss simulation.** Recorded here because it is
the mistake this whole programme exists to stop repeating: `fs-restart` is a good test of a different
thing.

### 3.4 Resource exhaustion - BUILT (`fs-full` 14/0, `fs-metafull` 9/0), and the second one FOUND A LEAK

The interesting question is not whether a write fails when the disk is full. It is what the failure
COSTS: whether the refusal is reported accurately, whether the blocks it half-claimed are handed
back, whether a file with nothing to do with it is still intact, and whether the filesystem accepts
valid work again afterwards. An allocator that strands a few blocks on every refusal turns a full
disk into a shrinking one, and nothing in a listing would ever show it.

The volume is baked to the edge HOST-SIDE - a canary plus three 10,600-block files on a 16 MiB disk -
rather than filled from the prompt, because filling 16 MiB a file at a time is thousands of commands
and copying megabytes inside QEMU spends the whole runtime on the least interesting part.

What it establishes, in order:

| | |
|---|---|
| the refusal happens | copying a 10,600-block file into a volume with a few hundred free is refused |
| **and names its reason** | `copy: failed - no space`, not a guess |
| a bystander is untouched | the canary still reads correctly after the failed allocation |
| **nothing leaked** | fsck still has nothing to repair - the refused claim was handed back in full |
| no corruption | 0 bad blocks throughout |
| the volume still works | delete a fill file, and both a small write and the large copy that was just refused succeed |

**The leak check is the one that matters**, and it only became possible because fsck reports its
repairs. A refused allocation that keeps what it reserved is invisible from every other angle: the
directory never referenced those blocks, so no listing, no read and no walk would show them. Only the
free accounting knows, and until this branch it corrected itself in silence.

**A note on how the test was sized, because the first version was wrong.** It originally asked for a
ONE-BLOCK write and the write succeeded - "a few hundred blocks free" turned out to be eight hundred,
which is ample for one block. A test that only fails when the arithmetic is exactly right is a test
of the arithmetic. Asking for 10,600 blocks against a few hundred cannot be rescued by a rounding
error, and the large claim also makes a leak obvious if the refusal strands what it reserved.

#### Exhaustion through DIRECTORY GROWTH - `fs-metafull` 9/0, and it found a real leak

**First, the half of this row that cannot be built, said plainly.** This paragraph used to list
"exhausting METADATA while data space remains (and the reverse)" as uncovered. In GSFS those are not
separate pools: `grow_dir` and `alloc_file` both call `alloc_run` against the one free bitmap, so
there is no metadata reserve to exhaust independently and testing for it would be theatre - the same
shape as 3.5's note that a single-threaded `fs` makes intra-operation interleaving unreachable.

**What IS reachable is the other allocation path, and nothing had touched it.** `fs-full` refuses one
large file: that is `alloc_file`, and the directory never grows during it. `fs-metafull` fills a
DIRECTORY instead, one tiny file at a time, so `grow_dir` runs repeatedly as the volume runs down -
and a directory growth that fails does so on the shared metadata every other entry depends on.

**It found a block leak on the first run.**

```
check: REPAIRED the FREE COUNT - the superblock claimed 12 free, the tree says 13
       (counted too little free space, off by 1)
```

One block, stranded permanently, on every refused create. `alloc_file` took the extent; `dir_add`
then failed inside `grow_dir` for want of one more directory block; the error propagated with the
extent still reserved. Nothing referenced those blocks afterwards - not the directory, not a
listing, not a walk - so only the free accounting knew, and it had one block fewer to give out for
the life of the volume. Exactly what this section's own warning describes: *an allocator that strands
a few blocks on every refusal turns a full disk into a shrinking one, and nothing in a listing would
ever show it.*

**The control is what makes it a finding rather than a coincidence.** Stopping twenty-nine creates
short of exhaustion gives `ok - filesystem is consistent` with nothing repaired. The strand belongs
to the REFUSAL, not to the writing.

**The fix is a rollback on the create path** (`write_path`, and the same shape in `write_new`). The
overwrite branch directly above it already reasoned about this ordering - "alloc the new file first,
free the old extent only after the record points at the new one" - and the create branch had no
rollback at all. A failed rollback does not mask the failure that caused it: the caller still gets
the original error.

**Still not covered:** injecting allocation failure at each individual allocation point rather than
only at the natural boundary. That needs an injector rather than a full disk, and is recorded rather
than half-done.

### 3.5 Concurrency, ordering and retries - BUILT (`fs-dupop` 5/0, `fs-lostreq` 10/0, `fs-twoclient` 8/0), and NARROWER than it looks

Stated honestly rather than adopted wholesale: **`fs` is single-threaded and serves one request to
completion before dequeuing the next.** There is no intra-operation interleaving to find, so "two
operations racing inside the filesystem" is not a reachable state and testing for it would be
theatre.

What IS reachable and worth attacking is the CLIENT side, and one item in it is a genuine open
weakness:

- **A duplicate request can repeat a destructive operation** - CONFIRMED, then closed at the client.
  `fs-dupop` completes a `move`, swallows its reply (`lose-reply-test`), and watches what the shell
  does. It did this:

  ```
    [diag] reacquired fs - retrying
    move: failed - source not found      <- the claim
    read /dup-b.txt
    duplicate-op-evidence                 <- the move had worked
  ```

  The operation SUCCEEDED and the operator was confidently told it failed. Not theoretical: the shell
  really does reacquire and re-send on a timeout, so this was live.

  **The protocol cannot deduplicate it away, and that is by design rather than oversight.** The
  correlation tag matches a reply to a request, and the retry deliberately draws a FRESH one so a
  late original can be told apart - which makes a retry indistinguishable from a new request. Closing
  it properly needs a client-supplied operation id that SURVIVES retries, plus a bounded reply cache
  in `fs`. That is real work and is recorded here rather than half-done.

  **What needed no protocol change was the honest answer.** Re-sending a non-idempotent op cannot
  help - it already ran - and can only mislead, so the shell no longer retries one. It says:

  ```
    move: OUTCOME UNKNOWN - the reply was lost; it MAY HAVE SUCCEEDED.
    Not re-sent - a retry can repeat a destructive operation. Check with `dir`
  ```

  Reads are still retried: nothing happened, so asking again is free. The mutating set is
  `op_is_mutating`, mirrored from `fs` because the shell must make this call at the moment `fs` is
  not answering.
- **Client timeouts on BOTH sides of the commit - BUILT (`fs-lostreq` 10/0).** `fs-dupop` above
  covers the after side: the move ran, its reply was swallowed. `fs-lostreq` covers the before side
  with a second injector (`drop-request-test`) that discards the request unserved, so the move never
  happened at all.

  **The two are indistinguishable from the client, and that is the finding rather than a gap.** A
  request sent, no reply, a deadline passed - identical in both. So the shell refuses to retry a
  mutating op either way and answers `OUTCOME UNKNOWN`, which is conservative here (nothing
  happened, so a retry would have been free) and necessary there (something did). Closing that gap
  properly needs the client-supplied operation id and bounded reply cache recorded above; until it
  exists, the conservative answer is the only honest one.

  What `fs-lostreq` pins is that the conservative answer stays TRUE on this side:

  ```
  the source file is untouched - the discarded move did NOT take effect
  the destination was never created
  fsck finds no corrupt blocks / the volume is consistent
  the shell did NOT claim the move succeeded
  re-issuing the operation AFTER an abandoned request works - nothing was left half-applied
  ```

  That last line is the one worth having. An abandoned request leaves no partial state behind, so
  the operator's own recovery - check with `dir`, then do it again - actually works.
- **Two clients on one path - BUILT (`fs-twoclient` 8/0), and the ordering is DOCUMENTED.** The
  bullet asked for the observable ordering to be checked against "what is documented - which
  currently is nothing, so documenting it is part of the gate". It is written down now, in
  `docs/persistence.md` 6.18, and the guarantee is one sentence: **every `fs` operation is atomic
  with respect to every other client.**

  It needs no locking. `fs` serves one request to completion before dequeuing the next, and every
  mutating op commits through the redo-journal - so there is no read-modify-write window a second
  client can enter, because there is no inside of an operation to reach.

  What is NOT atomic is a single client's multi-request IDIOM: between its two requests another
  client may be served. `write /x.txt` then `move /x.txt /y.txt` can legally have another client's
  `delete /x.txt` land between them, and the move then correctly reports its source gone. Each op
  was atomic; the sequence was not, and nothing promises otherwise.

  The test uses a REAL second client rather than a simulated one - `recorder` writing a capture
  through `fs` on its own schedule - and churns one path through create / read / rename / read /
  delete for six rounds in the same directory. Every read returned its own round's payload, the
  other client's file survived every round, the directory ended holding exactly what it should, and
  `drives check` reported 0 bad and consistent.

### 3.6 Stale identity and authority - PARTIALLY COVERED

The core of this is already enforced by the capability model rather than by convention: a file
capability is a delegated resource cap (7.10), revoked by a generation bump on delete, close and
rename, and `file-cap` (13 checks) pins unforgeable, non-escalating and revocable end to end. The
`move` doc records that open capabilities to a moved path are revoked including for every descendant.

What is NOT covered is the reuse case the checklist names: open A, restart `fs`, delete A, let B take
its storage, then use A's old handle. The generation mechanism should make this impossible, and
"should" is exactly the word this programme exists to remove.

### 3.7 Attack the block layer - BUILT (`fs-blockchaos` 10/0, `fs-blockdeath` 11/0, `fs-unplug` 8/0)

Kill and restart `block-driver` with requests outstanding; simulate hot-unplug, delayed return, I/O
error and device disappearance mid-write; inject late, duplicate, missing and out-of-order
completions.

**And there is already evidence this one matters.** `backlog/31` records a confirmed case of a reply
stream running behind - a service reading replies that belonged to earlier requests - in the network
stack, where a correlation tag proved the fault and was then REVERTED because rejecting a stale reply
is not the same as recovering from one. The fs/block channel has the same shape and the same tag
design. A completion from an old driver instance acknowledging a newer request is not hypothetical
here; it is the thing that already happened one layer over.

**BUILT: the completion stream, which is the half `backlog/31` warned about.** `io-error-test` makes
the DEVICE fail and `fs` answers that by retrying; `completion-chaos` corrupts the REPLY instead -
one duplicate, one missing, one carrying somebody else's tag - injected into ordinary `churn` traffic
after a measured 800-completion warm-up, so boot and mount are untouched.

Every detection assertion is paired with a recovery one, because that is the distinction that got
`backlog/31`'s tag reverted. A run where `fs` spots every bad completion and then cannot write a file
has failed this gate. What the machine actually did:

| injected | `fs` said | and then |
|---|---|---|
| DUPLICATE | `dropped 1 orphaned block reply(s) ... re-aligned` | the next write succeeded |
| WRONG-TAG | `discarded a block reply for tag 211 while awaiting 210` | `churn verify` completed |
| MISSING | `did not reply within 30 s - failing (NOT re-sent)` | the volume stayed mounted |

**And it found something, which is what it was for.** A file came back holding a mix of two
generations - `/churn/f2.bin diverges at byte 1 of 1200`. That is NOT corruption here, and the
distinction is the useful part: it sat beside `churn: done - 26 writes ... 2 refused`. The write was
refused and the caller was told. `fs-churn` requires `NONE torn` because a power cut returns no error
at all, so a mixed file there is unexplained; this test deliberately makes writes FAIL, and a caller
that ignores a reported failure and reads back a partial file is not a filesystem fault.

So the property asserted here is the sharper one, and it is the one §3.7 is really about: **no
completion fault may change data without somebody being told.** A tear accompanied by a refusal is a
reported failure; a tear with no refusal anywhere would be silent corruption.

**BUILT: the driver dying mid-request (`fs-blockdeath`).** A different fault from a wrong answer, and
a different recovery path - `fs` sees `SendFailed` (its cap names a dead endpoint), reacquires by
name and retries, which is the one retry the code considers safe because nothing is in flight when a
send never left. `churn` supplies the traffic and the kill arrives over the control channel on a
MARKER (churn's per-second heartbeat), never a timer: a fixed delay would sometimes cut before the
first write reached the driver, and the test would pass having proved nothing.

**The measurement is the point, not the survival.** `fs` allows each block request 30 s, so
"it recovered" is worth little - a system that merely timed out looks identical from the outside.
What separates the two is HOW FAST it noticed:

```
control: KILL block-driver
pci: BDF 0x0020 bus-master DISABLED on driver death (DMA quiesced)
kill_task: slot=6 'block-driver' freed 98 frames
supervisor: block-driver died, restarting
fs: block-driver send failed AND it could not be reacquired - the name does not resolve
```

**201 ms against a 30 s deadline.** The kernel told it (§8.6's death-wake); it did not sit out the
clock. That is Commandment V stated as a number: a dead dependency RETURNS, loudly, rather than
hanging its caller.

Two things the run surfaced that are now pinned rather than left as lines somebody once read. **DMA
is quiesced before the frames are reclaimed** - bus-mastering is disabled on driver death, and
`kill_task` frees 98 frames in the very next line; an unconfined DMA-capable driver has
kernel-equivalent reach (§6.4), so a dead one still mastering the bus could write into memory the
kernel has already handed out. And `fs` reports storage unavailable **in the gap** between the old
instance dying and the new one registering, then recovers on a later attempt - the window is real,
bounded, and loud rather than silent.

**BUILT: hot-unplug and device disappearance mid-write (`py scripts/fs_unplug.py`, 8/0).** The
DEVICE vanishes and does not come back - nothing is restarted and nothing recovers it, which is what
separates this from `fs-blockdeath` above, where the driver dies and the disk was there the whole
time.

**It does not run on x86, and finding out why changed the plan.** This paragraph used to say the row
needed "`device_del` over the monitor". It does, and on the machine every other storage suite uses
that command is refused:

```
(qemu) device_del thedisk
Error: Bus 'ahci.0' does not support hotplugging
```

QEMU's AHCI cannot hot-unplug at all. riscv64 carries its disk as a USB stick behind xHCI
(`storage_is_usb`), where the same `device_del` is accepted and the device is simply gone - and that
is the more honest unplug anyway, since people pull USB sticks and nobody hot-pulls a SATA disk.
Asked rather than assumed, which is the only reason a suite was not written against a mechanism that
refuses.

The stick is pulled with `churn` genuinely writing - 116 writes in, not at an idle prompt - and what
must hold is that the failure is LOUD and BOUNDED:

```
block-driver: 'xhci' did not answer within 10 s - reporting storage UNAVAILABLE rather than
              waiting on it (it is reachable but silent: busy, wedged, or idling with no controller)
fs: block read failed at lba 0 (device I/O error)
fs: op 10 took 11424500 us, 5 block ops, 99% of it inside them
fs: device I/O error seen - re-mounting before serving
```

No kernel panic, no wedged core, the shell back at a prompt, the disappearance **reported 12.5 s
after the unplug** - inside the 30 s a block request is allowed - and no stale content served from a
device that is gone.

**AND IT IS THE POSITIVE TEST FOR A FIX THAT HAD NONE.** The bounded `rpc` in `xhciblk.rs` (2026-09-21)
replaced an unbounded `request_with_reply` that waits forever on a peer which is alive but silent.
Every board in the hardware pass booted with its USB controller PRESENT and answering, so none of
them could reach that state - the fix had QEMU evidence and hardware no-regression, and nothing
more. A device pulled from under a live driver produces exactly it: `xhci` holding a request for
hardware that no longer exists. The `did not answer within 10 s` line above IS that bound firing.
Without it, `block-driver` blocks forever and `fs` eats a 30 s timeout per request with the shell
stalled behind it.

**One measurement note, because the first version of it was wrong.** The harness originally slept
45 s after the unplug and reported the latency as 45 s every time - it measured its own patience.
It now polls in one-second steps and stops the moment the system says something about the disk, so
12.5 s is the system's latency. A timing assertion that cannot fail is not an assertion.

Two calibration notes, because both first read as failures of the filesystem and were failures of the
test: a MISSING completion costs the caller its full 30 s deadline **by design** (`block-driver`
retries a busy device that long), so nine injections starved the verification phase of clock and
`drives check` reported nothing - indistinguishable from corruption at a glance. And the warm-up is
measured, not guessed: a boot consumes ~795 completions.

### 3.8a Power cuts, deliberately and at random - BUILT (`fs-window` 8/0, `fs-churn` 8/0)

Added after the Dell Wyse produced three clean power cuts and **not one `journal recovered` line**.
Nothing was wrong: the commit-to-checkpoint window is sub-millisecond, and a human with a plug samples
a fraction of a percent of a run. The recovery path had never run on silicon, and could not be made to.

Two instruments, because they answer different questions:

| | how | answers |
|---|---|---|
| **`fs-window`** | a `crash-window` build holds ONE known window open for ten seconds, armed by writing to a `/cutme...` path | **proves** recovery works - the next boot must say `journal recovered N block(s)` |
| **`fs-churn`** | the `churn <seconds>` utility runs thousands of transactions of every shape; the machine is cut at a moment nobody chose | **searches** for the windows nobody thought to aim at |

A proof and a search. Both carry to hardware unchanged, which is the point of building them here.

**`churn` is an ordinary shell command on a SHIPPING build** - no test feature compiled in. That
matters more than it looks: a fault that appears only in a build nobody ships is a fault about that
build. `crash-window` is necessarily a test feature, since holding a journal open is not a thing a
shipping filesystem should do.

**What `fs-churn` asserts is deliberately NOT "the journal recovered."** It usually will not; one cut
samples a narrow window once, and the run that verified this reported *"this cut fell outside the
commit window - no replay, which is the common case"*. What must hold every time, whatever the cut
hit, is the permitted-outcome table: the volume mounts, `0 bad`, and the accounting either consistent
or drifted in the SAFE direction. The assertion carrying the weight is the negative one -
`marked free but are IN USE` must never appear, because those blocks belong to a live file and the
next allocation would overwrite them.

One harness detail that decides whether either test is worth anything: **both kill on a MARKER, not a
timer.** `fs-window` cuts when `fs` announces the window is open; `fs-churn` cuts once churn's
per-second heartbeat proves it is writing. A fixed delay would sometimes cut before the first write
reached the disk, or after the checkpoint, and either way the test would pass having proved nothing.

### 3.8 Crash recovery itself - PARTIAL, and the covered half is MEASURED

**35 of the 75 tear points make the journal replay on mount.** Counted rather than assumed, and free:
every replay boot's serial is already captured and `fs` announces a recovery
(`journal recovered N block(s) from an interrupted write`).

| case | tear points | of which replay |
|---|---|---|
| overwrite | 22 | 10 |
| rename | 15 | 7 |
| move | 17 | 8 |
| delete | 21 | 10 |

That ratio is the size of the window between the commit record landing and the last home block being
written, which is the only window in which the journal does any work at all. So roughly half of every
sweep is a recovery running to completion and then being checked against the permitted-outcome table.
The count is reported per case precisely so a green tally cannot imply coverage it does not have: a
case where it came out ZERO would not have tested recovery at all, however many points it passed.

**What is covered:** recovery RUNS, on 35 genuinely distinct interrupted states, and the result is
inside the permitted set every time.

**What is NOT covered, and is the rest of this gate:** crashing DURING the recovery. Replay is itself
a sequence of writes, so the same record-and-replay nests - boot an `A_k` that replays, record the
writes the RECOVERY makes, then build and boot `A_k` plus each prefix of those. The properties it
would pin are the ones this section was written for: recovery is restartable, repeating it does not
progressively worsen the damage, and where it cannot determine a safe outcome it REFUSES a read-write
mount rather than guessing.

The code is already explicit that these are the stakes - `recover` leaves the journal INTACT on a read
failure so the next mount retries, refuses to apply blocks whose checksum changed between verify and
apply, and never clears a half-applied commit. Those are exactly the claims a nested sweep would
test, and they are currently argued rather than exercised.

The `read_only` mount path is the right mechanism for the refusal case and is already exercised by
`fs-compat` for a different reason.

### 3.9 Corruption and format validation - LARGELY COVERED

`fs-corrupt` (14), `fs-hostile` (6), `fs-fuzz` (83) and `fs-compat` (12) cover mutated headers,
damaged directory entries, cycles, impossible lengths, malformed names, and version/compat handling.
`fs-scrub` is read-only by construction and a suite asserts it repairs nothing.

Gaps: overlapping extents specifically, truncated images, and an explicit statement of on-disk field
widths and byte order - which matters for 3.10 and has never been written down as a contract.

### 3.10 Observability-unavailable - NOT APPLICABLE, with reason

The checklist asks that filesystem correctness not depend on events, logging or diagnostics being
available. **It structurally cannot.** `ctx.log()` is syscall 5 writing the kernel ring and serial
directly; no log line has ever been sent to the `events` service, whose contract declares only
`ipc_receive` (`CLAUDE.md` 11.4). Killing `events` loses no `fs` output and cannot block it.

Recorded as NOT APPLICABLE rather than PASS, because the honest claim is "the dependency does not
exist", not "we tested its absence". If logs are ever re-pointed at a service, this becomes a live
gate immediately - which is precisely why 11.4 forbids it.

### 3.11 Cross-ISA - BUILT, PASSES in QEMU (`py scripts/cross_isa.py`, 12/0)

**One GSFS volume, formatted and written by x86-64 over AHCI, then mounted, read, fsck'd and written
by riscv64 over USB BOT/SCSI, then carried back to x86-64 with riscv64's writes intact.** The same
raw file is attached to both machines; only the transport and the instruction set differ, which is
the whole test.

```
leg 1  x86-64 (AHCI)            formats, writes /from-x86.txt and /shared/note.txt, reads them back
leg 2  riscv64 (USB/BOT-SCSI)   mounts it, reads both, writes /from-riscv.txt, `drives check` 0 bad
leg 3  x86-64 (AHCI)            reads /from-riscv.txt, its own files survive, `drives check` 0 bad
```

So the expectation this section was written to remove is now a result: the format travels, and the
block transport does not change the picture. A volume written through one sector-at-a-time AHCI
controller is read through BOT/SCSI over a split transaction by a different ISA, and neither `fs`
nor `drives check` can tell.

**WHAT MADE IT REACHABLE, recorded because the obvious attempt was wrong.** The blocker below was
"riscv64 has no drive option at all", and the first fix attached an AHCI controller - reasoning that
`virt` has a real PCIe host bridge and `block-driver` already speaks AHCI. The kernel dutifully
granted `block-driver` an ABAR it would never read: `services/block-driver/build.rs` maps riscv64 to
`storage_is_usb`, and `main.rs` gates `#[cfg(not(storage_is_usb))] mod ahci`, so that file is not
compiled on this port. The device it needed was a USB stick behind `qemu-xhci`. Two real defects
were found on the way and both are fixed - a driver that hung when its host service was silent, and
a capacity latched at mount and never refreshed (`backlog/34`).

**One trap the gate now defends against itself**, because it cost a full red run: `riscv_build.py
--visionfive` links the kernel at 0x40200000 for the board, and QEMU's `virt` loads at 0x80200000.
Run the gate after a board build and OpenSBI comes up, our kernel prints nothing, and every riscv64
assertion fails - which reads exactly like "riscv64 cannot mount an x86 volume". The script builds
the kernel it needs rather than trusting what is lying in `target/`.

**Still cross-ISA only between two of the four ports.** The ARM rows below stand: `raspi4b` emulates
no VL805 and `raspi2b`'s stick re-enumerates endlessly, so aarch64 and arm32 remain unreachable in
QEMU and are a hardware job.

#### The blocker, as it stood - each port's own concrete reason

**riscv64's row is CLOSED** (see above). The other two are unchanged, measured rather than assumed:

| port | disk in QEMU | what happens |
|---|---|---|
| x86-64 | yes, AHCI | works - a volume was flashed, written and read back |
| riscv64 | none attached | `riscv_run.py` has no drive option at all |
| aarch64 | attached, invisible | no VL805 emulation, so the controller idles with no MMIO and the OS never sees it |
| arm32 | attached, never settles | the stick enumerates, binds, reports 16 MiB - **126 times in two minutes**, re-addressed each cycle. `block-driver` asks for capacity before the first enumeration completes, latches 0, and `fs` never mounts |

The control rules out the port and the day's changes: the same arm32 kernel with no stick attached
boots clean, `supervisor: ready`, prompt working, **zero** connect events. Real Pi 2 hardware runs the
storage stack fine - that is how `selfcheck` reaches 349/0 on the board - so this is QEMU's dwc2
emulation rather than the driver on silicon.

**That half-done note is superseded.** It read: "a GSFS volume was flashed and written on x86-64 and
is sitting on disk ... when a board is available, that image goes on a stick and the second half runs
unchanged." The second half runs now, in QEMU, on every run of `cross_isa.py` - and it did not need a
board.

**Why this matters more than one gate.** Every storage guarantee in this file is verified on ONE
architecture - fifteen suites, 75 tear points, the recovery measurements, all x86-64 and all AHCI. It
also hides a whole category of bug: anything where the BLOCK TRANSPORT changes the picture. AHCI
hands `fs` a sector; USB mass storage hands it one through BOT/SCSI over a split transaction. The
filesystem should not care, and "should not" is the phrase this programme exists to remove.

That reasoning held for as long as no port could attach a disk, and it is why this was recorded as
**not reachable** rather than merely not done. It stopped being true on 2026-09-21. What remains
true is the narrower version: for aarch64 and arm32, hardware is still the only way to get an
answer.

### 3.12 Physical hardware - ALL FIVE BOARDS PASS (2026-09-21), and the journal half is CLOSED and REPRODUCED

Real controllers, hotplug, restart, and the flush/durability assumptions that QEMU does not model.
Five boards. **A QEMU pass is never recorded as a hardware pass.**

#### The five-board pass, 2026-09-21 - one image per architecture, `selfcheck` twice on each

| board | arch | `selfcheck` | what only this board could say |
|---|---|---|---|
| Dell Wyse 5070 | x86-64 | 502 / 0 / 0, twice | no EHCI present, so it could not test the handoff below |
| HP T630 | x86-64 | 502 / 0 / 0, twice | **the EHCI BIOS handoff ran for the first time ever** |
| Raspberry Pi 2 | armv7 | 493 / 0 / **1**, three times | `chaos kill-storm dwc2` 100 rounds, 100/100 recovered |
| Raspberry Pi 4 | aarch64 | 502 / 0 / 0, twice | no `backlog/03` fault, no `backlog/22` blanking |
| VisionFive 2 Lite | riscv64 | 502 / 0 / 0, twice | **`reboot` was a spin loop**; fixed and verified |

**The counts are self-consistent, which is worth more than the zeros.** 502 wherever PCI exists and
493 + 1 skipped where it does not: the Pi 2 is the only board without PCI and the only one that
skips, and it names the reason (`hw-enumerator - this machine has no PCI to enumerate`). Nothing is
being silently dropped on any machine, which is the failure mode a bare "0 failed" cannot rule out.

**Two defects were found BY the hardware, not confirmed by it** - both in code that no QEMU run
could reach:

* **The EHCI USBLEGSUP handoff had never executed on any machine.** QEMU has no EHCI and the Wyse
  has none either, so only the T630 could run it. It hit the hard case on the first try: the
  firmware held the controller (`USBLEGSUP` bit 16 set) and REFUSED to release it, so ownership was
  forced - and the keyboard behind the hub kept working, which was the stated fear.
  (`backlog/11`, closed.)
* **`reboot` on riscv64 was `loop { spin_loop() }`** that printed `reboot: hardware reset` first.
  It wedged a hart inside syscall 18 and the liveness watchdog panicked ten seconds later, exactly
  as designed. Now SBI SRST, warm before cold - because a cold reboot power-cycles through the PMIC
  and OpenSBI's PMIC driver fails on this board (`pmic_ops: cannot read pmic power register`), while
  a warm one never touches it. Two clean reboots on the board, one with the stick pulled.

**What the hardware pass did NOT establish, said plainly.** The two USB-path fixes from the same day
have no positive hardware proof. Every board booted with its USB host controller PRESENT and
answering, so none reached the condition the fix addresses - a host service alive but SILENT. That
condition is what QEMU's `virt` produces (no USB controller at all, so `xhci` idles and never
replies), and it is structural on silicon: the VisionFive has its controller on-SoC, the Pi 2 has
DWC2, the Pi 4 has the VL805. Three boards establish NO REGRESSION, which is not the same claim.

#### Journal recovery on silicon - PROVEN 2026-09-18, reproduced twice 2026-09-20, Dell Wyse 5070

This was the standing hole and it is worth stating exactly what closed it, because three earlier
power cuts on this same board did NOT close it: they survived cleanly, and the journal was never
invoked in any of them. A cut that lands outside the commit-to-checkpoint window proves the
filesystem was consistent, not that recovery works. The window is sub-millisecond, which is why
`churn` exists - to run thousands of transactions so a human with a plug can land in one.

It landed. 30 GB SSD, GSFS0008 over 62,533,296 blocks:

```
16:37:20  gsh> churn 100
16:37:23  churn: 4s elapsed, 125 writes          <- the log ends here: power cut, mid-write
16:38:08  fs: journal recovered 4 block(s) from an interrupted write
16:38:08  fs: mounted GSFS0008 (62533296 blocks, bitmap 1..15332, root@15342, 62517885 free)
16:38:52  churn verify: 6 file(s) checked, 0 empty, NONE torn - every file holds one generation
19:13:25  check: 51 files, 3 dirs, 0 bad; 15411 blocks used, 62517885 free
19:13:25  check: the free count already agreed with the tree - nothing was repaired
19:13:25  check: ok - filesystem is consistent
```

Four blocks were durable in the journal and not yet checkpointed home when the power went. The
next mount replayed them, and content verification then found no file holding a mix of two writes.
That is the whole claim of §6.8 of `docs/persistence.md`, on real silicon, end to end.

**BOTH QUESTIONS ARE ANSWERED, and they are different questions.** `churn verify` reads content -
is any file a mix of two writes? `drives check` reads structure - tree, bitmap, free count.
`51_churn.md` is explicit that one does not imply the other: a file holding the first half of one
write and the second half of another has perfectly valid block CRCs, sits in a valid directory,
and occupies correctly accounted blocks.

**And the structural result is the STRONG form.** "Nothing was repaired" is not the same as "it
was repaired successfully". The permitted-outcome table (§2) allows an interrupted `delete` to
leave blocks marked used - a leak - which `drives check` would silently reclaim; that would still
have been a pass. It did not happen. The free count the recovery mount reported (62,517,885) is
the same number the check computed from the tree hours later, so recovery left the accounting
already correct rather than merely correctable.

**And it remains one board.** The backend-conditional caveat in `CLAUDE.md` §6.1 is untouched: this
is a SATA SSD behind AHCI, which attests durability at the journal barriers. The Pi 2's USB stick
refuses `SYNCHRONIZE CACHE` outright, so nothing here transfers to it.

#### Reproduced twice more, 2026-09-20 - and the POST-MORTEM is now one command

Two further cuts on the same board landed in the window, so the sub-millisecond window has now been
hit three times. That matters less than what happened next, which is a change in how the evidence is
collected rather than in what it says.

**The problem the third run solves: the evidence expires.** After a cut the machine reboots with
`/churn` still on disk, holding the only record of whether recovery held - and the NEXT churn
overwrites it. Every earlier run depended on a human remembering to type `churn verify` before
anything else touched the disk, at a bench, with the lid off. Forgetting it destroys the answer
silently.

`selfcheck` now verifies a leftover run before starting a new one, so the whole post-mortem is one
command:

```
17:05:43  smp: 4 cores ready
17:05:44  fs: journal recovered 1 block(s) from an interrupted write
          selfcheck: an earlier churn is still on disk - verifying it BEFORE it is overwritten
          churn verify: 7 file(s) checked, 0 empty, NONE torn
          PASS  churn - the earlier run holds no torn file (if it was cut, recovery held)
17:06:26  run: ran 502, failed 0, skipped 0
```

Recovery ran 1.1 seconds after the cores came up, the content check found no file holding a mix of
two writes, and the suite that reported it is the same one an operator runs for everything else.

**What `selfcheck` still cannot do is pull its own power.** A deliberate recovery test remains
`churn <seconds>` and a hand on the cord; what is automated is the VERDICT, not the fault. Saying so
matters because the automation is easy to mistake for coverage.

**Two `selfcheck` runs in one boot, both clean** (`ran 501, failed 0` then `ran 500, failed 0`),
which closes `backlog/36` on hardware rather than in QEMU. That entry existed because the second run
in a boot failed intermittently on `events persist status` - a fixed `wait 3` racing a variable
extent pre-fill. The state it waited for (`preparing`) never appeared in this capture and the poll's
30-attempt bound was never approached, so the fix reaches the real state immediately rather than the
old sleep merely having been long enough.

**Still one board.** Three cuts on one Dell Wyse is three samples of the same SATA-behind-AHCI
backend, and §6.1's caveat is untouched by repetition: a drive that attests durability at the
barriers is the favourable case, and `fs-lyingflush` exists precisely because the Pi 2's stick is
not it. Four boards remain.

## 4. Merge evidence

Filled in from what has actually been run. NOT RUN means not run.

| Gate | Result | Evidence / notes |
|---|---|---|
| Feature and operation tests | PASS (QEMU) | `osdev test fs-all` 30 of 30 in ~41 min, including `fs-tear` and `fs-model`; `files` 239/0; `shell` 183/0 (measured 2026-09-21) |
| Independent reference-model tests | PASS (QEMU) | `osdev test fs-model` - 8 seeds, ~1,500 operations, no disagreement. Found one real gap on its first run: `seal` idempotence was decided in the code and documented nowhere. Interruption, and 5 of the 12 operations, are named as uncovered in 3.2 |
| Crash-point and persistence matrix | **PASSES (QEMU)** | `osdev test fs-tear` - NINE operations, 220 tear points, every one inside the permitted set, plus a control proving the oracle can reject. Nine of the eleven rows of section 2; the two `write-at` forms have no shell verb that issues `OP_WRITE_AT` alone and section 2 permits any prefix of their chunks, so they are recorded as not separately reachable rather than faked (3.1). |
| Data/metadata exhaustion | **PASSES (QEMU)** | 3.4 - `fs-full` 14/0 and `fs-metafull` 9/0. The second fills a DIRECTORY until a create is refused and found a real leak: one block stranded per refused create, with a control attributing it to the refusal rather than the writing. Fixed. Metadata and data are not separate pools here, so exhausting one independently is unreachable. Per-allocation-point injection is still not covered. |
| Concurrency and retry ordering | **PASSES (QEMU)** | 3.5 - `fs-dupop` 5/0, `fs-lostreq` 10/0, `fs-twoclient` 8/0. Timeouts on BOTH sides of the commit, and two real clients on one directory. The ordering guarantee is documented in `docs/persistence.md` 6.18. The duplicate-request gap is still real and still recorded: closing it needs a client-supplied op id plus a bounded reply cache in `fs`. |
| Stale-handle and identity tests | **PASSES (QEMU)** | `file-cap` 13/0 covers revocation on delete/close/rename. `fs-reuse` 8/0 covers the case none of those could: a capability minted BEFORE an `fs` restart, with the blocks it named handed to a different file afterwards. The stale cap resolved to nothing - not to the replacement, not to anything. That would have been a leak of AUTHORITY rather than space, which no fsck can see (3.6). |
| Block-driver completion stream (duplicate / missing / out-of-order) | **`fs-blockchaos` 10/0** | 3.7. The `backlog/31` shape, detected AND recovered |
| Block-driver killed WITH REQUESTS OUTSTANDING | **`fs-blockdeath` 11/0** | 3.7. Noticed in 201 ms against a 30 s deadline - woken, not timed out |
| Hot-unplug / device disappearance mid-write | **PASSES (QEMU)** | 3.7 - `py scripts/fs_unplug.py` 8/0. The stick is pulled with `churn` writing; no panic, no wedge, reported 12.5 s after the unplug and no stale content served. On riscv64 because QEMU's AHCI refuses `device_del` outright - USB is the only bus that can be unplugged, and the more honest one. |
| A destructive op whose REPLY is lost, then retried | **`fs-dupop` 5/0** | 3.5. Found a live gap: a succeeded `move` reported as failed. Fixed at the client |
| Power cut on a drive with a VOLATILE WRITE CACHE | **`fs-cache` 8/0** | 3.3. The first suite to cut a medium that had not yet committed what it acknowledged |
| Power cut on a drive that IGNORES the barrier | **`fs-lyingflush` 7/0** | 3.3 / 6.1's unguaranteed case. Asserts detection, not recovery |
| Interrupted recovery | **PASSES (QEMU)** | 3.8 - recovery RUNS on 35 of 75 measured tear points and lands inside the permitted set every time; `fs-window` 8/0 (a real machine kill inside the commit window) and `fs-churn` 9/0 (a cut at an unchosen moment). And now `fs-rtear` 5/0: the SECOND-ORDER tear, cutting the REPLAY itself at every sector it writes. 4 of 5 cuts made recovery re-run, so replay is re-entrant rather than one-shot - which is what a redo journal's idempotence claims and what nothing had checked. |
| Corruption and format validation | PASS (QEMU) | `fs-corrupt` 14/0, `fs-hostile` 6/0, `fs-fuzz` 83/0, `fs-compat` 12/0. Gaps named in 3.9 |
| Observability-unavailable | NOT APPLICABLE | 3.10 - `fs` logging does not route through any service; `CLAUDE.md` 11.4 |
| Cross-ISA QEMU image tests | **PASSES (QEMU)** | 3.11 - `py scripts/cross_isa.py` 12/0. One volume, x86-64 (AHCI) -> riscv64 (USB BOT/SCSI) -> x86-64: each side reads the other's files and `drives check` reports 0 bad on both. aarch64 and arm32 remain unreachable in QEMU (no VL805 emulation; the arm32 stick re-enumerates) and are a hardware job. |
| Physical-hardware validation | **selfcheck: 5 of 5.** Power cut: **5 of 5** | **`selfcheck` on all five boards** (3.12). **The POWER CUT is 4 of 5, every one in the STRONG form** - recovered, content intact, accounting correct WITHOUT repair. Dell Wyse 5070 (AHCI, 2026-09-18). VisionFive 2 (USB/xhci, riscv64) and Raspberry Pi 4 (USB/xhci, aarch64), both 2026-09-22, cut DETERMINISTICALLY with a `crash-window` build. **HP T630 (AHCI, 2026-09-22) is the strongest of the four: an UNASSISTED cut** - x86 has no crash-window build, so the power went out at a moment nobody chose and landed inside the commit-to-checkpoint window on the first attempt. `selfcheck` 509/0/0 twice, `journal recovered 4 block(s)`, `churn verify` 7 files NONE torn, `drives check` 0 bad and nothing repaired - with no ten-second window helping the drive. It also ran the EHCI BIOS handoff's hard case again (`released=0 ... FORCED after timeout`) with the keyboard behind the hub still working; no other board has EHCI. **The second and third boards earned the `nothing repaired` clause**: the FIRST VisionFive cut reported `REPAIRED the FREE COUNT ... off by 1`, traced to the mount reading the superblock BEFORE the replay that rewrites it. Fixed, then re-verified on three controllers. **THE PI 2 HAS NOW BEEN CUT TOO, and it is deliberately NOT counted as a fifth.** 2026-09-22, unassisted, 18 s into `churn 30`: the volume came back with `churn verify` 6 files NONE torn, `drives check` 0 bad, and the superblock's free count EXACTLY matching a full rebuild from the tree - the assertion the stale-superblock bug broke, holding on a third controller. But there was **no `journal recovered` line**, so the journal did no work and the ORDERING the guarantee rests on was never exercised. A cut that misses the commit window tests durability, not recovery. Worse, the build could not say which had happened: `Fs::recover` discarded a torn commit record in SILENCE, identically to finding no record at all, so the one instrument that could have answered 6.1 was mute in exactly the case 6.1 is about. Now fixed and covered by a deliberate `fs-tear` case, because no tear point can reach it - the tap cuts BETWEEN sectors and the commit record is one sector, so 220 tear points had never once tested that branch. A second unassisted cut (17 s) also missed, which the arithmetic predicts: the commit window lasts UNDER A MILLISECOND against a ~52 ms transaction on this stick, so an unaimed cut hits it with probability under 2%. **THE THIRD ONE HIT** (2026-09-23, 17 s into `churn 30`, plain image, `CUT THE POWER AT ANY POINT`): `fs: journal recovered 4 block(s) from an interrupted write`, then `churn verify` 7 files NONE torn, `drives check` 13 files 0 bad, and the superblock's free count matching a full tree rebuild exactly with nothing repaired. **The strong form, unassisted, on the board 6.1 names as the exception** - and the SECOND unassisted cut in this matrix after the T630, so two of the five owe nothing to a held-open window. Three attempts at ~2% each is ordinary luck, not a suspicious result. What it establishes is the ordering itself: the commit record was durable before any home block moved, on a stick 6.1 says cannot be ordered. `backlog/42` carries the case for amending 6.1. |
| Kernel changes / scope boundary review | PASS | No kernel source change on this branch. `osdev build` runs 20 commandment checks and 73 redteam probes, including the kernel module set against 4.3 |

**Merge rule adopted:** do not merge until the required gates pass, genuinely inapplicable gates are
justified in writing, and the remaining limitations are documented. Where hardware validation is
deferred, the result is labelled **QEMU-validated only** and hardware readiness is not claimed.

## 5. Turning surprises into permanent tests

For every bug this finds: preserve the seed, the operation sequence, the fault point and the failing
image; reduce to the smallest deterministic reproduction; name the violated invariant and the layer
responsible; fix the smallest component without weakening a boundary; add the reproduction to a
suite; rerun the focused test and the full sweep.

**Chaos discovers surprises; deterministic regressions keep them fixed.** `fs-tear` is built this way
already - it keeps its recording on every run, not only on failure, and keeps the failing image
whenever a tear point falls outside the permitted set.

## 6. Not in scope

- Making `write-at` atomic by default. The journaled variant exists for callers that need it; making
  it the default would put every streaming byte through a 32 KiB journal region.
- Repairing a torn sequence automatically. Detection is the guarantee; silent repair of data whose
  correct value is unknown is the second half of the mission statement at the top of this file.
- Expanding the kernel to make any of this testable. Filesystem semantics stay in userspace (4.4);
  a test that needs a kernel change needs a different test.

## 7. MISCIS

The source checklist twice refers to **MISCIS** ("do not expand MISCIS merely to make a test pass";
"Kernel changes / MISCIS boundary review"). It is the mnemonic for the kernel's six responsibilities -
**M**emory isolation, **I**PC, **S**cheduling, **C**apabilities, **I**nterrupts, **S**MP routing -
now written into `CLAUDE.md` §4.3, which is where a reader will look for it.

So the constraint is §4.4 in short form: a seventh responsibility is a change to what the kernel IS.
Nothing in this programme needs one. The merge-evidence row reads "Kernel changes / scope boundary
review" and is PASS because no kernel source changed on this branch at all.

This section originally recorded the term as not understood, and is kept rather than deleted because
the reading was provisional and is now confirmed - the difference between a guess and a checked fact
is worth leaving visible.
