#!/usr/bin/env bash
set -euo pipefail

help=$(nix run .#release -- --help)
grep -Fq -- 'release --version X.Y.Z [--check] [--allow-downgrade]' <<<"$help"
grep -Fq -- '--check  nonmutating ref/version preflight only; does not run validation or build artifacts' <<<"$help"
if grep -Fq -- '--submit-linux-build' <<<"$help"; then
  printf 'GitHub release help exposes the SourceHut Linux submission flag\n' >&2
  exit 1
fi
if grep -Eq -- '--skip-(validate|tag|artifact|pages)' <<<"$help"; then
  printf 'release help exposes an obsolete bypass flag\n' >&2
  exit 1
fi

for doc in AGENTS.md README.md docs/release.md; do
  grep -Fq 'nix run .#release -- --version X.Y.Z --check' "$doc"
  grep -Fq 'nix run .#release -- --version X.Y.Z' "$doc"
  if grep -Fq -- '--submit-linux-build' "$doc"; then
    printf '%s documents the obsolete SourceHut Linux submission flag\n' "$doc" >&2
    exit 1
  fi
  if grep -Eq -- '--skip-(validate|tag|artifact|pages)' "$doc"; then
    printf '%s documents an obsolete release bypass flag\n' "$doc" >&2
    exit 1
  fi
done

printf 'release help and documentation contract passed\n'
