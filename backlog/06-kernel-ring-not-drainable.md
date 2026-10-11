# 6. No syscall exposes the kernel's 16 KiB log ring to userspace

**Status: OPEN, re-verified 2026-09-20.** `drain_kernel_ring_buffer` is still a no-op whose body
is two comments (`sdk/rust/src/service_context.rs`), so `events log` still begins when `events`
does and everything logged before that is on serial alone.

*(2026-10-09: the BOOT half is answered another way. Since 2026-10-08 the kernel keeps a fixed copy of
the first 32 KiB ever logged, read by copy through InspectKernel query 27 and shown as `events log
boot` (CLAUDE.md 11.4). The ring itself is still not drainable, and `drain_kernel_ring_buffer` is
still the stub, with no caller.)*

*(2026-10-10: the stub is gone - `backlog/80` S7 deleted `drain_kernel_ring_buffer` and its twin
`recv_log_message`, which did nothing and had no caller. What this item records is unchanged: no
syscall drains the ring.)*

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
