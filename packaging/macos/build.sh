#!/bin/bash
# Build Hop.app and Hop-<version>.dmg in dist/.
#
#   packaging/macos/build.sh [--dev]
#
# --dev   include the developer commands (hop drive, hop menu, …) for testing
#
# Signing, notarization and Sparkle are driven by packaging/macos/config.sh (and the HOP_*
# environment variables it reads). Without a Developer ID the app is signed with the local
# certificate (make-local-identity.sh) or ad hoc, and Gatekeeper asks for "Open Anyway" once.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
source "$HERE/config.sh"
source "$ROOT/packaging/cargo-release.sh"

DEV=0
for a in "$@"; do
  case "$a" in
    --dev) DEV=1 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
BUILD=$(git -C "$ROOT" rev-list --count HEAD) # rises with every commit, as updates need
DIST="$ROOT/dist"
APP="$DIST/$APP_NAME.app"
FEATURES=(--no-default-features)
[ "$DEV" = 1 ] && FEATURES=()

echo "== Hop $VERSION ($BUILD), $BUNDLE_ID"

# 1. A universal binary.
rustup target list --installed | grep -q x86_64-apple-darwin || rustup target add x86_64-apple-darwin
for t in aarch64-apple-darwin x86_64-apple-darwin; do
  MACOSX_DEPLOYMENT_TARGET="$MIN_MACOS" cargo_release "$t" -p hop ${FEATURES[@]+"${FEATURES[@]}"}
done

# 2. The bundle.
rm -rf "$APP" && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Frameworks"
lipo -create -output "$APP/Contents/MacOS/$APP_NAME" \
  "$ROOT/target/aarch64-apple-darwin/release/hop" "$ROOT/target/x86_64-apple-darwin/release/hop"
"$ROOT/packaging/icon/make-icons.sh" >/dev/null
cp "$ROOT/packaging/icon/out/AppIcon.icns" "$APP/Contents/Resources/"

sed -e "s|@APP_NAME@|$APP_NAME|g" -e "s|@BUNDLE_ID@|$BUNDLE_ID|g" -e "s|@VERSION@|$VERSION|g" \
    -e "s|@BUILD@|$BUILD|g" -e "s|@MIN_MACOS@|$MIN_MACOS|g" -e "s|@COPYRIGHT@|$COPYRIGHT|g" \
    "$HERE/Info.plist.in" > "$APP/Contents/Info.plist"
if [ -n "$SPARKLE_FEED_URL" ]; then
  [ -n "$SPARKLE_PUBLIC_KEY" ] || { echo "SPARKLE_FEED_URL needs SPARKLE_PUBLIC_KEY (Sparkle's generate_keys)" >&2; exit 1; }
  plutil -insert SUFeedURL -string "$SPARKLE_FEED_URL" "$APP/Contents/Info.plist"
  plutil -insert SUPublicEDKey -string "$SPARKLE_PUBLIC_KEY" "$APP/Contents/Info.plist"
  plutil -insert SUEnableAutomaticChecks -bool true "$APP/Contents/Info.plist"
  "$HERE/sparkle.sh" "$APP/Contents/Frameworks"
fi
plutil -lint -s "$APP/Contents/Info.plist"

# 3. Signing, inside out (Sparkle's helpers, the framework, then the app).
LOCAL_IDENTITY=""
if [ -z "$SIGN_IDENTITY" ] && security find-identity -p codesigning 2>/dev/null | grep -q "\"$LOCAL_SIGN_NAME\""; then
  LOCAL_IDENTITY="$LOCAL_SIGN_NAME"
fi
sign() {
  if [ -n "$SIGN_IDENTITY" ]; then
    codesign --force --timestamp --options runtime --sign "$SIGN_IDENTITY" "$@"
  else
    codesign --force --sign "${LOCAL_IDENTITY:--}" "$@"
  fi
}
FW="$APP/Contents/Frameworks/Sparkle.framework"
if [ -d "$FW" ]; then
  for x in "$FW"/Versions/B/XPCServices/*.xpc "$FW/Versions/B/Autoupdate" "$FW/Versions/B/Updater.app"; do
    [ -e "$x" ] && sign "$x"
  done
  sign "$FW"
fi
sign --entitlements "$HERE/entitlements.plist" "$APP"
codesign --verify --strict --deep "$APP"
echo "   signed: ${SIGN_IDENTITY:-${LOCAL_IDENTITY:-ad-hoc}}"

notarize() {
  [ -n "$NOTARY_PROFILE" ] || return 0
  xcrun notarytool submit "$1" --keychain-profile "$NOTARY_PROFILE" --wait
}

# 4. Notarize the app itself, so its ticket is stapled even outside the DMG.
if [ -n "$NOTARY_PROFILE" ]; then
  [ -n "$SIGN_IDENTITY" ] || { echo "notarization needs a Developer ID (SIGN_IDENTITY)" >&2; exit 1; }
  ditto -c -k --keepParent "$APP" "$DIST/$APP_NAME.zip"
  notarize "$DIST/$APP_NAME.zip"
  xcrun stapler staple "$APP"
  rm "$DIST/$APP_NAME.zip"
fi

# 5. The disk image: the app and a link to /Applications.
DMG="$DIST/$APP_NAME-$VERSION.dmg"
STAGE=$(mktemp -d)
cp -R "$APP" "$STAGE/" && ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -quiet -volname "$APP_NAME" -srcfolder "$STAGE" -format UDZO -ov "$DMG"
rm -rf "$STAGE"
[ -n "$SIGN_IDENTITY" ] && codesign --force --timestamp --sign "$SIGN_IDENTITY" "$DMG"
if [ -n "$NOTARY_PROFILE" ]; then
  notarize "$DMG"
  xcrun stapler staple "$DMG"
fi
echo "== $APP"
echo "== $DMG"
