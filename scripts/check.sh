#!/bin/bash
set -e

# Quiet full verification: lint first (cheapest, and what CI's fmt/clippy
# jobs actually gate on — issue #341, so a locally green run can't still
# fail CI lint), then test on host, then cross-check for Windows.
cargo fmt --all --check
cargo clippy -q --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo test -q --workspace
cargo check -q --workspace --target x86_64-pc-windows-gnu

echo "✓ All checks passed"
