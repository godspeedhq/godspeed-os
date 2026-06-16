# Utility: `mem`

**Utility:** `mem` — physical memory usage
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`mem` answers **how much physical RAM is in use?** — one line, system-wide. It
reports raw facts and renders no verdict (`0_conventions.md` §1 rule 7).

## 2. Invocation

| Command | Meaning |
|---|---|
| `mem` | Print one physical-memory summary line and return. |

## 3. Output

```
gsh> mem
mem: 2048 KiB used / 4096 MiB total (0.04% used, 4095 MiB free)
```

The percentage is computed in hundredths with integer math, so the microkernel's
tiny footprint shows as e.g. `0.04%` rather than rounding to `0%`.

## 4. Data source

Frames are 4 KiB pages, from the kernel frame allocator via introspection:
- `inspect_kernel_total_frames()` — total frames (× 4 KiB = total).
- `inspect_kernel_free_frames()` — free frames.
- used = total − free; KiB = frames × 4; MiB = frames ÷ 256.

## 5. Capabilities

- **`INTROSPECT`** (READ) — the frame-count queries are gated (`InspectKernel`
  4/5). The shell holds this cap, so the built-in can call them.
- **Console output** to print the line.

## 6. Non-goals

- **No per-process memory.** That is the job of `status` (per-task MEM column) and
  `observe`. `mem` is the system total only.
- **No history or graphing.** Point-in-time.

## 7. Conformance

Conforms: own `mem help` / `mem version` (with a real example, per `0_conventions.md`); listed by the shell's top-level `help`
under **System**. See `0_conventions.md` §3.
