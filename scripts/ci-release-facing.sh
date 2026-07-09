#!/usr/bin/env bash
# Validate release-facing package surfaces without uploading anything.

set -euo pipefail

version=$(awk '/^\[package\]/{s=1} s && /^version = /{gsub(/"/,"",$3); print $3; exit}' Cargo.toml)
license=$(awk '/^\[package\]/{s=1} s && /^license = /{gsub(/"/,"",$3); print $3; exit}' Cargo.toml)

[[ $license == MIT ]] || { printf 'expected MIT license, got %s\n' "$license" >&2; exit 1; }
[[ -s LICENSE ]] || { printf 'missing LICENSE file\n' >&2; exit 1; }
grep -q 'MIT License' LICENSE

nix build .#rdny --out-link result-rdny
[[ $(./result-rdny/bin/rdny --version) == "rdny $version" ]]
./result-rdny/bin/rdny --help | grep -q 'Chrome automation from the command line'
./result-rdny/bin/rdny help screenshot | grep -q 'Capture a screenshot'
if grep -R -- '--json' README.md docs/pages; then
  printf 'docs mention nonexistent --json support\n' >&2
  exit 1
fi

nix build .#release-artifact --out-link result-release-artifact
shopt -s nullglob
artifacts=(result-release-artifact/*.tar.gz)
[[ ${#artifacts[@]} -gt 0 ]] || { printf 'no release tarballs found\n' >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for artifact in "${artifacts[@]}"; do
  [[ -s $artifact.sha256 ]] || { printf 'missing checksum for %s\n' "$artifact" >&2; exit 1; }
  (cd "$(dirname "$artifact")" && sha256sum -c "$(basename "$artifact").sha256")
  tar -xzf "$artifact" -C "$tmp"
done

binary=$(find "$tmp" -type f -name rdny -perm -111 -print -quit)
[[ -n ${binary:-} ]] || { printf 'release artifact did not contain an executable rdny binary\n' >&2; exit 1; }
[[ $("$binary" --version) == "rdny $version" ]]
"$binary" --help | grep -q 'Chrome automation from the command line'
mkdir -p "$tmp/home"
env -i HOME="$tmp/home" PATH=/usr/bin:/bin "$binary" --version | grep -q "^rdny $version$"
env -i HOME="$tmp/home" PATH=/usr/bin:/bin "$binary" --help | grep -q 'Chrome automation from the command line'

[[ -n $(find "$tmp" -type f -name README.md -print -quit) ]] || { printf 'release artifact did not contain README.md\n' >&2; exit 1; }
[[ -n $(find "$tmp" -type f -name CHANGELOG.md -print -quit) ]] || { printf 'release artifact did not contain CHANGELOG.md\n' >&2; exit 1; }
[[ -n $(find "$tmp" -type f \( -name LICENSE -o -name LICENSE.txt \) -print -quit) ]] || { printf 'release artifact did not contain LICENSE\n' >&2; exit 1; }

printf 'release-facing checks passed for rdny %s\n' "$version"
