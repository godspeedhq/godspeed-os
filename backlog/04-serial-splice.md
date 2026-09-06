# 4. The kernel splices one log line into another under load

**Severity:** observability - and it corrupts the evidence used to debug everything else.
**Seen on:** arm32, x86_64, and aarch64. Not arch-specific.

## The fingerprint

Two log lines interleaved into one, mid-token:

```
PASS  caps hw-enumerator | assert lacks service_controlnet-stack: `time` asked for the clock - no route
selfchfs: block-driver did not report capacity within 20s - coming up storage-unavailable
selfchselfcheck
```

The second line begins inside the first, with no newline between them.

## Why it matters more than it looks

It has produced **both false FAILs and false PASSes** in the test harness: a spliced line can break
an assertion that would have passed, and it can also carry a substring that satisfies one that
should have failed. During the events-service work an entire diagnosis was nearly built on a
spliced line, and a run reported "A6 PASSED" on a corrupted line.

This is the tool used to debug every other item in this folder, so its integrity is load-bearing.

## What is known

- `kprintln!` uses a kernel-side `SpinLock` plus a synchronous UART write. Under load, multiple
  cores contend for the lock and busy-wait on the UART FIFO.
- The harness can NAME a splice when it sees one (added during the arm32 work), so occurrences are
  detectable after the fact. The kernel side is untouched.
- It survives on a single core too (`selfchselfcheck` above is from an `--smp 1` boot), so it is not
  purely cross-core contention.

## Next step

`services/events/CLAUDE.md` already carries a proposed design under "TODO: per-core kernel log
buffers": static per-core ring buffers in BSS, `kprintln!` writes only the calling core's own buffer
(SPSC, no lock), a drain syscall, and a panic path that bypasses the buffer. ~200-300 lines across
`kernel/src/log.rs`, `syscall/dispatch.rs` and the sink service.

That is kernel growth for a diagnostic, so it needs the 26.2 test applied honestly first: is this
pulled into existence by a real operational problem? Three sessions of corrupted evidence is an
argument that it is.
