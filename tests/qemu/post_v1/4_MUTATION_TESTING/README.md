# Mutation Testing Evidence

Where to keep artefacts from the `mutation` workflow (`.github/workflows/mutation.yml`, `workflow_dispatch`-only,
paused). None is in this directory today.

| File | Contents |
|------|----------|
| `mutation-report.txt` | Full stdout/stderr from `cargo mutants` |
| `mutants.out/` | cargo-mutants output directory: `missed.txt`, `caught.txt`, `timeout.txt`, `outcomes.json` |

Each run of that workflow uploads these as a named artefact (`mutation-report-<sha>`).
Download from the GitHub Actions "Artifacts" tab.

## Interpreting outcomes.json

```json
{
  "missed":  [...],   // mutants the tests did NOT catch - fix these
  "caught":  [...],   // mutants the tests caught - evidence of coverage
  "timeout": [...],   // mutants that timed out - usually slow compile paths
  "unviable": [...]   // mutants that didn't compile - not informative
}
```

Kill rate = `caught / (caught + missed)` × 100. Target: ≥ 80%.

## Running locally (Linux / WSL)

The workflow's own command, from the repository root (`kernel/mutants.toml` scopes the mutations; without
`--config` it is not found and every mutant runs):

```sh
cargo mutants --package kernel --config kernel/mutants.toml -- --lib
```

Results appear in `mutants.out/`.
