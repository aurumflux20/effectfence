#!/usr/bin/env bash
# The EXACT checks CI runs, in the same order, with real exit codes.
#
# Why this file exists: `cargo test` exits 0 on warnings, CI's `-D warnings`
# does not. Approximating CI locally, or grepping a log for "passed", let three
# commits sit red on main. Run this, not a subset of it.
set -euo pipefail

echo "→ clippy (warnings are errors)"
cargo clippy --all-targets --all-features -- -D warnings

echo "→ fmt check"
cargo fmt -- --check

echo "→ tests (all features)"
cargo test --all-features

echo "✓ all CI checks pass locally"
