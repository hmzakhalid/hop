# Settings for packaging Hop for macOS. Every script in this folder reads this file.

# The bundle identifier. Accessibility permission, Open at Login and Sparkle all key off it, so
# it never changes after the first public release (1.0.0).
BUNDLE_ID="${HOP_BUNDLE_ID:-io.github.hmzakhalid.hop}"
APP_NAME="Hop"
MIN_MACOS="14.0"     # what Hop is tested on (Open at Login alone would allow 13)
COPYRIGHT="© 2026 the Hop authors. Free software under the GPL-3.0."

# Sparkle 2 (updates). Pinned by version and SHA-256; the build refuses anything else. The
# archive is Sparkle's Swift Package release, whose checksum Sparkle publishes in Package.swift at
# the release tag (github.com/sparkle-project/Sparkle/blob/2.10.1/Package.swift); GitHub's asset
# digest agrees. Keep at 2.9.6 or later (GHSA-3x7w-j75x-ppq5), and re-pin for security releases.
SPARKLE_VERSION="2.10.1"
SPARKLE_SHA256="4c2e48a3e50ebcc2c88a58a4c7264a1a99c3c31857ce754357afb9cdb323022a"
# The appcast URL and the EdDSA public key from Sparkle's generate_keys. Leave empty for a
# build without updates.
SPARKLE_FEED_URL="${HOP_SPARKLE_FEED_URL:-}"
SPARKLE_PUBLIC_KEY="${HOP_SPARKLE_PUBLIC_KEY:-}"

# Signing: a "Developer ID Application: …" identity. Without one, builds are signed with the
# local certificate below; Gatekeeper then asks people to choose "Open Anyway" once.
SIGN_IDENTITY="${HOP_SIGN_IDENTITY:-}"
# Without a Developer ID, builds sign with this certificate when the keychain has it
# (make-local-identity.sh), so macOS keeps Hop's permissions across updates. Releases require it:
# an ad-hoc signature changes with every build, and every update would ask for permissions again.
# Back up its private key (Keychain Access → export) to build releases on another Mac.
LOCAL_SIGN_NAME="Hop Local Development"
# A notarytool keychain profile (xcrun notarytool store-credentials NAME …), or empty to skip.
NOTARY_PROFILE="${HOP_NOTARY_PROFILE:-}"
