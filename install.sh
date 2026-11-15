#!/bin/sh
# Installs the latest merkwerk release on an Apple Silicon Mac:
#   curl -fsSL https://raw.githubusercontent.com/ArzelaAscoIi/merkwerk/main/install.sh | sh
# Downloads the archive built by the release workflow, checks its SHA-256, then runs it once,
# which writes ~/Applications/merkwerk.app and opens it.
set -eu
repo=ArzelaAscoIi/merkwerk
if [ "$(uname -sm)" != "Darwin arm64" ]; then
  echo "merkwerk releases are built for Apple Silicon Macs. Build it with cargo instead: https://github.com/$repo#install" >&2
  exit 1
fi
tag=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
[ -n "$tag" ] || { echo "Could not find the latest merkwerk release." >&2; exit 1; }
asset="merkwerk-$tag-aarch64-apple-darwin.tar.gz"
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
cd "$dir"
echo "Downloading merkwerk $tag"
curl -fsSLO "https://github.com/$repo/releases/download/$tag/$asset"
curl -fsSLO "https://github.com/$repo/releases/download/$tag/$asset.sha256"
shasum -a 256 -c "$asset.sha256" >/dev/null || { echo "Checksum mismatch, not installing." >&2; exit 1; }
tar -xzf "$asset"
./merkwerk
