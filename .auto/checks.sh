#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy --all-targets --all-features --quiet -- -D warnings
cargo test --all-targets --all-features --quiet
