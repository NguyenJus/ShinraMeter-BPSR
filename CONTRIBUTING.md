# Contributing to ShinraMeter-BPSR

Thanks for your interest in contributing. This is a small, mostly-solo
project, so please read this before opening an issue or PR.

## Reporting a bug

Open a [GitHub issue](../../issues/new) and include:

- The app version (check the title bar, or the zip filename you downloaded).
- Your OS version (Windows build number).
- What you did, what you expected, and what actually happened.
- The relevant status banner text, if the overlay showed one — see the
  Troubleshooting section of the README for what each banner means.

**Do not paste raw log files, packet-inspection dumps, or `RUST_LOG=debug`
output into an issue.** These can contain other players' character names and
other traffic from your game session, and issues are public. If a log excerpt
is genuinely needed to diagnose the bug, trim it down to the minimal relevant
lines and redact any names that aren't your own. Never attach the log file or
dump file itself (`ShinraMeter-BPSR.log`, `inspect/dump-<pid>.jsonl`).

## Requesting a feature

Open an issue describing the problem you're trying to solve, not just the
solution — what you're doing in-game when you want this, and why the current
behavior doesn't cover it. Check existing open issues first to avoid
duplicates.

## Development setup

The project is a Rust workspace, developed cross-platform (including on
WSL) and targeting Windows.

Prerequisites, cross-compiling to Windows from Linux/WSL:

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt-get install -y gcc-mingw-w64-x86-64 binutils-mingw-w64-x86-64
```

`crates/capture` links `windivert` with the `vendored` feature, which shells
out to `x86_64-w64-mingw32-gcc` during the build script, so the mingw
toolchain above is required even for a plain `cargo check --target
x86_64-pc-windows-gnu`.

Common commands:

```sh
# Fast host-only type check (no mingw needed)
cargo check --workspace

# Cross-target type check (requires mingw)
cargo check --workspace --target x86_64-pc-windows-gnu

# Run the test suite
cargo test --workspace

# Full release build for Windows
cargo build --release --target x86_64-pc-windows-gnu
```

The built executable lands at
`target/x86_64-pc-windows-gnu/release/ShinraMeter-BPSR.exe`. See the README's
Build and Testing sections for more detail, including how the WinDivert
runtime is packaged.

## Code style and local checks

Run `scripts/check.sh` before opening a PR — it runs the same test and
cross-check steps as CI:

```sh
./scripts/check.sh
```

This runs `cargo test -q --workspace` followed by `cargo check -q --workspace
--target x86_64-pc-windows-gnu`. CI additionally runs `cargo fmt --all
--check` and `cargo clippy --workspace --all-targets --target
x86_64-pc-windows-gnu` with warnings denied — run `cargo fmt --all` and
`cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu`
locally as well before pushing.

**Note:** CI on this repo may be queued or triggered manually rather than
running immediately on every push. Please run the checks above locally before
opening a PR rather than relying on CI to catch problems — a PR that hasn't
been checked locally may sit longer before review.

## Pull requests

- Keep PRs small and focused on one change. Large, multi-purpose PRs are
  harder to review and more likely to stall.
- Link the issue your PR addresses (`Fixes #123`) rather than duplicating its
  description.
- Make sure `scripts/check.sh`, `cargo fmt --all --check`, and `cargo clippy`
  pass locally before requesting review.
- Describe what you tested and how, especially for anything that touches
  packet decoding or capture — a synthetic repro or the meter simulator is
  preferred over attaching real capture data.
