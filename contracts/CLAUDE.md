# contracts/

JSON Schema for service contracts (§13.5).

## Files

| File                      | Purpose |
|---------------------------|---------|
| `schema/service.schema.json` | JSON Schema (draft 2020-12) for all `service.toml` files |

## What the schema validates (§13.4)

- Required fields: `name`, `version`, `resources.memory.request`, `resources.memory.limit`.
- Name format: lowercase alphanumeric + hyphens.
- Version: SemVer `N.N.N`.
- Memory sizes: `NNN(KiB|MiB|GiB)`.
- Capability names: known set only (no arbitrary keys).
- Core IDs: integer 0–15 (range check only; availability is runtime).

## What the schema does NOT validate (§13.4)

- Behavioral correctness.
- That the binary actually uses only declared caps.
- That the memory limit is reasonable.
- That the requested core will be available at spawn time.

## Running validation

```
osdev validate
```

This runs against every `contracts/*.toml` file found in the repo. CI must pass `osdev validate` before any PR is merged.

## Schema versioning

The schema is versioned via `$id`. Breaking changes (removing a required field, changing a type) require a major version bump and a documented migration path. Additive changes (new optional capability type) are minor version bumps.
