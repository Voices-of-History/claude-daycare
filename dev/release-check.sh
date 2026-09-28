#!/bin/bash
# Format, test, and build the release target(s) for the machine you are on.
# macOS builds aarch64-apple-darwin. Linux builds the static musl binary for
# this machine's architecture (x86_64 or aarch64); `ring` needs a C compiler
# for that target, e.g. `musl-tools` on Debian/Ubuntu or zig via cargo-zigbuild.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CRATE="$(dirname "$HERE")"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) TARGET=aarch64-apple-darwin ;;
  Linux-x86_64) TARGET=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) TARGET=aarch64-unknown-linux-musl ;;
  *)
    echo "release-check: no release target for $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

cd "$CRATE"
cargo fmt --check
cargo test --offline --locked
rustup target add "$TARGET" >/dev/null
cargo build --release --offline --locked --target "$TARGET"
echo "built target/$TARGET/release/daycare-runner"
