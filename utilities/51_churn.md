# Utility: `churn` - keep the filesystem in constant motion, so a power cut lands somewhere

**Status:** Built. Verified in QEMU by `osdev test fs-churn`, which runs it, kills the machine
mid-churn, and checks the volume comes back consistent.

    churn <seconds>

Write, rename and delete continuously for `<seconds>`, then stop and report. Press `q` to stop early.

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
churn: writing continuously for 60s - CUT THE POWER AT ANY POINT (q to stop)
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

**It leaves `/churn` behind after a cut**, deliberately. Those files are the evidence, and `drives
check` walks them. Delete the directory when you are done with it.

## Conventions

Obeys `utilities/0_conventions.md`: `churn help` and `churn version`, a word-not-flag argument, raw
facts without editorialising, and `q` aborts (rule 9) - a command that runs for a minute and cannot be
interrupted is one the operator has to reboot out of. Bounded (26.6): a fixed file rotation and fixed
stack buffers, no heap (26.6.1).
