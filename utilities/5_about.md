# Utility: `about`

**Utility:** `about` — system identity and credits
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`about` answers **what is this system?** — a one-shot identity card: what the OS is,
how many cores it is running on, and who made it.

## 2. Invocation

| Command | Meaning |
|---|---|
| `about` | Print the identity block and return. |

## 3. Output

```
gs> about
GodspeedOS: a capability-based microkernel (v1 milestone)
  running on 4 core(s)
  Created by Bankole Ogundero.
```

ASCII only — the framebuffer console's font is ASCII, so no em-dashes or other
non-ASCII glyphs (they render blank on the TV).

## 4. Data source

Static identity text, plus `inspect_core_count()` (`InspectKernel` query 8) for the
core count.

## 5. Capabilities

- **`INTROSPECT`** (READ) — for the core count (query 8); the shell holds it.
- **Console output** to print the block.

## 6. Non-goals

- **No version/build metadata sprawl.** `about` is an identity card, not a build
  manifest. Per-utility versions are reported by each utility's own `version`
  (`0_conventions.md` §1 rule 5), not aggregated here.

## 7. Conformance

Built-in: no `about help` / `about version` yet; listed by the shell's top-level
`help` under **System**. See `0_conventions.md` §3.
