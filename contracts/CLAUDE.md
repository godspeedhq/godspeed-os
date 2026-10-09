# contracts/

Service contracts and JSON Schema (§13.5). Every service declares what it needs; the OS decides whether to grant it.

## Files

| File                          | Purpose |
|-------------------------------|---------|
| `schema/service.schema.json`  | JSON Schema (draft 2020-12) for every `contracts/<name>.toml` contract file |

There are no contracts in this directory. Each crate carries its own, at
`services/<name>/contracts/<name>.toml` or `examples/<name>/contracts/<name>.toml`.

A contract is a build-time DECLARATION. The kernel never reads it: a service's authority comes from
the supervisor's spawn request (CLAUDE.md §13.6). `scripts/contract_check.py` reconciles a service
contract's memory limit, placement core, `ipc_send`, `hw_device`/`hw_pci_*` and `resource_mint`
against that spawn row, and requires `log_write` of a crate that prints through `godspeed::io`;
`IV-contract-authority` in `scripts/commandments.py` reconciles `service_control`, `resource_mint`,
`pci_cfg` and `cpu_clock` against what is actually granted.

## Contract format (§13.1)

```toml
name    = "ping"
version = "0.1.0"

[resources.memory]
request = "32MiB"   # minimum needed to start
limit   = "64MiB"   # maximum permitted

[capabilities]
ipc_send    = ["pong"]
ipc_receive = ["ping"]
log_write   = true

[placement]
core = 0    # optional; omit for round-robin
```

## What the schema validates (§13.4)

- Required fields: `name`, `version`, `resources.memory.request`, `resources.memory.limit`, and a
  `[capabilities]` table (which may be empty).
- Name format: lowercase alphanumeric + hyphens.
- Version: SemVer `N.N.N`.
- Memory sizes: `NNN(KiB|MiB|GiB)`.
- Capability names: known set only (no arbitrary keys).
- Core IDs: integer 0-15 (range check only; availability is runtime).

## What the schema does NOT validate (§13.4)

- Behavioral correctness.
- That the binary actually uses only declared caps.
- That the memory limit is reasonable.
- That the requested core will be available at spawn time.

Build-time validation is structural, not behavioral. Behavioral enforcement is runtime-only (§13.4).

## Running validation

```bash
osdev validate
```

Runs against every `.toml` directly inside any `contracts/` directory in the tree (up to five levels
deep, skipping `target/` and `build/`). It is on no developer build path: `osdev`'s `EXTRA_CHECKS`
does not include it, and the only workflow that runs it, `build.yml`, is `workflow_dispatch`-only and
paused. Run it by hand after touching a contract or this schema.

## Placement field semantics (§13.2)

| Contract field            | Supervisor behavior |
|---------------------------|---------------------|
| `[placement]` omitted     | Round-robin across ready cores |
| `[placement] core = N`    | Requires exactly core N; fails with `PlacementInvalid` if core N unavailable |

Strict semantics: a named core is a deployment-intent statement. The supervisor never silently reroutes to a different core.

## Schema versioning

The schema is versioned via `$id`. Breaking changes (removing a required field, changing a type) require a major version bump and a documented migration path. Additive changes (new optional capability type) are minor version bumps.
