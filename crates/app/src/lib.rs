//! Library face of the overlay: every module `main.rs` used to own, so the
//! crate's integration tests (`crates/app/tests/`) can drive `Pipeline`
//! directly. `main.rs` keeps only the process entry point.
pub mod dump;
pub mod fonts;
pub mod icons;
// IMAGINE-TAKEDOWN: one of five sites — see `docs/plans/2026-08-17-issue-33-imagines-plan.md` D4.
pub mod imagines;
pub mod inspect;
pub mod logging;
pub mod paths;
pub mod pipeline;
pub mod platform;
pub mod scene_bosses_cache;
pub mod settings;
pub mod ui;
