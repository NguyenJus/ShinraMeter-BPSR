//! Diagnostic observation hooks (issue #25 slice A: "Add a packet
//! inspection mode to verify unknown opcodes and attr ids").
//!
//! This crate never touches a filesystem, spawns a thread, or otherwise does
//! IO — every decode entry point that can see something worth observing
//! instead accepts an `Option<&dyn InspectSink>` (or, for [`crate::Decoder`]'s
//! long-lived state, an `Option<Arc<dyn InspectSink>>`). When the sink is
//! `None` — the default, and every non-diagnostic call site — the extra work
//! collapses to a single null check on the decode hot path, so a normal run
//! pays nothing beyond that; the pre-#25 dropping behavior is otherwise
//! byte-for-byte unchanged (see `frame::handle_notify`).
//!
//! Aggregation (counts, first-seen timestamps), rate limiting, and any
//! actual file IO belong entirely to the sink implementation, which lives
//! outside this crate — see `crates/app/src/inspect.rs`. Keeping that split
//! is what lets `bpsr-protocol` stay host-testable and free of GUI/app
//! dependencies (`cargo test --workspace --exclude shinra-bpsr`).

/// Receives what the decoder would otherwise drop or pass over in silence.
/// Every method is a synchronous, fire-and-forget observation call made from
/// the decode hot path (the capture thread in the real app): implementations
/// must not block and must never panic.
pub trait InspectSink: Send + Sync {
    /// A Notify-shaped fragment, after decompression (when the outer frame
    /// carried the zstd flag), for *every* service uuid encountered — not
    /// only `frame::SERVICE_UUID`. This single hook is what both slice A
    /// item 1 (log unrecognized service/method ids instead of dropping them)
    /// and item 4 (the raw frame dump slice B replays offline) are fed from;
    /// implementations tell the two cases apart by comparing `service_uuid`
    /// against `frame::SERVICE_UUID`.
    fn on_notify(&self, service_uuid: u64, method_id: u32, payload: &[u8], now_ms: u64);

    /// An attr id on entity `uid`'s attr list with no known constant in
    /// `attrs::attr_id` (slice A item 3).
    fn on_unknown_attr(&self, uid: i64, attr_id: i32, raw: &[u8]);
}
