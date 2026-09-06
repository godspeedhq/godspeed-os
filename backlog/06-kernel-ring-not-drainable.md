# 6. No syscall exposes the kernel's 16 KiB log ring to userspace

**Severity:** feature. A known, recorded gap - not a defect.

## What it costs today

`ctx.drain_kernel_ring_buffer()` is a **no-op stub**. The consequence is stated in
`services/events/CLAUDE.md`: **`events log` begins when `events` does.** Anything logged before that
service exists - the whole of boot, and every line from a service that started earlier - is on
serial only.

For a machine with a serial cable that is a non-issue. For a Pi wired to a TV it means the boot
sequence is unreadable after the fact.

## Why it is not built

Draining it needs a new `InspectKernel` query, which is kernel growth for a diagnostic. 26.2 says a
feature is added when a real operational problem requires it, and so far the serial console has
always been available when it mattered.

## What would change that

If a machine ever needs post-hoc boot diagnosis with no serial attached - a bare-metal Pi failure a
user has to report from the screen alone - this moves from convenience to necessity.

Note it interacts with item 4: if per-core kernel log buffers land, the drain syscall is part of
that design anyway, and this item is absorbed by it rather than built separately.
