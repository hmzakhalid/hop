# Sourced by the build scripts. Keeps this machine's paths (home folder, checkout, Cargo cache)
# out of release binaries, which otherwise carry them in panic messages.
#
#   cargo build … --config "$(trim_paths <target-triple>)"
#
# A target's rustflags from --config join those in .cargo/config.toml (such as +crt-static on
# Windows) instead of replacing them, which RUSTFLAGS would do. When several prefixes match,
# rustc applies the last, so the most specific come last.
trim_paths() {
  local cargo_home="${CARGO_HOME:-$HOME/.cargo}"
  printf 'target.%s.rustflags=["--remap-path-prefix=%s=~", "--remap-path-prefix=%s=cargo", "--remap-path-prefix=%s=hop"]' \
    "$1" "$HOME" "$cargo_home" "$ROOT"
}
