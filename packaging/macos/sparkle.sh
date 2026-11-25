#!/bin/bash
# Put Sparkle.framework (pinned version, verified SHA-256) into the given Frameworks folder.
# The download is cached in ~/Library/Caches/hop-packaging.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/config.sh"
DEST="$1"
CACHE="$HOME/Library/Caches/hop-packaging"
ARCHIVE="$CACHE/Sparkle-$SPARKLE_VERSION-spm.zip"
mkdir -p "$CACHE"
if [ ! -f "$ARCHIVE" ]; then
  curl -fsSL --proto '=https' -o "$ARCHIVE.part" \
    "https://github.com/sparkle-project/Sparkle/releases/download/$SPARKLE_VERSION/Sparkle-for-Swift-Package-Manager.zip"
  mv "$ARCHIVE.part" "$ARCHIVE"
fi
GOT=$(shasum -a 256 "$ARCHIVE" | cut -d' ' -f1)
if [ "$GOT" != "$SPARKLE_SHA256" ]; then
  rm -f "$ARCHIVE"
  echo "Sparkle archive hash mismatch (got $GOT); refusing to use it." >&2
  exit 1
fi
WORK=$(mktemp -d)
ditto -x -k "$ARCHIVE" "$WORK"
mkdir -p "$DEST"
ditto "$WORK/Sparkle.xcframework/macos-arm64_x86_64/Sparkle.framework" "$DEST/Sparkle.framework"
rm -rf "$WORK"
