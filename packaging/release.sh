#!/bin/bash
# Build a release of both apps, check that nothing from this machine is inside, and publish it.
#
#   packaging/release.sh            build and check; the files land in dist/release/
#   packaging/release.sh --publish  also create the GitHub release v<version> from CHANGELOG.md
#
# The website and the README link to releases/latest/download/<name>, so the file names never
# change between versions: Hop.dmg and Hop-Setup.exe.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/common.sh"
PUBLISH=0
for a in "$@"; do
  case "$a" in
    --publish) PUBLISH=1 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done
TAG="v$VERSION"

# A committed tree: the build id inside both apps names a commit anyone can look up.
if [ -n "$(git -C "$ROOT" status --porcelain)" ]; then
  echo "Commit your changes first: the build id must name a public commit." >&2
  exit 1
fi
# A stable, certificate-backed signature, so an update keeps the Mac's Accessibility permission.
# ("-" is codesign's spelling of ad hoc.)
if [ "$SIGN_IDENTITY" = "-" ]; then
  echo "HOP_SIGN_IDENTITY=- is an ad-hoc signature; releases need a certificate." >&2
  exit 1
fi
if [ -z "$SIGN_IDENTITY" ] && ! security find-identity -p codesigning 2>/dev/null | grep -q "\"$LOCAL_SIGN_NAME\""; then
  echo "No signing identity. Set HOP_SIGN_IDENTITY, or run packaging/macos/make-local-identity.sh" >&2
  echo "(or import the backed-up \"$LOCAL_SIGN_NAME\" key) so updates keep their permissions." >&2
  exit 1
fi
NOTES=$(awk -v v="$VERSION" '$0 ~ "^## " { if (on) exit; on = ($2 == v) } on' "$ROOT/CHANGELOG.md")
[ -n "$NOTES" ] || { echo "CHANGELOG.md has no \"## $VERSION\" section" >&2; exit 1; }

"$HERE/macos/build.sh"
"$HERE/windows/build.sh"

# Nothing private in what ships. The DMG and the installer are compressed, so check the programs
# that go into them.
PRIVATE=("$HOME" "/Users/$(id -un)" "$(scutil --get ComputerName 2>/dev/null || true)" "$(scutil --get LocalHostName 2>/dev/null || true)"
         "$(git config --global user.email 2>/dev/null || true)" "$(git -C "$ROOT" config user.email 2>/dev/null || true)")
for b in "${SHIPPED_PROGRAMS[@]}"; do
  # Read the whole text first: piping strings into grep -q fails open under pipefail.
  text=$(strings -a "$b") && [ -n "$text" ] || { echo "Could not read $b. Not releasing." >&2; exit 1; }
  for p in "${PRIVATE[@]}"; do
    [ -n "$p" ] || continue
    if grep -qiF -- "$p" <<<"$text"; then
      echo "$b contains \"$p\". Not releasing." >&2
      exit 1
    fi
  done
done
echo "== no private paths or names in the binaries"
if [ -n "$(git -C "$ROOT" status --porcelain)" ]; then
  echo "The build changed tracked files (a redrawn icon?); commit them, then release." >&2
  exit 1
fi
# Authority lines appear only at -vv.
signature=$(codesign -dvv "$MAC_APP" 2>&1) || { echo "Could not read Hop.app's signature." >&2; exit 1; }
grep -q '^Authority=' <<<"$signature" || { echo "Hop.app is not certificate-signed. Not releasing." >&2; exit 1; }

OUT="$ROOT/dist/release"
rm -rf "$OUT" && mkdir -p "$OUT"
cp "$MAC_DMG" "$OUT/Hop.dmg"
cp "$WIN_SETUP" "$OUT/Hop-Setup.exe"
(cd "$OUT" && shasum -a 256 Hop.dmg Hop-Setup.exe > SHA256SUMS.txt)
cat "$OUT/SHA256SUMS.txt"

[ "$PUBLISH" = 1 ] || { echo "== $OUT (not published; pass --publish)"; exit 0; }
git -C "$ROOT" fetch -q origin
if [ "$(git -C "$ROOT" rev-parse HEAD)" != "$(git -C "$ROOT" rev-parse '@{u}')" ]; then
  echo "Push this commit first: the release tag must point at a public commit." >&2
  exit 1
fi
printf '%s\n\n%s\n' "$NOTES" "$(cat "$HERE/release-footer.md")" | sed '1d' > "$OUT/notes.md"
gh release create "$TAG" "$OUT/Hop.dmg" "$OUT/Hop-Setup.exe" "$OUT/SHA256SUMS.txt" \
  --target "$(git -C "$ROOT" rev-parse HEAD)" --title "Hop $VERSION" --notes-file "$OUT/notes.md"
