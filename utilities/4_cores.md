# Utility: `cores`

**Utility:** `cores` - CPU core count
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`cores` answers **how many cores came up?** - the number of CPUs the kernel brought
online at boot (§9, §11.2). SMP is static in v1, so this is fixed for the system's
lifetime. `cores ticks` adds one diagnostic: how fast each core's scheduler is
ticking.

## 2. Invocation

| Command | Meaning |
|---|---|
| `cores` | Print the ready core count and return. |
| `cores ticks` | Sample each core's quantum count for 5 s (paced by the wall clock) and print quanta per second and the raw sampled count per core. |

## 3. Output

```
gsh> cores
cores: 4
gsh> cores ticks
sampling 5s (RTC-paced)...
core  quanta/s  sampled   (quantum = timer tick OR yield)
  C0     100     500   (BSP - keeps the normal period)
  C1     100     500
```

The `ticks` numbers above are illustrative. A quantum is a timer tick or a yield, so
an idle core that slows its timer can show well under 100/s. At most 16 cores are
sampled.

## 4. Data source

`inspect_core_count()` → `InspectKernel` query 8 (`smp::core::ready_count()`).
`cores ticks` reads `inspect_core_total_ticks(core)` → `InspectKernel` query 7
(`scheduler::core_total_ticks`) before and after the sample.

## 5. Capabilities

- **`INTROSPECT`** (READ) - queries 7 and 8 are gated; the shell holds the cap.
- **Console output** to print the line.

## 6. Non-goals

- **No per-core detail.** Per-core CPU% and placement live in `observe` /
  `status`, not here. `cores` is the headline count only.

## 7. Conformance

Conforms: own `cores help` / `cores version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **System**. See `0_conventions.md` §3.
