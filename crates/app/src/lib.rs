//! Library face of the overlay: every module `main.rs` used to own, so the
//! crate's integration tests (`crates/app/tests/`) can drive `Pipeline`
//! directly. `main.rs` keeps only the process entry point.
pub mod dump;
pub mod fonts;
pub mod icons;
pub mod imagines;
pub mod inspect;
pub mod logging;
pub mod paths;
pub mod pipeline;
pub mod platform;
pub mod settings;
pub mod ui;
