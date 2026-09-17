# The `feat/gsfs` hardware pass - what to run, and what would make it fail

**Status: NOT RUN. Everything on this branch is QEMU-validated only.** This is the checklist for the
five boards, written while the QEMU work was fresh so that the reasons behind each step are recorded
rather than reconstructed later.

`docs/gsfs-next.md` says a hardware pass "confirms at the end", and for most of this branch that is
exactly right. **Two items on this list are different: they cannot be answered in QEMU at all**, and
for those the boards are not a confirmation, they are the only instrument. They are marked
**ONLY-ON-HARDWARE**.

## 0. Before touching a board

Build fresh and verify the sweep is green on the machine you are flashing from, because a stale image
is the trap this project has hit most:

```
osdev build                 # 20 commandments, 73 redteam probes, 15 checkers
osdev test fs-all           # 16 suites, roughly 20 minutes
osdev test shell            # 174
osdev test files            # 222
```

Check the build log says `build: <svc> OK` for every service. An image built before the services it
embeds has shipped stale binaries on both Pi ports before, and several "confirmations" tested code
that was not running.

## 1. What this branch changed, and therefore what to look at

Five things touch the disk or the shell, and a hardware pass is looking for the ones QEMU cannot see.

**The on-disk format gained two features** (`docs/persistence.md` 6.17): timestamps in the 60 bytes
every directory block already wasted, and a sealed flag riding the top bit of the size field behind a
`ro_compat` feature bit. The magic is unchanged at `GSFS0008`, so **an existing volume must still
mount**, and this is the first thing to check on a board that has data on it.

**The `LIST_DIR` reply widened twice** - an mtime and then a flags byte - and seven consumers step
through it. In QEMU they all pass; the failure mode if one was missed is reading the next entry's
name out of this one's timestamp, which looks like garbage in a listing rather than a crash.

**`ls` is now `dir`**, `long` and `human` are gone, and `dir bytes` replaces them. `scripts/selfcheck.gsh`
was updated in 34 places, so a selfcheck failure naming a missing command is a rename that was missed.

**`fs` now sends a failure REASON on the wire** and the shell prints it. The reply grew; byte 0 is
unchanged, so an unchanged consumer is unaffected.

**`drives check` now reports whether a repair was needed**, with its direction.

## 2. The pass, per board

Five boards: HP T630 and Dell Wyse 5070 (x86-64, AHCI), Raspberry Pi 2 (ARMv7, USB stick), Raspberry
Pi 4 (AArch64), StarFive VisionFive 2 Lite (RISC-V 64).

### 2.1 Every board: the volume still mounts, and the shell still works

```
drives                      # the volume is seen, sized, and labelled
dir /                       # four columns: NAME TYPE SIZE MODIFIED
dir bytes /                 # the same table with exact byte counts
ls                          # must NOT work - prints: try `dir`
selfcheck                   # the full suite
```

**What would fail:** `selfcheck` naming an unknown command is a missed rename. A listing with garbled
names is a missed `LIST_DIR` consumer. A volume that does not mount is the format change, and that is
the one to stop and report rather than reformat past.

### 2.2 Every board with storage: timestamps and sealing survive a real power cycle

```
write /hw-stamp.txt hello
dir /                       # a real date in MODIFIED, not `unknown`
seal /hw-stamp.txt          # answer y
write /hw-stamp.txt tamper  # must be refused, naming the seal
                            # ---- now power off at the wall, not `reboot` ----
dir /                       # `seal` still in TYPE, same date
read /hw-stamp.txt          # still `hello`
drives check                # 0 bad; says whether anything was repaired
```

**Why a wall power-cut and not `reboot`.** A clean reboot flushes; pulling the power does not. This is
the only step that exercises the durability path the way a user will.

**A pre-existing volume is the better subject** for the first half: a file written before this branch
has no timestamp, and must read `unknown` rather than being given an invented date.

### 2.3 ONLY-ON-HARDWARE: the cross-ISA handoff (`backlog/34`)

**No non-x86 port can attach a usable disk in QEMU**, so this has never been run anywhere.
`build/xisa-x86-written.img` is a 16 MiB GSFS volume written on x86-64, holding `/from-x86.txt`
("written-on-x86-64") and `/shared/note.txt` ("hello-from-intel").

1. Put that image on a USB stick and boot the **Pi 2** from it.
2. `dir /` - both entries present, sizes and dates as written.
3. `read /from-x86.txt` and `read /shared/note.txt` - exact content.
4. `write /from-pi2.txt written-on-armv7` and `seal /shared/note.txt`.
5. Bring the stick back to an x86 board: both original files intact, the new file present, the seal
   still refusing writes.

**What this is really testing.** Not endianness - every field is little-endian by construction. It is
the BLOCK TRANSPORT: AHCI hands `fs` a sector, USB mass storage hands it one through BOT/SCSI over a
split transaction. The filesystem should not care, and "should not" is the phrase this exercise
exists to remove. Every storage guarantee on this branch is currently verified on one architecture
and one transport.

### 2.4 ONLY-ON-HARDWARE: durability where the device will not be ordered

The Pi 2's stick **refuses `SYNCHRONIZE CACHE` outright** (`CLAUDE.md` 6.1). On that board the
crash-recovery guarantee is narrower than everywhere else, and `fs` is supposed to say so once per
mount rather than imply a guarantee it cannot deliver.

```
drives                      # look for the durability warning in the boot log
```

**What would fail:** silence. A backend that cannot be ordered must announce it. QEMU's disks always
flush, so this line has never been seen fire outside that board.

### 2.5 The T630 and the Wyse: the storage stack under chaos

```
chaos kill-storm fs 10
selfcheck
drives check
```

`fs` and `block-driver` are restartable, and the format changes are the first to be exercised across
a restart storm on real silicon. `drives check` afterwards is the part that matters: a storm that
leaves the accounting drifted will now SAY so, where before it would have repaired it in silence.

### 2.6 Pi 4 and VisionFive: no storage, so the shell half only

Neither board has a working disk path today (`backlog/34`). Run 2.1 and confirm the shell, the
renamed command and the listing render correctly on their consoles. A `dir` on a machine with no
filesystem must say storage is unavailable rather than hang - the rule above the rules.

## 3. What a failure is worth

Record what you saw, not what it probably was. The three most valuable findings of this branch were
all cases where the first explanation was wrong - a "starved" net-stack that was not, a test failure
that was the test, and a leak that turned out to be permitted. A serial capture is worth more than a
diagnosis, and `build/serial_output.log` costs nothing to keep.

**Take timings only from lines with a colon** (`fs: ...`). A dump of the `events` ring is
column-padded and its timestamps are the repaint rate, not the event time - that has already nearly
put a wrong hardware figure into a document.

## 4. What this pass does NOT cover

Stated so nobody reads a green pass as more than it is:

- **The carnage programme** (`docs/gsfs-carnage.md`) is a separate effort with its own evidence
  table. Nothing here closes any of its gates except cross-ISA.
- **The torn-write work is QEMU-only and stays that way.** Record-and-replay needs a tap on the write
  path and a way to rebuild the disk between boots; on hardware the equivalent is a real power cut,
  which is 2.2 and is a much blunter instrument.
- **Metadata exhaustion, the block-layer attack and the reference model** are NOT RUN anywhere.
