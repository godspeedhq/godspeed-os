# Static Analysis Evidence

Where to keep captures of the three tools the `static-analysis` job in `.github/workflows/build.yml` runs.
That workflow is `workflow_dispatch`-only (paused), and the job writes no files: its output is in the
GitHub Actions log. None of the files below is in this directory today.

| File | Contents |
|------|----------|
| `cargo-audit.txt` | Output of `cargo audit` (appended per run by CI) |
| `miri.txt` | Output of `cargo miri test -p kernel --lib` |
| `cargo-geiger.txt` | Output of `cargo geiger --package kernel` |

Save a local capture here under these names if you run the tools by hand.

To reproduce locally (Linux / WSL):

```sh
cargo audit
cargo miri test -p kernel --lib
cargo geiger --package kernel
```

On Windows, miri requires WSL or a Linux CI runner; it does not support the
`x86_64-pc-windows-msvc` host target for `no_std` test compilation.
