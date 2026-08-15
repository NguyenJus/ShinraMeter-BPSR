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
//! dependencies (`cargo test --workspace --exclude ShinraMeter-BPSR`).

/// Receives what the decoder would otherwise drop or pass over in silence.
/// Every method is a synchronous, fire-and-forget observation call made from
/// the decode hot path (the capture thread in the real app): implementations
/// must not block and must never panic.
pub trait InspectSink: Send + Sync {
    /// A Notify-shaped fragment, for *every* service uuid encountered — not
    /// only `frame::SERVICE_UUID`. This single hook is what both slice A
    /// item 1 (log unrecognized service/method ids instead of dropping them)
    /// and item 4 (the raw frame dump slice B replays offline) are fed from;
    /// implementations tell the two cases apart by comparing `service_uuid`
    /// against `frame::SERVICE_UUID`.
    ///
    /// `payload_decoded` says how to read `payload`: `true` (the ordinary
    /// case) means it is the decompressed bytes `decode::decode_notify`
    /// would consume; `false` means zstd decompression failed and `payload`
    /// is the raw, still-compressed bytes exactly as they arrived. A
    /// fragment whose payload we cannot decompress — a corrupt frame, or one
    /// in a codec we don't speak — is precisely the kind of traffic this
    /// tooling exists to surface, so it reaches the sink like any other
    /// rather than being dropped on the way (the normal, sink-less path
    /// still drops it).
    fn on_notify(
        &self,
        service_uuid: u64,
        method_id: u32,
        payload: &[u8],
        payload_decoded: bool,
        now_ms: u64,
    );

    /// An attr id on entity `uid`'s attr list, for *every* id the
    /// `attrs::player_info_from_attrs` / `attrs::enemy_hp_from_attrs` walks
    /// see with non-empty `raw_data` and a nonzero id — known or not, and on
    /// enemy entities as well as player ones. `known` is `true` when
    /// `attrs::attr_id` has a constant for it (the value was therefore also
    /// decoded into the entity's `PlayerInfo`), `false` when it isn't
    /// (slice A item 3). Widened from an unknowns-only hook (slice B) so a
    /// sink can diff a *known* id like `FIGHT_POINT` across a deliberate
    /// in-game change — the confirmation procedure's control run
    /// (`docs/packet-inspection.md`) needs to see that value move, not just
    /// discover new ids. Implementations that only care about discoveries
    /// filter on `known` themselves.
    fn on_attr(&self, uid: i64, attr_id: i32, raw: &[u8], known: bool);
}
