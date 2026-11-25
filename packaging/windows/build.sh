#!/bin/bash
# Build dist/Hop-Setup-<version>.exe (the Windows installer) on this Mac.
#
#   packaging/windows/build.sh
#
# Needs: rustup target x86_64-pc-windows-gnu, mingw-w64 (linker and windres) and makensis
# (brew install mingw-w64 makensis).
# Signing: set HOP_WIN_SIGN to a command that signs the file given as its last argument
# (for example an osslsigncode or Azure Trusted Signing wrapper). Without it, nothing is signed
# and Windows SmartScreen will warn.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
source "$ROOT/packaging/trim-paths.sh"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
TARGET=x86_64-pc-windows-gnu
DIST="$ROOT/dist"
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

(cd "$ROOT" && cargo build -q --release --target "$TARGET" -p hop-agent --config "$(trim_paths "$TARGET")")
for exe in hop-agent.exe hop-tray.exe; do
  cp "$ROOT/target/$TARGET/release/$exe" "$STAGE/"
  [ -n "${HOP_WIN_SIGN:-}" ] && $HOP_WIN_SIGN "$STAGE/$exe"
done
cp "$ROOT/crates/hop-agent/res/hop.ico" "$STAGE/"
cp "$ROOT/LICENSE" "$STAGE/LICENSE.txt"

mkdir -p "$DIST"
OUT="$DIST/Hop-Setup-$VERSION.exe"
ARGS=(-V2 -DVERSION="$VERSION" -DSTAGE="$STAGE" -DOUTFILE="$OUT")
[ -n "${HOP_WIN_SIGN:-}" ] && ARGS+=(-DSIGN="$HOP_WIN_SIGN")
makensis "${ARGS[@]}" "$HERE/hop.nsi"
echo "== $OUT ${HOP_WIN_SIGN:+(signed)}"
