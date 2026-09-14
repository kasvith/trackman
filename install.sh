#!/bin/sh
# Installs trackman from GitHub releases.
#
#   curl -fsSL https://raw.githubusercontent.com/kasvith/trackman/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/kasvith/trackman/main/install.sh | sh -s -- v0.2.0
#
# TRACKMAN_INSTALL_DIR sets where the binary goes (default /usr/local/bin).
set -eu

repo=kasvith/trackman
version=${1:-latest}
dir=${TRACKMAN_INSTALL_DIR:-/usr/local/bin}

die() {
  echo "trackman install: $*" >&2
  exit 1
}

case $(uname -s) in
  Linux) os=unknown-linux-musl ;;
  Darwin) os=apple-darwin ;;
  *) die "unsupported OS $(uname -s); download a build from https://github.com/$repo/releases" ;;
esac
case $(uname -m) in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac

if [ "$version" = latest ]; then
  url=https://github.com/$repo/releases/latest/download
else
  version=v${version#v}
  url=https://github.com/$repo/releases/download/$version
fi
archive=trackman-$arch-$os.tar.gz

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"

echo "Downloading $archive ($version)"
curl -fsSL -o "$archive" "$url/$archive" || die "no $archive in release $version"
curl -fsSL -o sha256sums.txt "$url/sha256sums.txt" || die "no sha256sums.txt in release $version"
grep " $archive\$" sha256sums.txt >expected || die "no checksum for $archive"
if command -v sha256sum >/dev/null; then
  sha256sum -c expected >/dev/null
else
  shasum -a 256 -c expected >/dev/null
fi || die "checksum mismatch for $archive"
tar -xzf "$archive"

if mkdir -p "$dir" 2>/dev/null && [ -w "$dir" ]; then sudo=; else sudo=sudo; fi
$sudo mkdir -p "$dir"
$sudo install -m 755 trackman "$dir/trackman"
echo "Installed trackman to $dir/trackman"
case :$PATH: in
  *":$dir:"*) ;;
  *) echo "Add $dir to your PATH to run trackman" ;;
esac
