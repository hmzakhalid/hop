# Sourced by the build scripts. `cargo_release <target> <cargo build args…>` builds in release
# mode for <target>, keeping this machine's paths (home folder, checkout, Cargo cache) out of the
# binaries, which otherwise carry them in panic messages.
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
