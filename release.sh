#!/bin/sh
# Cuts a release: works out the next version from conventional commits (or takes one,
# e.g. ./release.sh 1.2.0), updates Cargo.toml and CHANGELOG.md, commits, tags and pushes.
# The Release workflow builds and publishes from the tag.
set -eu

die() {
  echo "release: $*" >&2
  exit 1
}

[ -z "$(git status --porcelain)" ] || die "commit or stash your changes first"

version=${1:-}
[ -n "$version" ] || version=$(git cliff --bumped-version)
version=${version#v}
echo "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "version must look like 1.2.3, got '$version'"

printf 'Release v%s? [y/N] ' "$version"
read -r answer
[ "$answer" = y ] || exit 1

perl -0pi -e 's/^version = ".*?"/version = "'"$version"'"/m' Cargo.toml
cargo update --workspace --quiet
git cliff --tag "v$version" --output CHANGELOG.md
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore(release): prepare for v$version"
git tag -a "v$version" -m "v$version"
git push --atomic origin HEAD "v$version"
