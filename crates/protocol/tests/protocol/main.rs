//! The crate's single integration-test target (plan T1.5). Both suites below
//! used to be their own `tests/*.rs`, and so their own test executable; they
//! are modules of one binary now. Cargo discovers `tests/protocol/main.rs` as
//! the `protocol` test target, so test names are module-qualified (e.g.
//! `cargo test -p bpsr-protocol framing::byte_at_a_time_delivery_matches_single_push`).

mod framing;
mod robustness;
