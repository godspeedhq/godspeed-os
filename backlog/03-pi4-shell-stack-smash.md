# 3. Pi 4: the shell faults with a return address of ASCII spaces

**Severity:** correctness. Costs one command, never the machine - the supervisor respawns a fresh
prompt, which is 6.2 working.
**Sightings:** three. Most recent 2026-09-06 @ f91663e8, mid-`selfcheck`, right after
`chaos max-carnage 100`.

## The fingerprint

```
*** aarch64 EXCEPTION: LowerEL/A64 Synchronous
    ESR_EL1  = 0x82000004  (instruction abort, lower EL)
    ELR_EL1  = 0x2020202020202020        <- returned to ASCII spaces
    x19 = x20 = x21 = x29 = x30 = 0x2020202020202020
    x22..x28 INTACT
    task = shell
faulted at: > mkdir /sc/x/y/z parents
```

Five callee-saved registers, contiguous in the spill area, and the rest untouched: a **~40-byte
overwrite at the bottom of one frame**, i.e. a callee writing past its own frame upward. Not a wide
blast, not exhaustion.

## Ruled out - do not re-derive these

- **The canary reading is a RED HERRING.** SP (`0x7ffed910`) sits BELOW the canary VAs
  (`0x7fff7f18`), so the stack had legitimately grown past that region and its contents are live
  frames. A destroyed canary there means nothing. (This trap was already recorded once and was
  nearly walked into a second time.)
- **Not stack exhaustion:** 64 pages mapped, SP ~74 KiB below the top.
- **Not DMA:** `0 INSIDE a DMA reservation; nearest arena is 1 frame away`.
- **Not shell code.** Services are `unsafe`-free (18.2, enforced by `scripts/unsafe_check.py`) and
  Rust bounds-checks every array write, so a shell buffer overrun would PANIC, not smash. The
  writer cannot be safe Rust.
- **Not the SDK's kernel->user buffers.** Every `as_mut_ptr()` site audited: Recv/TryRecv (2/34)
  use a local `[0u8; MAX_PAYLOAD]` and pass MAX_PAYLOAD; RecvTimeout (35) packs `buf.len()`; Call
  (41) passes `recv_len`; CallDeadline (50) passes a power-of-two cap class; `net_frame_rx` (43)
  passes `dst.len()`; `net_info` (44) takes `&mut [u8; 7]`; `usb_disk_read` (47) takes
  `&mut [u8; 512]`; task_stat (16) passes 80.
- **No SDK/kernel size mismatch:** `MAX_PAYLOAD` (4096) == `MAX_MESSAGE_SIZE` (4096).

So the writer is unsafe code OUTSIDE the audited SDK buffer paths - kernel-side, most likely a
kernel->user write whose destination is not the caller's declared buffer.

## Why it has not been fixed

**Local reproduction is impractical.** Pi 4 under QEMU TCG runs ~20x slower than wall time; a 300 s
window did not reach `gsh>`, let alone chaos plus selfcheck. Iterating needs hardware or an
instrument.

## Next step (costs a kernel change, so it widens the test surface)

The kernel already counts kernel->user writes - the fault dump prints
`kernel->user writes: 587198 totalling 51745315 bytes; largest 3581 bytes to 0x7fffef78`. Extend it
to record the ADDRESS RANGE of the last N writes per task and flag any landing outside the buffer
the syscall was handed. That attributes the write at the moment it happens rather than at the
epilogue, and turns a silent smash into a loud refusal (26.7) - the same shape as the CallDeadline
cap-class fix.

Two hardware round-trips: one to identify, one to fix.
