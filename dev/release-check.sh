#!/bin/bash

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CRATE="$(dirname "$HERE")"

cd "$CRATE"
cargo fmt --check
cargo test --offline --locked
cargo build --release --offline --locked --target aarch64-apple-darwin
