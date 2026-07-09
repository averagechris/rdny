#!/usr/bin/env bash
# Fail the job if ci-smoke recorded a failure. Kept separate so ci-smoke can
# finish artifact/log capture before the build status is marked failed.

set -euo pipefail

workdir=${RDNY_SMOKE_WORKDIR:-"$PWD/smoke-artifacts"}
status_file="$workdir/status"

if [[ ! -s $status_file ]]; then
  printf 'smoke gate: missing status file: %s\n' "$status_file" >&2
  exit 1
fi

status=$(<"$status_file")
if [[ $status != 0 ]]; then
  printf 'smoke gate: smoke failed with status %s; see %s\n' "$status" "$workdir" >&2
  exit "$status"
fi

printf 'smoke gate: PASS\n'
