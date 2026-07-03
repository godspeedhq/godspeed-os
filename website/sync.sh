#!/usr/bin/env bash
# sync.sh - regenerate the FULL GodspeedOS documentation site locally, in ONE command:
# both the mdBook narrative (pulled from the {{#include}} source-of-truth stubs - docs/,
# CLAUDE.md, COMMANDMENTS.md, the ALMANAC, ...) AND the SDK rustdoc (/api). It mirrors the
# `docs` GitHub Action (.github/workflows/pages.yml), so what you preview locally is what
# gets deployed. Output: website/book/ (with /api). Run from anywhere.
#
#   bash website/sync.sh          # (on Windows, from Git Bash)
#   ./website/sync.sh             # if executable
#
# Needs: mdbook (cargo install mdbook --version 0.4.40) and python3 (for the link-fixup
# preprocessor). rustdoc is best-effort - if it can't build, the book still ships (minus /api).
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

echo "==> [1/3] mdBook - the narrative site (includes pull docs/, CLAUDE.md, the almanac, ...)"
( cd website && mdbook build )

echo "==> [2/3] rustdoc - the SDK API reference (-> /api)"
doc=""
if cargo doc -p godspeed-sdk --no-deps --target x86_64-unknown-none; then
  doc="target/x86_64-unknown-none/doc"
elif cargo doc -p godspeed-sdk --no-deps; then
  doc="target/doc"
fi

echo "==> [3/3] assemble the site (book + /api)"
if [ -n "$doc" ] && [ -d "$doc" ]; then
  mkdir -p website/book/api && cp -r "$doc"/* website/book/api/
  echo "    ready: website/book/  (with /api)"
else
  echo "    rustdoc not produced - shipping the book WITHOUT /api (same best-effort as the workflow)"
fi

echo "==> done. Preview:  cd website && mdbook serve   ->   http://localhost:3000"
