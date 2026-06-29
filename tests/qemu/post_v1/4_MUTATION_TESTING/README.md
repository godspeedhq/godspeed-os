# Mutation Testing Evidence

This directory collects artefacts from the `mutation` workflow job.

| File | Contents |
|------|----------|
| `mutation-report.txt` | Full stdout/stderr from `cargo mutants` |
| `mutants.out/` | cargo-mutants output directory: `missed.txt`, `caught.txt`, `timeout.txt`, `outcomes.json` |

CI uploads these as a named artefact (`mutation-report-<sha>`) on every run.
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

```sh
cd kernel
cargo mutants -- --lib
```

Results appear in `kernel/mutants.out/`.
