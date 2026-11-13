#!/bin/sh
# Build jiminit as a statically linked release binary.
#
# `+crt-static` only applies to the final binary (cargo rustc), because
# RUSTFLAGS would also apply it to proc-macro crates such as serde_derive,
# which the target cannot build as static crates.
set -eu
cd "$(dirname "$0")/.."

# TARGET=... builds for another architecture, e.g.
#   rustup target add aarch64-unknown-linux-gnu
#   TARGET=aarch64-unknown-linux-gnu ./scripts/build.sh
TARGET=${TARGET:-}
mkdir -p out
if [ -n "$TARGET" ]; then
    cargo rustc --release --target "$TARGET" -- -C target-feature=+crt-static
    BIN=target/$TARGET/release/jiminit
else
    cargo rustc --release -- -C target-feature=+crt-static
    BIN=target/release/jiminit
fi
install -m755 "$BIN" out/jiminit

file out/jiminit
