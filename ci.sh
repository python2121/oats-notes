#!/usr/bin/env bash
# The quality gate: formatting, clippy with warnings denied, the whole test suite.
set -euo pipefail
cd "$(dirname "$0")"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace "$@"
# crates/oats-sources-macos is outside the workspace (its Core Audio FFI needs `unsafe`), so
# the workspace commands above don't lint or test it: its own pass, same rules
CARGO_TARGET_DIR=target/sources-macos cargo clippy --manifest-path crates/oats-sources-macos/Cargo.toml --all-targets -- -D warnings
CARGO_TARGET_DIR=target/sources-macos cargo test --manifest-path crates/oats-sources-macos/Cargo.toml
