# Utility: `churn` - keep the filesystem in constant motion, so a power cut lands somewhere

**Status:** Built. Verified in QEMU by `osdev test fs-churn`, which runs it, kills the machine
mid-churn, and checks the volume comes back consistent.

    churn <seconds>     write, rename and delete continuously, then stop and report
    churn verify        after a cut: is any file a MIX of two writes?
    churn tear          deliberately make one file a mix, to prove `verify` can say so
    churn reset         remove /churn and its files

Press `q` to quit a run early.

## `churn tear` - proving the detector can fire

`churn verify` has reported `NONE torn` on every hardware run there has ever been. That is the right
answer, and it says nothing at all about whether the detector *could* say otherwise. **A check never
observed failing is not evidence.**

Every other corruption test in this project - `fs-corrupt`, `fs-scrub`, `fs-hostile` - damages the
disk **host-side, before boot**, which is not available on a machine whose disk you cannot take out.
`churn tear` is the same proof reachable from the shell:

```
gsh> churn tear
churn tear: /churn/f0.bin now holds generation 12 from byte 0 and generation 49 from byte 508 - a MIX
churn tear: the blocks are well-formed and their CRCs are correct, so `drives check`
churn tear: and `drives scrub` will BOTH still report clean. Only `churn verify` sees it.
churn tear: nothing repairs this - detection is the guarantee. `churn reset` removes it.
```

### Why it is called `tear` and not `corrupt`

Because it is not corruption, and the difference is the entire point. It writes a **well-formed
block of a different generation**: every CRC is correct, the tree is correct, the accounting is
correct. `drives scrub` reports `0 bad` afterwards, truthfully. What the file has is a **tear** - a
mix of two writes - which is the word this whole programme already uses (`fs-tear`, "torn write",
and `verify`'s own `TORN`). Calling it corruption would name it after a thing it deliberately is not.

### Nothing repairs it, and `drives check` least of all

The obvious next sentence to write would be "run `drives check` to fix it". That would be wrong
twice over: `drives check` **cannot** fix it, and it cannot even **see** it. It validates structure -
tree, bitmap, free count - and the structure is immaculate.

That is not a limitation, it is the design. `gsfs-carnage.md` §6 puts repair explicitly out of
scope: *detection is the guarantee; silent repair of data whose correct value is unknown is the
second half of the mission statement.* The accounting **can** be rebuilt because the free bitmap is
a derived view of one irreducible source, the tree (`CLAUDE.md` §26.4). File content has no second
source, so there is nothing to rebuild it from. A filesystem that "fixed" this would be inventing
bytes and calling them yours.

`churn reset` removes the torn file when you are finished looking at it.

### It does not ask [y/N], deliberately

`seal` asks because there is no unseal; `drives flash` asks because it erases a disk. `churn tear`
can only ever write to `/churn/fN.bin` - files `churn` itself created, in a directory `churn reset`
exists to delete. It cannot reach anything else, so a confirm would be ceremony rather than a guard.

Pinned by `osdev test fs-tear-detect`, which asserts the interesting half: `churn verify` says TORN
**while `drives check` and `drives scrub` both still say clean**. That is the demonstration that the
two questions below are genuinely different, rather than the assertion of it.

## The two questions after a power cut, and why both are needed

`drives check` validates **structure** - the tree, the bitmap, the CRCs.
`churn verify` validates **content**.

They are not the same question, and the second had no answer until now. A file holding the first half
of one write and the second half of another has perfectly valid block CRCs (each block was written
whole), sits in a perfectly valid directory, and occupies correctly accounted blocks. Every check this
project had would pass it.

**How verify knows.** Every byte churn writes encodes the generation that wrote it:
`byte[k] = (gen + k) mod 251`. So one read decides it - take the generation from byte 0, and every
later byte is predicted. The first disagreement is the tear point:

```
gsh> churn verify
churn verify: TORN - /churn/f7.bin diverges at byte 1216 of 3000 (block 2, offset 200 within it)
churn verify: 1 of 8 file(s) are TORN - each holds a mix of two writes.
```

251 is the largest prime under 256, chosen so the pattern does not align with the 508-byte block
payload. A tear on a block boundary therefore still lands mid-pattern and stays visible, rather than
looking like a continuation.

**It has been seen to fire.** A file was corrupted mid-way host-side and the block CRC re-stamped, so
the damage was structurally invisible. `drives check` reported `0 bad, consistent` on that disk;
verify named the exact byte. A detector nobody has watched fail is not evidence.

## Why it exists

A power cut is only interesting if it lands somewhere interesting, and by default it does not.

Three real cuts on a Dell Wyse, pulled at the wall partway through a `selfcheck` run, produced three
clean mounts and **not one `journal recovered` line**. Nothing was wrong: the window between a commit
record becoming durable and the last home block being written is sub-millisecond, and a human with a
plug samples a fraction of a percent of a run. The filesystem was never asked to recover, so nothing
was learned about whether it can.

There are two ways to fix that, and they answer different questions:

| | what it does | what it answers |
|---|---|---|
| `write /cutme.txt ...` (the `crash-window` test build) | holds ONE known window open for ten seconds | **proves** the recovery path works |
| `churn <seconds>` | runs thousands of transactions of every shape for as long as you ask | **searches** for the windows nobody thought to aim at |

A proof and a search. Maximum carnage wants both.

**And `churn` runs on a shipping build**, with no test feature compiled in. That matters more than it
looks: a fault that appears only in a build nobody ships is a fault about that build.

## What it actually does

A fixed rotation of eight files in `/churn`, rewritten in place, so the volume **never fills** however
long it runs. Four payload sizes - 64, 500, 1200 and 3000 bytes - because the allocator takes
different paths for a single data block, a partial extent and several blocks, and a one-size churn
would only ever exercise one of them.

Every fifth iteration it **renames the file and then deletes it**, rather than only overwriting.
Overwrites exercise one journal path; renames and deletes move directory entries and free extents,
which is where the interesting interrupted states live. The torn-write sweep found its only real
finding in a delete, not in a write.

The payload varies per iteration rather than being a block of one repeated byte, so if a torn write
ever does surface the bytes say which iteration wrote them.

## Using it

```
gsh> churn 60
churn: writing continuously for 60s - CUT THE POWER AT ANY POINT [q] quit
churn: 1s elapsed, 47 writes
churn: 2s elapsed, 95 writes
...
```

Pull the plug at any point. On the way back up:

```
gsh> drives check
```

**What the answer means**, from the permitted-outcome table in `docs/gsfs-carnage.md`:

- `nothing was repaired` - clean. The journal did its job, or the cut fell between transactions.
- `journal recovered N block(s) from an interrupted write` in the boot log - **the case worth
  hunting**. The cut landed in the window and recovery ran.
- `REPAIRED ... held as used but are unreachable - a leak` - **permitted.** Wasted blocks, reconciled,
  no data at risk. QEMU produces exactly this at three of twenty-one tear points on a delete.
- `REPAIRED ... marked free but are IN USE` - **stop and report.** Blocks belonging to a live file
  were handed back; the next allocation would overwrite them. Never yet observed.
- A volume that will not mount, or `N bad` - stop and send the serial log.

## What it is not

**Not a benchmark.** It reports writes and bytes because they say whether it was actually working, not
because the number means anything comparable. Throughput here is dominated by the serial log and the
journal barriers, so a bigger number is not a better disk.

**Not a soak test for the rest of the system.** It hammers storage and nothing else. `chaos` is the
utility for killing services; this one never kills anything.

**It leaves `/churn` behind, deliberately - after a cut AND after a clean finish.** `churn reset`
removes it when you want it gone.

Not automatic, and the reasoning is worth stating because the opposite looks tidier. Those files are
the evidence: after a cut they are what `churn verify` reads. A run that ends normally is
indistinguishable from one somebody walked away from, so auto-deleting would mean coming back to find
the thing you meant to examine gone. And the set is bounded at eight files rewritten in place, so
nothing accumulates however many times it runs - there is no hygiene problem to solve. Deleting data
because a command reached its end is the kind of silent helpfulness this project avoids.

## No size or path parameters

Asked for, and declined for now (26.2 - a feature is pulled into existence, not anticipated).

The four sizes are not arbitrary: 64, 500, 1200 and 3000 bytes span a sub-block write, a single block,
a partial extent and a multi-block extent, because the allocator takes a different path for each. A
size parameter would let a run NARROW that, which is the opposite of what carnage wants by default.

A path parameter matters when multi-drive ships and not before. If a specific test needs either, it
arrives then with a reason attached.

## Conventions

Obeys `utilities/0_conventions.md`: `churn help` and `churn version`, a word-not-flag argument, raw
facts without editorialising, and `q` aborts (rule 9) - a command that runs for a minute and cannot be
interrupted is one the operator has to reboot out of. Bounded (26.6): a fixed file rotation and fixed
stack buffers, no heap (26.6.1).

The wording is **`[q] quit`**, matching `observe` and `ping` - the house phrasing for a CONTINUOUS
run - and now the ONLY form. One key, one word, because the letter IS the mnemonic, which is the
whole reason the key is `q`. "abort" would have earned the letter `a` and never had it, yet the shell
advertised `(press q to abort)` in six places and rule 9 mandated it. Both corrected. Unix makes the
same association from the other direction: ctrl+C says "cancel".
