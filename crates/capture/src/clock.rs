//! The two clocks the Windows capture loop (`win.rs`) stamps events
//! with, factored out so the split between them stays unit-testable on any
//! host (win.rs itself cannot be built off Windows).

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Monotonic milliseconds since this process started capturing — not an
/// epoch timestamp — so a forward wall-clock step cannot spuriously trip the
/// stall guard's time budget.
///
/// Only for the reassembler's stall budget (#413). Never for stamping a
/// decoded event: see [`now_ms`].
pub fn mono_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Wall-clock milliseconds since the Unix epoch, for stamping decoded
/// protocol events (`Decoder::push_stream`).
///
/// Must stay on the same clock as `bpsr_app::pipeline::now_ms`: the meter
/// compares an event's `timestamp_ms` directly against that wall clock
/// (idle-timeout and boss-engagement checks in `crates/meter`). #413 moved
/// this whole function to `Instant::elapsed` for the stall guard and took
/// the event stamps with it, so every event looked decades old, idle_timeout
/// fired on the next tick, and `ResetReason::NewFight` wiped the meter on
/// every hit — the v0.3.1 00:00 reset loop. The stall guard now uses
/// [`mono_ms`]; wire timestamps keep this one.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{mono_ms, now_ms};

    /// Issue: the v0.3.1 00:00 reset loop (#413). `Decoder::push_stream`
    /// stamps events the meter compares against `bpsr_app::pipeline::now_ms`'s
    /// wall clock, so the two must share an epoch; only the reassembler's
    /// stall budget may use the monotonic clock. The `push_stream`/
    /// `reassembler.push` wiring this guards lives in `win.rs`, which is
    /// cfg(windows) and not exercised by this test.
    #[test]
    fn now_ms_is_epoch_based_and_mono_ms_is_process_relative() {
        // 2020-01-01T00:00:00Z: unmistakably epoch-based, not process-relative.
        assert!(now_ms() > 1_577_836_800_000, "now_ms is not epoch-based");
        assert!(mono_ms() < now_ms(), "mono_ms should not be epoch-based");
    }
}
