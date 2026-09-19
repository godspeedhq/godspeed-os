# GSFS maximum carnage - the guarantees, written down, then attacked

**Status, as of 2026-09-18.** BUILT AND PASSING in QEMU: torn writes (`fs-tear` 18/0, four
operations, 75 tear points, 35 exercising journal recovery), resource exhaustion (`fs-full` 14/0),
power cuts aimed and random (`fs-window` 8/0, `fs-churn` 8/0), and the **independent oracle**
(`fs-model`, §3.2), the block layer (§3.7 - `fs-blockchaos` for the completion stream,
`fs-blockdeath` for the driver dying mid-request; hot-unplug is not). NOT RUN: concurrency and retry ordering (§3.5), the
remaining rows of §3.3, and cross-ISA (§3.11, not reachable in QEMU).

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
| `fs-tear` | **NEW.** Every prefix of an operation's writes, recorded from the real driver and booted. 3 operations, 54 tear points, plus a control proving the oracle can reject. Section 3.1 |
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

**Results so far - three operations, 54 tear points, every one inside the permitted set:**

| operation | sectors written | tear points | of which replay | outcome |
|---|---|---|---|---|
| overwrite a whole file | 22 | 22 | 10 | wholly `ORIGINAL` or wholly `NEWNEWNEW`, never a mix |
| rename | 15 | 15 | 7 | the old name or the new name, never both, never neither |
| move across directories | 17 | 17 | 8 | in the source or in the destination, never both, never neither |
| delete | 21 | 21 | 10 | gone, or still there, or gone-with-a-leak. Never a block marked free while a live file uses it |

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

### 3.3 Crash at every persistence boundary - PARTIAL

3.1 does this for three operations, exhaustively. The remaining work is the other operations, the
reordered/delayed/failed variants, and the volatile-write-cache model: an acknowledged write is not
durable without the declared barrier, and the test must be able to express the difference.

**Killing `fs` is a service-restart test, not a power-loss simulation.** Recorded here because it is
the mistake this whole programme exists to stop repeating: `fs-restart` is a good test of a different
thing.

### 3.4 Resource exhaustion - BUILT, PASSES in QEMU (`osdev test fs-full`, 14/0)

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

**Not yet covered:** exhausting METADATA while data space remains (and the reverse), and injecting
allocation failure at each individual allocation point rather than only at the natural boundary.

### 3.5 Concurrency, ordering and retries - NOT RUN, and NARROWER than it looks

Stated honestly rather than adopted wholesale: **`fs` is single-threaded and serves one request to
completion before dequeuing the next.** There is no intra-operation interleaving to find, so "two
operations racing inside the filesystem" is not a reachable state and testing for it would be
theatre.

What IS reachable and worth attacking is the CLIENT side, and one item in it is a genuine open
weakness:

- **A duplicate request can repeat a destructive operation, and nothing stops it today.** The fs
  protocol carries a correlation tag at byte 0 that is ECHOED, never interpreted - it exists to match
  a reply to a request, not to deduplicate. A client that times out and retries a `delete` or a
  `move` sends it twice, and the second one executes. For `delete` that is harmless; for a
  `move` whose first attempt succeeded, the retry operates on a path that no longer means what the
  client thought. This is the clearest thing on this list that is a design gap rather than a missing
  test.
- Client timeouts injected immediately before and after a commit, then retried, with the outcome
  inspected.
- Two clients issuing conflicting sequences (create, rename, delete, recreate) against the same
  paths, checking the observable ordering matches what is documented - which currently is nothing,
  so documenting it is part of the gate.

### 3.6 Stale identity and authority - PARTIALLY COVERED

The core of this is already enforced by the capability model rather than by convention: a file
capability is a delegated resource cap (7.10), revoked by a generation bump on delete, close and
rename, and `file-cap` (13 checks) pins unforgeable, non-escalating and revocable end to end. The
`move` doc records that open capabilities to a moved path are revoked including for every descendant.

What is NOT covered is the reuse case the checklist names: open A, restart `fs`, delete A, let B take
its storage, then use A's old handle. The generation mechanism should make this impossible, and
"should" is exactly the word this programme exists to remove.

### 3.7 Attack the block layer - BUILT (`fs-blockchaos` 10/0, `fs-blockdeath` 11/0)

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

**STILL NOT RUN:** hot-unplug and device disappearance mid-write. Those need the emulated DEVICE to
vanish rather than the driver, which is a QEMU-side injection (`device_del` over the monitor) and a
separate piece of work.

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

`fs-corrupt` (14), `fs-hostile` (6), `fs-fuzz` (43) and `fs-compat` (12) cover mutated headers,
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

### 3.11 Cross-ISA - NOT RUN, and NOT REACHABLE IN QEMU (`backlog/34`)

Attempted, and the attempt is the result. **No non-x86 port can reach a disk in QEMU**, each for its
own concrete reason, measured rather than assumed:

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

**The half that IS done:** a GSFS volume was flashed and written on x86-64 and is sitting on disk,
holding `/from-x86.txt` and `/shared/note.txt`. When a board is available, that image goes on a stick
and the second half runs unchanged.

**Why this matters more than one gate.** Every storage guarantee in this file is verified on ONE
architecture - fifteen suites, 75 tear points, the recovery measurements, all x86-64 and all AHCI. It
also hides a whole category of bug: anything where the BLOCK TRANSPORT changes the picture. AHCI
hands `fs` a sector; USB mass storage hands it one through BOT/SCSI over a split transaction. The
filesystem should not care, and "should not" is the phrase this programme exists to remove.

So this gate is recorded as **not reachable in QEMU** rather than merely not done, which is a
different fact: it changes what the hardware pass is FOR. For everything else in this file hardware
is a confirmation at the end. For this, it is the only way to get an answer at all.

### 3.12 Physical hardware - PARTIAL (1 of 5 boards), and the journal half is now CLOSED

Real controllers, hotplug, restart, and the flush/durability assumptions that QEMU does not model.
Five boards. **A QEMU pass is never recorded as a hardware pass.**

#### Journal recovery on silicon - PROVEN, 2026-09-18, Dell Wyse 5070

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

## 4. Merge evidence

Filled in from what has actually been run. NOT RUN means not run.

| Gate | Result | Evidence / notes |
|---|---|---|
| Feature and operation tests | PASS (QEMU) | `osdev test fs-all` (19 suites, including `fs-tear` and `fs-model`); `files` 239/0; `shell` 183/0 (measured 2026-09-18) |
| Independent reference-model tests | PASS (QEMU) | `osdev test fs-model` - 8 seeds, ~1,500 operations, no disagreement. Found one real gap on its first run: `seal` idempotence was decided in the code and documented nowhere. Interruption, and 5 of the 12 operations, are named as uncovered in 3.2 |
| Crash-point and persistence matrix | PARTIAL (QEMU) | `osdev test fs-tear` 18/0 - four operations, 75/75 tear points, oracle proved able to reject. Seven rows of section 2 remain |
| Data/metadata exhaustion | PARTIAL (QEMU) | `osdev test fs-full` 14/0 - a refused allocation names its reason, damages no bystander and leaks nothing. Metadata-vs-data exhaustion not covered |
| Concurrency and retry ordering | NOT RUN | 3.5, and narrower than it reads - see the single-threaded note. The duplicate-request gap is real |
| Stale-handle and identity tests | PARTIAL (QEMU) | `file-cap` 13/0 covers revocation on delete/close/rename; storage REUSE across an `fs` restart is not covered |
| Block-driver completion stream (duplicate / missing / out-of-order) | **`fs-blockchaos` 10/0** | 3.7. The `backlog/31` shape, detected AND recovered |
| Block-driver killed WITH REQUESTS OUTSTANDING | **`fs-blockdeath` 11/0** | 3.7. Noticed in 201 ms against a 30 s deadline - woken, not timed out |
| Hot-unplug / device disappearance mid-write | NOT RUN | 3.7. Needs the DEVICE to vanish (QEMU `device_del`), not the driver |
| Interrupted recovery | PARTIAL (QEMU) | 3.8 - recovery RUNS on 35 of 75 tear points (measured) and lands inside the permitted set every time. Plus `fs-window` 8/0 (a real machine kill inside the commit window, recovered) and `fs-churn` 8/0 (a cut at an unchosen moment). Crashing DURING recovery is still not covered |
| Corruption and format validation | PASS (QEMU) | `fs-corrupt` 14/0, `fs-hostile` 6/0, `fs-fuzz` 43/0, `fs-compat` 12/0. Gaps named in 3.9 |
| Observability-unavailable | NOT APPLICABLE | 3.10 - `fs` logging does not route through any service; `CLAUDE.md` 11.4 |
| Cross-ISA QEMU image tests | NOT RUN - NOT REACHABLE | 3.11 / `backlog/34`. No non-x86 port can attach a usable disk in QEMU: riscv64 has no drive option, aarch64 has no VL805 emulation, arm32's stick re-enumerates 126 times and never settles. The x86 half is written and waiting |
| Physical-hardware validation | PARTIAL (1 of 5 boards), journal half CLOSED | Dell Wyse 5070, 2026-09-18: `selfcheck` 492/0 on a 30 GB SSD. A power cut during `churn` landed INSIDE the commit-to-checkpoint window: the next mount reported `journal recovered 4 block(s) from an interrupted write`, and `churn verify` then found 6 files, NONE torn. `drives check` then reported `0 bad` and `nothing was repaired`, with the free count identical to the one the recovery mount computed - so both the content and the structural question are answered, and the structural one in its strong form. Recovery on silicon is proven (§3.12). Three earlier cuts had survived cleanly without ever invoking the journal, which proved consistency and not recovery. Still open: four boards |
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
