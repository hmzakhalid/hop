# Sourced by every packaging script: what the release is called and where its pieces are, and
# how to build them.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$ROOT/packaging/macos/config.sh"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
DIST="$ROOT/dist"

MAC_TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
MAC_APP="$DIST/$APP_NAME.app"
MAC_DMG="$DIST/$APP_NAME-$VERSION.dmg"
WIN_TARGET=x86_64-pc-windows-gnu
WIN_PROGRAMS=(hop-agent.exe hop-tray.exe)
WIN_SETUP="$DIST/Hop-Setup-$VERSION.exe"

# The Mac program for each architecture, joined into one universal binary.
MAC_SLICES=()
for t in "${MAC_TARGETS[@]}"; do MAC_SLICES+=("$ROOT/target/$t/release/hop"); done
# Every program that ships, as built (for checks; the DMG and installer are compressed).
SHIPPED_PROGRAMS=("${MAC_SLICES[@]}")
for p in "${WIN_PROGRAMS[@]}"; do SHIPPED_PROGRAMS+=("$ROOT/target/$WIN_TARGET/release/$p"); done

# `cargo_release <target> <cargo build args…>` builds in release mode for <target>, keeping this
# machine's paths (home folder, checkout, Cargo cache) out of the binaries, which otherwise carry
# them in panic messages.
#
# The remapping goes in as the target's rustflags through --config, which joins those in
# .cargo/config.toml (such as +crt-static on Windows). RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS from
# the environment would replace both, so they are dropped. When several prefixes match, rustc
# applies the last, so the most specific come last.
cargo_release() {
  local target="$1"; shift
  local cargo_home="${CARGO_HOME:-$HOME/.cargo}"
  local flags
  flags=$(printf 'target.%s.rustflags=["--remap-path-prefix=%s=~", "--remap-path-prefix=%s=cargo", "--remap-path-prefix=%s=hop"]' \
    "$target" "$HOME" "$cargo_home" "$ROOT")
  (cd "$ROOT" && env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS cargo build -q --release --target "$target" --config "$flags" "$@")
}
