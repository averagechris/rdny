#!/usr/bin/env bash
set -euo pipefail

help=$(nix run .#release -- --help)
grep -Fq -- 'release --version X.Y.Z [--check] [--allow-downgrade] [--submit-linux-build]' <<<"$help"
grep -Fq -- '--check               verify release readiness without editing files or publishing refs' <<<"$help"
if grep -Eq -- '--skip-(validate|tag|artifact|pages)' <<<"$help"; then
  printf 'release help exposes an obsolete bypass flag\n' >&2
  exit 1
fi

for doc in AGENTS.md README.md; do
  grep -Fq 'nix run .#release -- --version X.Y.Z --check' "$doc"
  grep -Fq 'nix run .#release -- --version X.Y.Z --submit-linux-build' "$doc"
  if grep -Eq -- '--skip-(validate|tag|artifact|pages)' "$doc"; then
    printf '%s documents an obsolete release bypass flag\n' "$doc" >&2
    exit 1
  fi
done

printf 'release help and documentation contract passed\n'
