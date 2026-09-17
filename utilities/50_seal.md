# Utility: `seal` - freeze a file's content, permanently

**Status:** Built. Verified in QEMU by `osdev test fs-time` (15/0), which seals a file, proves the
write is refused, **reboots the machine**, and proves it is still refused and the content is still
the original.

    seal <path> [yes]

Freeze a file's content. After this the file can be read, listed, renamed, moved and deleted - but
its bytes can never change again.

It asks `[y/N]` first, because **there is no unseal.** `seal <path> yes` skips the prompt - a
confirm reads the console, so a script cannot answer one, and the warning still prints either way.
What `yes` buys is not silence, it is the ability to be automated.

## Why there is no unseal

A seal a holder can lift is not a guarantee, it is a request. The entire value of the flag is that
nothing short of deleting the file undoes it: an auditor reading a sealed file does not have to
establish who had write access in the meantime, because nobody did.

That is also why `seal` is the only command here that asks before acting on a single file. Every
other one can be undone by doing the opposite.

## What it does NOT promise

Stated plainly, because a security feature that is vague about its edges is worse than one that is
narrow and clear:

- **It does not stop deletion.** A seal freezes CONTENT, not existence, and deleting a file needs
  authority over its parent directory rather than over the file. Refusing deletion would make a
  sealed file unremovable, so a disk could be filled with rubbish nobody is permitted to clear - a
  denial of service bought with a guarantee nobody asked for.
- **It does not stop renaming or moving.** The bytes are what is frozen. Archiving a sealed log into
  another directory is a reasonable thing to want, and it changes nothing about the content.
- **It is not encryption.** A sealed file is as readable as any other, to anyone who can read the
  disk.

## Output

```
gsh> seal /audit.log
seal /audit.log - its content can NEVER be changed again, and there is no unseal.
 Seal it? [y/N]: y
sealed /audit.log

gsh> write /audit.log tampered
write: failed

gsh> ls long /
  NAME                  TYPE        SIZE  MODIFIED
  audit.log             seal        812  2026-09-17 07:33
```

A sealed file shows as **`seal`** in the TYPE column of `ls long`, not as a marker beside `file`. It
is less an attribute of a file than a different kind of thing to have on a disk - one you cannot
change - and burying that in a suffix makes it easy to miss exactly when it matters.

## How it is enforced

Three refusals, and the first is the one that matters:

1. **`fs` will not mint a writable capability to a sealed file.** Refused at `open`, not at each
   write, because handing out a capability that looks writable and fails on use is a worse answer
   than refusing plainly - and §7.3 says rights narrow, so a capability that cannot be honoured
   should never exist.
2. Every write path (`write`, the streaming `write_at`, the journaled variant) refuses, because the
   seal is carried on the `Entry` that each of them walks to. A new write route cannot be added later
   that forgets to ask.
3. The volume records a **`ro_compat` feature bit** the first time anything is sealed, so a build
   that does not know about seals mounts the whole volume READ-ONLY (§6.15) rather than writing
   through a flag it cannot see.

## Where the bit lives, and what that costs

The 64-byte directory record was full, so the flag rides **the top bit of the 64-bit size** - room no
file can reach, since 2^63 bytes is eight exabytes. Every size read goes through an accessor that
masks it off, which is not tidiness: `write_at` bounds a fragmented file's extent by its size, so the
flag leaking into that arithmetic would let a write run past the file's own blocks.

The one cost is that a build which does not know this feature would display a nonsense size for a
sealed entry. That is why the `ro_compat` bit exists: such a build mounts read-only, so it can
neither act on the wrong number nor change anything. Full reasoning in `docs/gsfs-next.md` §3.

## Conventions

Obeys `utilities/0_conventions.md`: `seal help` prints usage, the argument is a path (so `seal` is
NOT in `NO_PATH_CMDS` and Tab completes paths), raw facts without editorialising, and the confirm
uses the same line-edited prompt as `drives flash` - backspace works, and the decision is the final
line.
