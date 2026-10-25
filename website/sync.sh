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

echo "==> [2/3] rustdoc - the API reference: the standard library AND the SDK (-> /api)"
# BOTH crates. `godspeed` (the standard library) is what an ordinary program is written against and
# is the surface the Stranger Test (CLAUDE.md 22.7) measures; documenting only `godspeed-sdk`
# published the layer underneath and left the public one unreachable. See backlog/fixed/45.
doc=""
if cargo doc -p godspeed -p godspeed-sdk --no-deps --target x86_64-unknown-none; then
  doc="target/x86_64-unknown-none/doc"
elif cargo doc -p godspeed -p godspeed-sdk --no-deps; then
  doc="target/doc"
fi

echo "==> [3/3] assemble the site (book + /api)"
if [ -n "$doc" ] && [ -d "$doc" ]; then
  mkdir -p website/book/api && cp -r "$doc"/* website/book/api/
  # rustdoc writes a page per crate and none above them, so /api/ itself was a 404. A page that
  # names both, unless a later rustdoc writes its own.
  if [ ! -e website/book/api/index.html ]; then
    cat > website/book/api/index.html <<'HTML'
<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>GodspeedOS API reference</title></head>
<body style="font-family: sans-serif; max-width: 40em; margin: 2em auto; padding: 0 1em; line-height: 1.5">
<h1>GodspeedOS API reference</h1>
<ul>
<li><a href="godspeed/index.html"><code>godspeed</code></a>, the standard library: what a program is written against.</li>
<li><a href="godspeed_sdk/index.html"><code>godspeed_sdk</code></a>, the SDK underneath it: what the kernel can do.</li>
</ul>
<p><a href="../stdlib.html">The standard library, explained</a></p>
</body></html>
HTML
  fi
  echo "    ready: website/book/  (with /api)"
else
  echo "    rustdoc not produced - shipping the book WITHOUT /api (same best-effort as the workflow)"
fi

echo "==> done. Preview:  cd website && mdbook serve   ->   http://localhost:3000"
