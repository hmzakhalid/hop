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
source "$HERE/../common.sh"
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

cargo_release "$WIN_TARGET" -p hop-agent
for exe in "${WIN_PROGRAMS[@]}"; do
  cp "$ROOT/target/$WIN_TARGET/release/$exe" "$STAGE/"
  [ -n "${HOP_WIN_SIGN:-}" ] && $HOP_WIN_SIGN "$STAGE/$exe"
done
cp "$ROOT/crates/hop-agent/res/hop.ico" "$STAGE/"
cp "$ROOT/LICENSE" "$STAGE/LICENSE.txt"

mkdir -p "$DIST"
OUT="$WIN_SETUP"
ARGS=(-V2 -DVERSION="$VERSION" -DSTAGE="$STAGE" -DOUTFILE="$OUT")
[ -n "${HOP_WIN_SIGN:-}" ] && ARGS+=(-DSIGN="$HOP_WIN_SIGN")
makensis "${ARGS[@]}" "$HERE/hop.nsi"
echo "== $OUT ${HOP_WIN_SIGN:+(signed)}"
