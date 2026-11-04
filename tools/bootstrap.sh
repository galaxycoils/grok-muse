#!/usr/bin/env bash
set -euo pipefail
project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
darius_path="$(dirname "$project_root")/Darius"
darius_revision=f47a4ce980abecb751c9f5c79e1c424a20100433
if [[ ! -d "$darius_path/.git" ]]; then
  git clone --no-checkout https://github.com/galaxycoils/darius.git "$darius_path"
  git -C "$darius_path" checkout --detach "$darius_revision"
fi
actual_revision="$(git -C "$darius_path" rev-parse HEAD)"
if [[ "$actual_revision" != "$darius_revision" ]]; then
  printf '%s\n' "Darius must be at $darius_revision. Existing checkout was preserved: $actual_revision" >&2
  exit 1
fi
cd "$project_root"
cargo check --workspace --locked
