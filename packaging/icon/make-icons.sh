#!/bin/bash
# Build the app icons from the drawing: AppIcon.icns (macOS) and hop.ico (Windows).
# Outputs go to packaging/icon/out/. Needs Xcode's swift, sips and iconutil (all on macOS).
set -euo pipefail
cd "$(dirname "$0")"
OUT=out
rm -rf "$OUT" && mkdir -p "$OUT/AppIcon.iconset"
xcrun swift draw-icon.swift "$OUT/master.png"
for s in 16 32 128 256 512; do
  sips -z $s $s "$OUT/master.png" --out "$OUT/AppIcon.iconset/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) "$OUT/master.png" --out "$OUT/AppIcon.iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$OUT/AppIcon.iconset" -o "$OUT/AppIcon.icns"
mkdir -p "$OUT/ico"
for s in 16 20 24 32 40 48 64 256; do
  sips -z $s $s "$OUT/master.png" --out "$OUT/ico/$s.png" >/dev/null
done
python3 -I ico.py "$OUT/hop.ico" "$OUT"/ico/{16,20,24,32,40,48,64,256}.png
echo "icons: $OUT/AppIcon.icns $OUT/hop.ico"
