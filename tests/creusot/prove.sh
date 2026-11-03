#!/usr/bin/env bash
# Prove the kernel functions that carry a Creusot contract (README.md). Linux only: WSL on Windows, or
# verification/Containerfile. Exits non-zero if any proof fails.
#
# Creusot compiles every function in the included files, and proves only those named here: the ones
# whose contract is the point. Every other function in those files is compiled and NOT proved, which
# README.md says in as many words.
set -euo pipefail
cd "$(dirname "$0")"
if [ -d "$HOME/.local/share/creusot" ]; then eval "$(opam env --switch="$HOME/.local/share/creusot")" 2>/dev/null; fi
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/creusot-target}"
cargo creusot --only=coma
proved=(
  capability/rights/impl_Rights/contains
  capability/rights/impl_Rights/narrow
  capability/rights/impl_Rights/without
  capability/rights/impl_Rights/union
  capability/generation/impl_Generation/bump
  ipc/queue/impl_MessageQueue/is_full
  ipc/queue/impl_MessageQueue/is_empty
  ipc/queue/impl_MessageQueue/enqueue
  ipc/queue/impl_MessageQueue/dequeue
)
files=()
for f in "${proved[@]}"; do
  p="verif/kernel_creusot_rlib/$f.coma"
  [ -f "$p" ] || { echo "prove.sh: $p was not generated - was the function renamed?" >&2; exit 1; }
  files+=("$p")
done
cargo creusot --only=prove "${files[@]}"
