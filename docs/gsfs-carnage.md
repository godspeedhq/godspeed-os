# GSFS maximum carnage - the guarantees, written down, then attacked

**Status: the torn-write gate PASSES in QEMU across three operations (`osdev test fs-tear`, 14/0,
54 tear points), the rest are NOT RUN. This is QEMU-validated only; no hardware result is claimed
anywhere in this file.**

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

| operation | sectors written | tear points | outcome |
|---|---|---|---|
| overwrite a whole file | 22 | 22 | wholly `ORIGINAL` or wholly `NEWNEWNEW`, never a mix |
| rename | 15 | 15 | the old name or the new name, never both, never neither |
| move across directories | 17 | 17 | in the source or in the destination, never both, never neither |

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

### 3.2 An independent oracle - NOT RUN

A small abstract model of files, directories, names and contents, host-side in `osdev` where the
suites already live and where a `HashMap` is allowed (26.6.1 governs what runs on the machine).

**It must not reuse GSFS allocation, traversal, rename or recovery logic.** A model that shares the
implementation reproduces its bugs and agrees with them, which is worse than no model because it
produces a green tick. This is the single most important constraint on this gate.

Reproducible sequences, recorded seeds, and comparison of return values, trees, metadata and file
bytes. Under injected interruption the comparison is not equality but MEMBERSHIP of the permitted set
in section 2 - which is why that table had to exist first.

### 3.3 Crash at every persistence boundary - PARTIAL

3.1 does this for three operations, exhaustively. The remaining work is the other operations, the
reordered/delayed/failed variants, and the volatile-write-cache model: an acknowledged write is not
durable without the declared barrier, and the test must be able to express the difference.

**Killing `fs` is a service-restart test, not a power-loss simulation.** Recorded here because it is
the mistake this whole programme exists to stop repeating: `fs-restart` is a good test of a different
thing.

### 3.4 Resource exhaustion - NOT RUN

Fill data space to near capacity and exercise every operation; exhaust metadata while data remains
and the reverse; inject allocation failure at each allocation point. Then verify what matters, which
is not the error message: no leaked blocks, no double allocation, no orphaned-but-reachable data, no
damage to unrelated files, and a filesystem that still accepts valid work afterwards.

`drives check` rebuilds the free bitmap by walking the tree, so it is the natural oracle for the leak
and double-allocation half.

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

### 3.7 Attack the block layer - NOT RUN

Kill and restart `block-driver` with requests outstanding; simulate hot-unplug, delayed return, I/O
error and device disappearance mid-write; inject late, duplicate, missing and out-of-order
completions.

**And there is already evidence this one matters.** `backlog/31` records a confirmed case of a reply
stream running behind - a service reading replies that belonged to earlier requests - in the network
stack, where a correlation tag proved the fault and was then REVERTED because rejecting a stale reply
is not the same as recovering from one. The fs/block channel has the same shape and the same tag
design. A completion from an old driver instance acknowledging a newer request is not hypothetical
here; it is the thing that already happened one layer over.

### 3.8 Crash recovery itself - NOT RUN

Interrupt recovery, at each mutation point, repeatedly, on the same image. Verify it is restartable
and does not progressively worsen the damage, and that when it cannot determine a safe outcome it
REFUSES a read-write mount rather than guessing. The existing `read_only` mount path is the right
mechanism for that refusal and is already exercised by `fs-compat` for a different reason.

This is where 3.1's machinery pays off twice: recovery is itself a sequence of writes, so the same
record-and-replay applies to it directly.

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

### 3.11 Cross-ISA - NOT RUN

Create and modify the same image across x86-64, riscv64 and aarch64 in QEMU, comparing bytes, trees
and metadata after each handoff. Every field is little-endian by construction, so the expectation is
a pass - and an expectation is not a result. Cheap, and the kind of thing a user finds if we do not.

### 3.12 Physical hardware - NOT RUN, and cannot be claimed from any of the above

Real controllers, hotplug, restart, and the flush/durability assumptions that QEMU does not model.
Five boards. **A QEMU pass is never recorded as a hardware pass.**

## 4. Merge evidence

Filled in from what has actually been run. NOT RUN means not run.

| Gate | Result | Evidence / notes |
|---|---|---|
| Feature and operation tests | PASS (QEMU) | `osdev test fs-all` (now 15 suites including `fs-tear`); `files` 222/0; `shell` 174/0 |
| Independent reference-model tests | NOT RUN | 3.2. Blocked on nothing but effort; the outcome table it needs now exists |
| Crash-point and persistence matrix | PARTIAL (QEMU) | `osdev test fs-tear` 14/0 - three operations, 54/54 tear points, oracle proved able to reject. Eight rows of section 2 remain |
| Data/metadata exhaustion | NOT RUN | 3.4 |
| Concurrency and retry ordering | NOT RUN | 3.5, and narrower than it reads - see the single-threaded note. The duplicate-request gap is real |
| Stale-handle and identity tests | PARTIAL (QEMU) | `file-cap` 13/0 covers revocation on delete/close/rename; storage REUSE across an `fs` restart is not covered |
| Block-driver restart and hot-unplug | NOT RUN | 3.7. See `backlog/31` for the same failure shape one layer over |
| Interrupted recovery | NOT RUN | 3.8 |
| Corruption and format validation | PASS (QEMU) | `fs-corrupt` 14/0, `fs-hostile` 6/0, `fs-fuzz` 43/0, `fs-compat` 12/0. Gaps named in 3.9 |
| Observability-unavailable | NOT APPLICABLE | 3.10 - `fs` logging does not route through any service; `CLAUDE.md` 11.4 |
| Cross-ISA QEMU image tests | NOT RUN | 3.11 |
| Physical-hardware validation | NOT RUN | 3.12. No hardware result is claimed anywhere in this file |
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

## 7. One term not adopted, because it was not understood

The source checklist twice refers to **MISCIS** ("do not expand MISCIS merely to make a test pass";
"Kernel changes / MISCIS boundary review"). The term appears nowhere in this project and was not
guessed at. Both instances have been read as the KERNEL SCOPE boundary - `CLAUDE.md` 4.4's anti-scope
and 26.10's mechanism-not-policy rule - which is what the surrounding sentences are about. If it
means something else, this section is where to correct it.
