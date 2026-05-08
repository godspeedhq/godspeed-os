# Unsafe Audit (§18.4)

CI verifies this file matches all `unsafe` blocks in `kernel/src/`. Every entry must include: file path, line number, and the SAFETY argument.

## Format

```
### path/to/file.rs:NN
**SAFETY:** <argument for why this is sound>
```

---

## Current inventory

This file is populated as `unsafe` blocks are implemented. Until then it is intentionally empty. CI will fail if the source contains `unsafe` blocks not listed here.

When you add an `unsafe` block:
1. Add a `// SAFETY: <argument>` comment in the source.
2. Add an entry to this file in the same commit.
3. CI checks both; missing either causes failure.
