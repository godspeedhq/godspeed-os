# Utility: `cores`

**Utility:** `cores` — CPU core count
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`cores` answers **how many cores came up?** — the number of CPUs the kernel brought
online at boot (§9, §11.2). SMP is static in v1, so this is fixed for the system's
lifetime.

## 2. Invocation

| Command | Meaning |
|---|---|
| `cores` | Print the ready core count and return. |

## 3. Output

```
gsh> cores
cores: 4
```

## 4. Data source

`inspect_core_count()` → `InspectKernel` query 8 (`smp::core::ready_count()`).

## 5. Capabilities

- **`INTROSPECT`** (READ) — query 8 is gated; the shell holds the cap.
- **Console output** to print the line.

## 6. Non-goals

- **No per-core detail.** Per-core CPU% and placement live in `observe` /
  `status`, not here. `cores` is the headline count only.

## 7. Conformance

Conforms: own `cores help` / `cores version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **System**. See `0_conventions.md` §3.
