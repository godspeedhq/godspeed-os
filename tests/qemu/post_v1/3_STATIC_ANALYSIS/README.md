# Static Analysis Evidence

This directory collects CI artefacts from the `static-analysis` workflow job.

| File | Contents |
|------|----------|
| `cargo-audit.txt` | Output of `cargo audit` (appended per run by CI) |
| `miri.txt` | Output of `cargo miri test -p kernel --lib` |
| `cargo-geiger.txt` | Output of `cargo geiger --package kernel` |

All three are written by CI on each run. The files here are the most-recent local
captures if you ran the tools manually. CI results live in the GitHub Actions log.

To reproduce locally (Linux / WSL):

```sh
cargo audit
cargo miri test -p kernel --lib
cargo geiger --package kernel
```

On Windows, miri requires WSL or a Linux CI runner; it does not support the
`x86_64-pc-windows-msvc` host target for `no_std` test compilation.
