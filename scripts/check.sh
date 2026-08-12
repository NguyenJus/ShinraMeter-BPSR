#!/bin/bash
set -e

# Quiet full verification: test on host, then cross-check for Windows
cargo test -q --workspace
cargo check -q --workspace --target x86_64-pc-windows-gnu

echo "✓ All checks passed"
