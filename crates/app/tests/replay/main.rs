//! The crate's single integration-test target (`docs/replay-system-tests.md`).
//!
//! Every scenario file below used to be its own `tests/*.rs`, and so its own
//! ~340 MB test executable, relinked per worktree. They are modules of one
//! binary now: cargo discovers `tests/replay/main.rs` as the `replay` test
//! target, and plain `mod` declarations resolve against this directory. The
//! harness they share is still `tests/common/mod.rs`, reached as
//! `crate::common`. Test names are therefore module-qualified, e.g.
//! `cargo test -p ShinraMeter-BPSR replay_pull::multi_player_pull`.

#[path = "../common/mod.rs"]
mod common;

mod replay_death;
mod replay_dump;
mod replay_dump_bin;
mod replay_entities;
mod replay_history;
mod replay_lifecycle;
mod replay_pull;
mod replay_scenarios;
mod replay_scenarios_2;
mod replay_team;
