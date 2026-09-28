# Release process

New rdny releases are GitHub-only. The local release app comes from the
SHA-pinned `averagechris/fleet` `lib.fleet.presets.rust` preset. It prepares
and validates the release commit, then atomically publishes `main` and its
annotated `vX.Y.Z` tag to `averagechris/rdny`. The tag starts the thin caller
in `.github/workflows/release.yml`; the SHA-pinned fleet workflow builds:

- `aarch64-darwin` on `macos-14`
- `x86_64-linux` on `ubuntu-24.04`

Each platform contributes an archive, checksum, and `release-identity-<platform>`
file to an Actions artifact named `release-<platform>`. The identity file binds
the build to the annotated tag object and its peeled commit. A green workflow
means only that all six files are ready; an operator must publish the four
archive and checksum files and refresh the website manually.

## Commands

```bash
nix run .#release -- --version X.Y.Z --check
nix run .#release -- --version X.Y.Z
```

Run these from a fresh empty `@` whose parent exactly matches local `main` and
`main@origin`. `--check` is a nonmutating ref/version preflight only: it checks
the requested version and release refs, but does not run validation or build
the release artifact. The real release command updates `Cargo.toml`,
`Cargo.lock`, and `CHANGELOG.md`, runs the configured fmt, Clippy, test, and
release-contract gates, creates an annotated tag, and atomically pushes the
release commit and tag. GitHub Actions then builds and verifies both platform
artifacts. The local command does not upload assets, create a GitHub Release,
or dispatch the website.

The workflow does not run on a `main` push. To recover an existing release,
dispatch it from the default branch (for example,
`gh workflow run release.yml --ref main -f tag=v0.3.1`). The requested tag must
exist on `origin`, be annotated, and peel to a commit that is an ancestor of
the selected `main` commit. This recovery mode never moves the tag and disables
automatic publication. Tag pushes still require the event SHA to equal the
tag's peeled commit. Unknown tags,
malformed tags, forks, pull requests, and mismatched refs fail closed.

## Publish the built artifacts

After the workflow succeeds, use a locally authorized `gh` session (run
`gh auth refresh -h github.com -s workflow` if dispatch permission is absent):

```bash
set -euo pipefail

repo=averagechris/rdny
tag=vX.Y.Z
run_id=123456789
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]

# Accept only this repository's successful release builder. A tag push must run
# at the tag commit. A recovery dispatch must run from this repository's main.
run="$(gh run view "$run_id" -R "$repo" \
  --json workflowName,conclusion,event,headBranch,headSha)"
test "$(jq -r .workflowName <<<"$run")" = \
  'Build release artifacts (manual publication required)'
test "$(jq -r .conclusion <<<"$run")" = success
event="$(jq -r .event <<<"$run")"
run_branch="$(jq -r .headBranch <<<"$run")"
run_sha="$(jq -r .headSha <<<"$run")"
repo_context="$(gh repo view "$repo" --json nameWithOwner,defaultBranchRef)"
test "$(jq -r .nameWithOwner <<<"$repo_context")" = "$repo"
test "$(jq -r .defaultBranchRef.name <<<"$repo_context")" = main

resolve_remote_tag() {
  local ref tag_data
  ref="$(gh api "repos/$repo/git/ref/tags/$tag")"
  test "$(jq -r .object.type <<<"$ref")" = tag
  tag_object="$(jq -r .object.sha <<<"$ref")"
  tag_data="$(gh api "repos/$repo/git/tags/$tag_object")"
  test "$(jq -r .object.type <<<"$tag_data")" = commit
  commit="$(jq -r .object.sha <<<"$tag_data")"
  test -n "$tag_object" && test -n "$commit"
}
resolve_remote_tag
case "$event" in
  push)
    test "$run_branch" = "$tag"
    test "$run_sha" = "$commit"
    ;;
  workflow_dispatch)
    test "$run_branch" = main
    # The workflow validates that this main commit contains the tag commit.
    gh api "repos/$repo/commits/$run_sha" --silent
    ;;
  *) exit 1 ;;
esac

# Use a new directory. Never remove an existing operator directory.
work="$(mktemp -d "${TMPDIR:-/tmp}/rdny-$tag.XXXXXX")"
mkdir "$work/aarch64-darwin" "$work/x86_64-linux"
gh run download "$run_id" -R "$repo" -n release-aarch64-darwin \
  -D "$work/aarch64-darwin"
gh run download "$run_id" -R "$repo" -n release-x86_64-linux \
  -D "$work/x86_64-linux"

version="${tag#v}"
darwin="$work/aarch64-darwin/rdny-v$version-aarch64-darwin.tar.gz"
linux="$work/x86_64-linux/rdny-v$version-x86_64-linux.tar.gz"
assets=("$darwin" "$darwin.sha256" "$linux" "$linux.sha256")
identities=(
  "$work/aarch64-darwin/release-identity-aarch64-darwin"
  "$work/x86_64-linux/release-identity-x86_64-linux"
)
expected=("${assets[@]}" "${identities[@]}")
for file in "${expected[@]}"; do test -f "$file"; done
test "$(find "$work" -type f | wc -l | tr -d ' ')" -eq 6
test "$(find "$work" -type f -print | sort)" = \
  "$(printf '%s\n' "${expected[@]}" | sort)"
for identity in "${identities[@]}"; do
  test "$(wc -l <"$identity" | tr -d ' ')" -eq 2
  test "$(sed -n '1p' "$identity")" = "$tag_object"
  test "$(sed -n '2p' "$identity")" = "$commit"
done
(cd "$work/aarch64-darwin" && shasum -a 256 -c -- "$(basename "$darwin.sha256")")
(cd "$work/x86_64-linux" && shasum -a 256 -c -- "$(basename "$linux.sha256")")

# Stop if any published release or draft already uses this tag. If this finds a
# draft from an interrupted attempt, stop creating new drafts. Inspect its
# existing assets with `gh release view "$tag" -R "$repo" --json isDraft,assets`.
# For each present asset, verify its bytes against the corresponding verified
# local asset, then upload only missing expected assets without clobbering
# existing ones. Fail and seek an operator decision for mismatched, incomplete,
# or open assets. Only after all expected assets are complete should you resume
# the remote four-file comparison below, and undraft the release only then.
matches="$(gh api --paginate "repos/$repo/releases?per_page=100" \
  --jq ".[] | select(.tag_name == \"$tag\") | .id")"
test -z "$matches" || {
  echo "release or draft already exists for $tag; follow the safe resume note" >&2
  exit 1
}

# Recheck the remote tag immediately before creating the draft.
old_tag_object="$tag_object"; old_commit="$commit"
resolve_remote_tag
test "$tag_object" = "$old_tag_object" && test "$commit" = "$old_commit"
gh release create "$tag" -R "$repo" --verify-tag --draft \
  --title "$tag" --notes-from-tag
gh release upload "$tag" -R "$repo" "${assets[@]}" # no --clobber

remote_dir="$(mktemp -d "${TMPDIR:-/tmp}/rdny-$tag-remote.XXXXXX")"
gh release download "$tag" -R "$repo" -D "$remote_dir"
test "$(find "$remote_dir" -type f -exec basename {} \; | sort)" = \
  "$(printf '%s\n' "${assets[@]##*/}" | sort)"
for asset in "${assets[@]}"; do
  cmp "$asset" "$remote_dir/$(basename "$asset")"
done
(cd "$remote_dir" && shasum -a 256 -c -- *.sha256)

# Recheck the tag once more before making the release public.
resolve_remote_tag
test "$tag_object" = "$old_tag_object" && test "$commit" = "$old_commit"
gh release edit "$tag" -R "$repo" --draft=false

gh workflow run pages.yml --repo averagechris/averagechris.github.io \
  -f project=rdny -f tag="$tag" -f sha="$commit"
```

Do not undraft until all four remote assets compare byte-for-byte. After the
site workflow is green, verify its live release label, download URLs, and both
displayed checksums against the sidecars.

## Historical SourceHut releases

SourceHut release artifacts and `builds/release-linux-x86_64.yml` are archival
for tags published before this migration. Do not dual-publish new tags and do
not submit that manifest for a new release. Historical source and artifacts
remain at <https://git.sr.ht/~averagechris/rdny>.

## Notes

- Versions are plain semver `X.Y.Z`; release tags are `vX.Y.Z`.
- Run `jj lint` before releasing. There are no release-stage bypass flags.
- Artifact tarballs are byte-reproducible (`--sort=name --mtime=@1 --owner=0
  --group=0`, `gzip -n`).
- `dist/` is generated output and stays untracked.
