#!/bin/sh
# Build jiminit as a statically linked release binary.
#
# `+crt-static` only applies to the final binary (cargo rustc), because
# RUSTFLAGS would also apply it to proc-macro crates such as serde_derive,
# which the target cannot build as static crates.
set -eu
cd "$(dirname "$0")/.."

mkdir -p out
cargo rustc --release -- -C target-feature=+crt-static
install -m755 target/release/jiminit out/jiminit

file out/jiminit
