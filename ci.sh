#!/usr/bin/env bash
# The quality gate: formatting, clippy with warnings denied, the whole test suite.
set -euo pipefail
cd "$(dirname "$0")"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace "$@"
