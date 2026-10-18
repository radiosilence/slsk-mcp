#!/usr/bin/env bash
# Prepare a release PR: branch from origin/main, bump the version everywhere
# CI compares it (the slsk-mcp crate and Cargo.lock, the Pulumi package and its
# lockfile, and the APP_VERSION the package deploys), turn the unreleased
# changelog fragments into the release, commit, push and open the PR. Never
# creates or pushes a tag: merging the PR is what releases.
#
#   scripts/release.sh X.Y.Z|patch|minor|major [--no-push]
set -euo pipefail

version=""
push=1
for arg in "$@"; do
  case "$arg" in
    --no-push) push=0 ;;
    -*) echo "unknown option: $arg" >&2; exit 2 ;;
    *) version="$arg" ;;
  esac
done
usage() { echo "usage: scripts/release.sh X.Y.Z|patch|minor|major [--no-push]" >&2; exit 2; }
[ -n "$version" ] || usage

crate=crates/slsk-mcp/Cargo.toml
versions=deploy/pulumi/src/versions.ts

cd "$(git rev-parse --show-toplevel)"
git fetch origin --quiet
current=$(git show "origin/main:$crate" | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)
IFS=. read -r major minor patch <<<"$current"
case "$version" in
  patch) version="$major.$minor.$((patch + 1))" ;;
  minor) version="$major.$((minor + 1)).0" ;;
  major) version="$((major + 1)).0.0" ;;
esac
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || usage
tag="v$version"
branch="release-$tag"

[ -z "$(git status --porcelain)" ] || { echo "working tree is not clean" >&2; exit 1; }
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null || [ -n "$(git ls-remote --tags origin "refs/tags/$tag")" ]; then
  echo "tag $tag already exists" >&2
  exit 1
fi

newest=$(printf '%s\n%s\n' "$current" "$version" | sort -V | tail -1)
if [ "$version" = "$current" ] || [ "$newest" != "$version" ]; then
  echo "$version is not greater than the current version $current" >&2
  exit 1
fi

# A branch left by a closed or merged release PR is cut again; an open PR is not touched.
local_branch=$(git rev-parse -q --verify "refs/heads/$branch" || true)
remote_branch=$(git ls-remote --heads origin "$branch")
if [ -n "$local_branch" ] || [ -n "$remote_branch" ]; then
  open_pr=$(gh pr list --head "$branch" --state all --json number,state -q 'map(select(.state == "OPEN"))[0].number // empty')
  if [ -n "$open_pr" ]; then
    echo "branch $branch has open PR #$open_pr: close it first, or merge it" >&2
    exit 1
  fi
  if [ -n "$local_branch" ]; then
    [ "$(git branch --show-current)" != "$branch" ] || git switch -q --detach origin/main
    git branch -q -D "$branch"
  fi
  if [ "$push" = 1 ]; then
    [ -z "$remote_branch" ] || git push -q origin --delete "$branch"
    echo "deleted stale branch $branch, cutting it again from origin/main"
  else
    echo "deleted stale local branch $branch, cutting it again from origin/main"
    [ -z "$remote_branch" ] || echo "warning: origin/$branch is left (--no-push); the push will need it deleted" >&2
  fi
fi

git switch -c "$branch" origin/main --no-track

sed -i.bak "s/^version = \"$current\"/version = \"$version\"/" "$crate"
rm -f "$crate.bak"
grep -q "^version = \"$version\"" "$crate"

# The package pins the image it deploys by APP_VERSION, so all three move together.
(cd deploy/pulumi && npm version "$version" --no-git-tag-version --allow-same-version >/dev/null)
sed -i.bak "s/^export const APP_VERSION = \"[^\"]*\";/export const APP_VERSION = \"$version\";/" "$versions"
rm -f "$versions.bak"
grep -q "^export const APP_VERSION = \"$version\";" "$versions"

# Only the workspace's own entries change in Cargo.lock.
cargo update --workspace

python3 scripts/changelog.py --release "$version"

git add -A
git commit -q -m "release: $tag"
echo "committed release: $tag on $branch"

if [ "$push" = 1 ]; then
  git push -q -u origin "$branch"
  section=$(awk -v h="## $version" '$0==h{f=1;next} f&&/^## /{exit} f' CHANGELOG.md)
  gh pr create --title "release: $tag" --body "$section"
else
  echo "not pushed (--no-push)"
fi
