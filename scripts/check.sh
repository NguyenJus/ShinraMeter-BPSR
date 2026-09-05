#!/bin/bash
set -e

# Quiet local verification: mirrors CI's fmt, clippy and test jobs; the
# cross `cargo check --target x86_64-pc-windows-gnu` approximates
# build-windows without the release link or its manifest/DLL assertions.
#
# Mirrors ci.yml's env block. This changes the fingerprint of every cargo
# invocation below, so the first run after adding this forces one full
# local rebuild.
export RUSTFLAGS="-D warnings"

cargo fmt --all --check
scripts/package-release.test.sh
cargo clippy -q --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo test -q --workspace
cargo check -q --workspace --target x86_64-pc-windows-gnu

echo "✓ All checks passed"
