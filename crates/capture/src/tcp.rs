//! OS-independent TCP stream reassembly.
//!
//! Turns possibly out-of-order / retransmitted TCP segments into an ordered
//! byte stream. Pure logic, no sockets — host-testable.

use std::collections::BTreeMap;

/// Default cap on the accumulated (undrained) byte stream: 10 MiB.
const DEFAULT_MAX_BUFFER: usize = 10 * 1024 * 1024;

/// Default cap on the accounted size of the out-of-order cache: 2 MiB.
/// Legitimate reordering never holds more than roughly a send window.
const DEFAULT_MAX_CACHE: usize = 2 * 1024 * 1024;

/// Bookkeeping charged per cached segment on top of its payload (BTreeMap
/// node share + `Vec` header + allocator slack), so that the cache cap bounds
/// a flood of tiny far-ahead segments by entry count as well as by bytes.
const CACHE_ENTRY_OVERHEAD: usize = 64;

/// How far behind `next_seq` a segment must start before it is read as the
/// peer re-anchoring the stream rather than as a retransmit. Retransmits and
/// keepalive probes sit within a send window of `next_seq` (TCP's largest
/// scaled window is ~1 GiB, real senders use a few MiB at most); a fresh ISN
/// after a reconnect on the same 4-tuple lands ~1 GiB behind on average.
const MIN_BEHIND_FOR_RESYNC: u32 = 16 * 1024 * 1024;

/// Upper bound on the stall-guard backoff multiplier (see `stall_backoff`).
/// The cache byte/segment cap (`enforce_cache_cap`) bounds memory
/// independently of how patient the guard is, so this only bounds how long
/// a busy, still-recovering stream is made to wait before the guard gives
/// up again — not how much memory waiting costs.
const MAX_STALL_BACKOFF: u32 = 64;

/// `reason` tag [`TcpReassembler::push`]'s stall guard passes to
/// `resync_with_reason` when it re-anchors on the live segment. Unlike an
/// externally driven resync it must *keep* the backoff it has earned — the
/// trip doubles it immediately afterwards (see `stall_backoff`).
const STALL_GUARD_REASON: &str = "stall_guard_re_anchor";

/// Reassembles a TCP byte stream from possibly out-of-order / retransmitted
/// segments, handling 32-bit sequence-number wraparound and recovering from
/// a permanent gap (e.g. after a reconnect or zone change) via a stall guard.
pub struct TcpReassembler {
    /// Out-of-order segments waiting for the gap ahead of them to fill in.
    /// Keyed by raw `u32`, so its iteration order is *not* modular sequence
    /// order across a wraparound — rank with [`Self::nearest_cached_seq`] /
    /// [`Self::furthest_cached_seq`] instead of `keys().next()`.
    cache: BTreeMap<u32, Vec<u8>>,
    /// Accounted size of `cache`: payload bytes plus
    /// [`CACHE_ENTRY_OVERHEAD`] per entry. Maintained incrementally so the
    /// cap check stays O(1) per push.
    cache_cost: usize,
    /// Upper bound on `cache_cost`; the segments furthest ahead of
    /// `next_seq` are evicted past this.
    max_cache: usize,
    /// Next sequence number expected to extend `buffer`.
    next_seq: Option<u32>,
    /// Contiguous, ordered bytes ready to be handed to the decoder.
    buffer: Vec<u8>,
    /// Upper bound on `buffer`'s size; oldest bytes are dropped past this.
    max_buffer: usize,
    /// Consecutive pushes that made no progress while the stream is stuck —
    /// either segments are cached ahead of an unfillable gap, or the peer
    /// re-anchored far behind `next_seq`. Once this hits `MAX_STALL_PUSHES`
    /// the reassembler gives up and resyncs.
    stall_pushes: usize,
    /// Multiplier applied to `MAX_STALL_PUSHES` to get the trip threshold
    /// (see `stall_threshold`). Real gameplay opens gaps in *clusters* —
    /// one resync's new anchor sits right at the front of the next gap —
    /// so re-arming at the same fixed threshold after every trip lets a
    /// merely-late (not lost) segment get raced by another trip before it
    /// can arrive, discarding real data every time (#283). Doubled on each
    /// trip, capped at `MAX_STALL_BACKOFF`; reset to 1 once a push both
    /// advances `next_seq` and leaves nothing cached — the signal that the
    /// stream, not just this one gap, has actually caught up.
    stall_backoff: u32,
    /// Set whenever stream bytes are discarded in a way that breaks byte
    /// contiguity with what a caller (e.g. a stateful protocol decoder) has
    /// already consumed: a buffer-cap trim or a stall-guard resync. Cleared
    /// by [`Self::take_loss`].
    loss: bool,
}

impl TcpReassembler {
    /// Pushes made with no progress (`next_seq` unmoved) before the stall
    /// guard forces a resync.
    pub const MAX_STALL_PUSHES: usize = 256;

    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_BUFFER, DEFAULT_MAX_CACHE)
    }

    pub fn with_max_buffer(max_buffer: usize) -> Self {
        Self::with_limits(max_buffer, DEFAULT_MAX_CACHE)
    }

    /// `max_cache` bounds the out-of-order cache's accounted size (payload
    /// bytes plus a fixed per-entry charge), not raw payload bytes alone.
    pub fn with_limits(max_buffer: usize, max_cache: usize) -> Self {
        Self {
            cache: BTreeMap::new(),
            cache_cost: 0,
            max_cache,
            next_seq: None,
            buffer: Vec::new(),
            max_buffer,
            stall_pushes: 0,
            stall_backoff: 1,
            loss: false,
        }
    }

    /// Current trip threshold: `MAX_STALL_PUSHES` scaled by the backoff
    /// earned by however recently (and unproductively) the guard last
    /// tripped. See `stall_backoff`.
    fn stall_threshold(&self) -> usize {
        Self::MAX_STALL_PUSHES.saturating_mul(self.stall_backoff as usize)
    }

    /// Feeds one TCP segment (`seq`, `payload`) into the reassembler.
    ///
    /// Empty payloads are ignored. The first segment ever pushed establishes
    /// the baseline `next_seq`. Segments behind `next_seq` (already
    /// consumed) are dropped as retransmits, unless they partially overlap
    /// — extend past `next_seq` with genuinely new bytes — in which case
    /// just the new tail is delivered. Segments at `next_seq` extend
    /// `buffer` and drain any now-contiguous cached segments; segments ahead
    /// of `next_seq` are cached until the gap fills in.
    ///
    /// Any bytes discarded in a way that breaks stream contiguity (buffer
    /// trim or stall-guard resync) are
    /// reported via [`Self::take_loss`]; callers holding downstream
    /// stateful state (e.g. a protocol decoder) should reset it when that
    /// returns `true`.
    pub fn push(&mut self, seq: u32, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }

        if self.next_seq.is_none() {
            self.next_seq = Some(seq);
        }
        let before = self.next_seq.expect("just set above");

        // Signed distance from the expected sequence number, wraparound-safe.
        let diff = seq.wrapping_sub(before) as i32;
        if diff < 0 {
            // Behind next_seq — but the segment may still extend past it
            // with new bytes (a coalesced/partial retransmit). Compute the
            // signed distance of the segment's end from next_seq the same
            // wraparound-safe way.
            let end_diff = seq.wrapping_add(payload.len() as u32).wrapping_sub(before) as i32;
            if end_diff > 0 {
                let skip = before.wrapping_sub(seq) as usize;
                self.advance_with(&payload[skip..]);
            }
            // else: fully-consumed retransmit — nothing new, drop it.
        } else if diff == 0 {
            self.advance_with(payload);
        } else {
            // A repacketized retransmit can land on an existing key with
            // *fewer* bytes than the cached segment; overwriting would
            // silently discard the cached tail and stall the stream there.
            let longer_than_cached = match self.cache.get(&seq) {
                Some(cached) => payload.len() > cached.len(),
                None => true,
            };
            if longer_than_cached {
                // Issue #213: logged only on the *opening* of a gap — the
                // empty-to-non-empty transition — not per cached segment.
                // A gap ahead of `next_seq` is the first observable symptom
                // of the #211 wedge, but a busy stream reorders constantly,
                // so one line per gap is the whole budget.
                if self.cache.is_empty() {
                    log::debug!(
                        "tcp: sequence gap opened: next_seq={before} seq={seq} gap={diff} bytes; caching {} bytes ahead of it",
                        payload.len()
                    );
                }
                self.cache_insert(seq, payload.to_vec());
            }
        }

        self.reconcile_stale_cache();
        self.enforce_cache_cap();

        let after = self.next_seq.expect("set above");
        // A segment implausibly far behind `next_seq` is neither a
        // retransmit nor a keepalive probe: the peer re-anchored the stream
        // (e.g. a reconnect on the same 4-tuple with a fresh ISN landing in
        // the "behind" half of the sequence space). That path caches
        // nothing, so it has to drive the stall guard itself — otherwise
        // every segment is dropped as a retransmit and capture stays dead
        // forever with no loss reported.
        let re_anchored = diff < 0 && before.wrapping_sub(seq) >= MIN_BEHIND_FOR_RESYNC;
        if after != before {
            // Forward progress: whatever gap was being waited on is gone.
            self.stall_pushes = 0;
            // Progress with nothing left cached: the stream is fully caught
            // up, not just past the one gap that last tripped. Only this —
            // not merely surviving to the next push — earns back the
            // default threshold.
            if self.cache.is_empty() {
                self.stall_backoff = 1;
            }
        } else if !self.cache.is_empty() || re_anchored {
            self.stall_pushes += 1;
            if self.stall_pushes >= self.stall_threshold() {
                match self.nearest_cached_seq() {
                    // Give up on the gap and restart from the cached segment
                    // nearest ahead of it, in modular order — keeping (not
                    // discarding) its data and whatever is contiguous with
                    // it (#211).
                    Some(nearest) => {
                        // Issue #213: the one line that would have named
                        // #211 in the log. Rate-limited by construction —
                        // the guard fires at most once per
                        // `MAX_STALL_PUSHES` pushes — so `warn` is
                        // affordable for something this abnormal.
                        log::warn!(
                            "tcp: stall guard tripped: stall_pushes={} next_seq={after} nearest={nearest} \
                             gap={} bytes cache_segments={} cache_bytes={}; re-anchoring on the nearest cached segment",
                            self.stall_pushes,
                            nearest.wrapping_sub(after),
                            self.cache.len(),
                            self.gap_bytes(),
                        );
                        self.resync_to_cached(nearest);
                    }
                    // Nothing cached: the live stream is behind `next_seq`,
                    // so re-anchor on this segment and deliver it.
                    None => {
                        log::warn!(
                            "tcp: stall guard tripped: stall_pushes={} next_seq={after} nearest=none; \
                             the peer re-anchored behind next_seq, so re-anchoring on the live segment at seq={seq}",
                            self.stall_pushes,
                        );
                        self.resync_with_reason(seq, STALL_GUARD_REASON);
                        self.advance_with(payload);
                    }
                }
                self.loss = true;
                // The cluster that just tripped the guard is exactly the
                // condition most likely to repeat right away (#283): back
                // off so the next gap gets more real chances to fill
                // before the guard gives up on it too.
                self.stall_backoff = self.stall_backoff.saturating_mul(2).min(MAX_STALL_BACKOFF);
            }
        }
        // Remaining case: no progress, nothing cached, no re-anchor — an
        // ordinary fully-consumed retransmit or duplicate. It is neither a
        // stall (there is no gap to wait on) nor progress, so it must
        // neither count toward the stall counter nor erase backoff earned
        // by a recent trip (#283).
    }

    /// Appends `payload` (already known to start exactly at `next_seq`) to
    /// `buffer`, advances `next_seq` past it, and drains any now-contiguous
    /// cached segments.
    fn advance_with(&mut self, payload: &[u8]) {
        let before = self.next_seq.expect("set by caller");
        self.append_to_buffer(payload);
        self.next_seq = Some(before.wrapping_add(payload.len() as u32));
        self.drain_contiguous();
    }

    /// Reconciles cached segments that start behind `next_seq` — an
    /// overlapping segment can cover and pass a cached segment's start
    /// without ever landing exactly on its key, leaving it unreachable by
    /// [`Self::drain_contiguous`].
    ///
    /// Such an entry is *not* automatically lost data:
    ///
    /// * `end <= next_seq`: every byte it holds was already delivered by the
    ///   covering segment, so dropping it discards nothing — no loss.
    /// * `start < next_seq < end`: it straddles the boundary and its tail
    ///   `[next_seq, end)` is genuinely new data already in hand — splice
    ///   that tail into the stream (which may in turn make further cached
    ///   segments contiguous) instead of throwing it away.
    ///
    /// Either way no bytes are discarded undelivered, so this never reports
    /// loss; the paths that do are the buffer-cap trim and the stall-guard
    /// resync.
    fn reconcile_stale_cache(&mut self) {
        loop {
            let Some(next) = self.next_seq else { return };
            // Wraparound-safe "starts behind next_seq"; BTreeMap key order is
            // raw-u32 order, so scan rather than range.
            let Some(seq) = self
                .cache
                .keys()
                .copied()
                .find(|&seq| (seq.wrapping_sub(next) as i32) < 0)
            else {
                return;
            };
            let data = self.cache_remove(seq).expect("key just found");
            let end = seq.wrapping_add(data.len() as u32);
            if (end.wrapping_sub(next) as i32) > 0 {
                let skip = next.wrapping_sub(seq) as usize;
                self.advance_with(&data[skip..]);
            }
            // else: fully superseded by delivered bytes — drop it silently.
        }
    }

    /// Drains any cached segments that are now contiguous with `buffer`.
    fn drain_contiguous(&mut self) {
        loop {
            let Some(next) = self.next_seq else { return };
            let Some(data) = self.cache_remove(next) else {
                break;
            };
            self.next_seq = Some(next.wrapping_add(data.len() as u32));
            self.append_to_buffer(&data);
        }
    }

    fn append_to_buffer(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
        if self.buffer.len() > self.max_buffer {
            let excess = self.buffer.len() - self.max_buffer;
            self.buffer.drain(0..excess);
            self.loss = true;
            // Issue #213: the other path that raises `loss`, and the one
            // with no other symptom at all — downstream it surfaces only as
            // an unexplained decoder reset. Reaching this at all means the
            // caller has left ~10 MiB undrained, so the volume is bounded
            // by how pathological the situation already is.
            log::warn!(
                "tcp: buffer cap of {} bytes exceeded; discarded_bytes={excess} from the front of \
                 the stream (the delivered stream is no longer contiguous)",
                self.max_buffer,
            );
        }
    }

    /// Hands the accumulated, ordered bytes to the caller (typically the
    /// protocol decoder) and clears the internal buffer.
    pub fn take_stream(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buffer)
    }

    /// Returns whether stream bytes were discarded (buffer-cap trim or
    /// stall-guard resync) since the
    /// last call, resetting the flag. Callers holding downstream stateful
    /// state derived from the byte stream (e.g. a protocol decoder) should
    /// reset it when this returns `true` — the stream is no longer
    /// contiguous with what preceded the gap.
    pub fn take_loss(&mut self) -> bool {
        std::mem::take(&mut self.loss)
    }

    /// Clears cached and buffered state and re-anchors the stream at `seq`.
    ///
    /// Both callers hand it a sequence number taken straight off the live
    /// stream, and neither has anything worth keeping. `win.rs` resyncs when
    /// it adopts a new server connection: everything held belongs to the
    /// abandoned flow's sequence space, so throwing it away is correct.
    /// [`Self::push`]'s stall guard resyncs when the peer re-anchored behind
    /// `next_seq` (a reconnect on the same 4-tuple with a fresh ISN) and the
    /// cache is *empty* — that path drops every segment as a retransmit
    /// rather than caching it, so there is nothing to salvage; when the
    /// cache is non-empty the guard uses [`Self::resync_to_cached`] instead.
    ///
    /// This clears `loss`, since the re-anchor is not itself a discarded-byte
    /// event for the server-change caller; the stall-guard path raises the
    /// flag again after the call, because there the stream really did break.
    pub fn resync(&mut self, seq: u32) {
        self.resync_with_reason(seq, "new_connection");
    }

    /// [`Self::resync`]'s body, with the caller's reason threaded into the
    /// diagnostic (issue #213). Which of the two callers re-anchored the
    /// stream is the single most useful fact about a resync line — an
    /// adopted new connection is routine, the stall guard giving up is not
    /// — and neither is recoverable from the sequence numbers alone.
    fn resync_with_reason(&mut self, seq: u32, reason: &str) {
        // `Option`'s own `Display`-via-`Debug` would render `Some(1234)`,
        // which is noise in a log line and awkward to grep for.
        let old_next_seq = match self.next_seq {
            Some(next) => next.to_string(),
            None => "none".to_string(),
        };
        log::info!(
            "tcp: resync reason={reason} old_next_seq={old_next_seq} new_next_seq={seq} \
             discarded_bytes={} discarded_segments={}",
            self.buffer.len() + self.gap_bytes(),
            self.cache.len(),
        );
        self.cache.clear();
        self.cache_cost = 0;
        self.buffer.clear();
        self.next_seq = Some(seq);
        self.stall_pushes = 0;
        // An externally driven resync — win.rs adopting a brand-new server
        // connection onto this instance — starts a fresh flow, whose gaps
        // have nothing to do with the dead flow's. Carrying the old flow's
        // backoff over would hand it an up-to-64x inflated trip threshold.
        // The stall guard's own re-anchor is the opposite case: its
        // doubling (applied right after this returns) is meant to compound
        // across a gap cluster (#283), so it keeps what it earned.
        if reason != STALL_GUARD_REASON {
            self.stall_backoff = 1;
        }
        self.loss = false;
    }

    /// Re-anchors the stream on `nearest`, a cached segment the stall guard
    /// (see [`Self::push`]) is giving up the unfillable gap in favor of.
    ///
    /// Unlike [`Self::resync`], the cache must survive here: `nearest` came
    /// from [`Self::nearest_cached_seq`], so it — and anything contiguous
    /// with it — is payload already in hand, not a dead flow's leftovers.
    /// Clearing the cache would discard the very segment being re-anchored
    /// on; since this is a passive one-way sniff, the server never
    /// retransmits a segment the real client already ACKed, so `next_seq`
    /// would then point at bytes that never arrive again — a permanent
    /// stall that re-fires the guard every `MAX_STALL_PUSHES` pushes forever
    /// (#211). Draining right after re-anchoring flushes `nearest` and every
    /// segment contiguous with it in one go.
    ///
    /// `buffer` survives for the same reason: it holds bytes assembled from
    /// segments that genuinely arrived in order, which a caller batching its
    /// [`Self::take_stream`] calls has simply not taken yet. The gap they now
    /// sit in front of is what [`Self::take_loss`] reports — the same
    /// contract the buffer-cap trim already relies on — so clearing them
    /// would drop delivered data the caller is owed while telling it nothing
    /// it is not told anyway.
    fn resync_to_cached(&mut self, nearest: u32) {
        self.next_seq = Some(nearest);
        self.loss = true;
        self.stall_pushes = 0;
        self.drain_contiguous();
    }

    /// Total bytes currently held in out-of-order cache, waiting on a gap.
    pub fn gap_bytes(&self) -> usize {
        self.cache.values().map(Vec::len).sum()
    }

    /// Number of out-of-order segments currently cached.
    pub fn gap_segments(&self) -> usize {
        self.cache.len()
    }

    /// Inserts (replacing any entry at `seq`), keeping `cache_cost` in sync.
    fn cache_insert(&mut self, seq: u32, data: Vec<u8>) {
        self.cache_cost += data.len() + CACHE_ENTRY_OVERHEAD;
        if let Some(old) = self.cache.insert(seq, data) {
            self.cache_cost -= old.len() + CACHE_ENTRY_OVERHEAD;
        }
    }

    /// Removes the entry at `seq`, keeping `cache_cost` in sync.
    fn cache_remove(&mut self, seq: u32) -> Option<Vec<u8>> {
        let data = self.cache.remove(&seq)?;
        self.cache_cost -= data.len() + CACHE_ENTRY_OVERHEAD;
        Some(data)
    }

    /// Cached segment nearest ahead of `next_seq` in modular sequence order.
    /// Ranked by wrapping distance: `BTreeMap`'s raw-`u32` key order puts a
    /// wrapped segment (say `0x0000_0100`) *before* an unwrapped one (say
    /// `0xFFFF_FF00`) even though it is ~4 GiB further ahead of the stream.
    fn nearest_cached_seq(&self) -> Option<u32> {
        let next = self.next_seq?;
        self.cache
            .keys()
            .copied()
            .min_by_key(|&seq| seq.wrapping_sub(next))
    }

    /// Cached segment furthest ahead of `next_seq` in modular sequence order.
    fn furthest_cached_seq(&self) -> Option<u32> {
        let next = self.next_seq?;
        self.cache
            .keys()
            .copied()
            .max_by_key(|&seq| seq.wrapping_sub(next))
    }

    /// Bounds the out-of-order cache, evicting the segments furthest ahead of
    /// `next_seq` first.
    ///
    /// `buffer` is capped but the cache used to be bounded only by the stall
    /// guard, whose counter resets on *any* forward progress. A sniffer
    /// accepts every packet matching the 4-tuple (no checksum or window
    /// validation), so a stream alternating one in-order segment with
    /// far-ahead junk keeps `stall_pushes` at 0 while the cache grows until
    /// RAM runs out. Far-ahead segments are evicted first: legitimate
    /// reordering sits close to `next_seq`, so those entries are the most
    /// speculative and the least likely to ever drain.
    ///
    /// Eviction deliberately does not report loss. The discarded bytes were
    /// never delivered and sit behind a gap that has not filled, so the
    /// delivered prefix stays contiguous and downstream decoder state stays
    /// valid; flagging loss here would reset the decoder on every junk
    /// packet. If the gap does later fill, the hole stops
    /// [`Self::drain_contiguous`] and the stall guard reports the break then.
    fn enforce_cache_cap(&mut self) {
        while self.cache_cost > self.max_cache {
            let Some(furthest) = self.furthest_cached_seq() else {
                break;
            };
            self.cache_remove(furthest);
        }
    }
}

impl Default for TcpReassembler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_segments_reassemble() {
        let mut r = TcpReassembler::new();
        r.push(100, b"abc");
        r.push(103, b"def");
        assert_eq!(r.take_stream(), b"abcdef".to_vec());
    }

    #[test]
    fn out_of_order_segments_reassemble_in_order() {
        let mut r = TcpReassembler::new();
        // baseline segment establishes next_seq
        r.push(100, b"AAA");
        // third segment arrives before the second: must be cached, not lost
        r.push(106, b"CCC");
        // second segment fills the gap and should drain the cached third
        r.push(103, b"BBB");
        assert_eq!(r.take_stream(), b"AAABBBCCC".to_vec());
    }

    #[test]
    fn duplicate_retransmit_ignored() {
        let mut r = TcpReassembler::new();
        r.push(100, b"AAA");
        // retransmit of already-consumed bytes must be dropped, not re-appended
        r.push(100, b"AAA");
        assert_eq!(r.take_stream(), b"AAA".to_vec());
    }

    #[test]
    fn stall_guard_resyncs_on_permanent_gap() {
        let mut r = TcpReassembler::new();
        r.push(1000, b"AAA"); // next_seq = 1003
        // segment at 5000 can never be drained because the gap [1003,5000)
        // will never arrive; the stall guard must eventually give up on it.
        for _ in 0..TcpReassembler::MAX_STALL_PUSHES {
            r.push(5000, b"X");
        }
        // The guard re-anchors on the cached segment (5000) *and delivers
        // it* (#211) instead of discarding it, so next_seq lands just past
        // it at 5001. The baseline "AAA" was never drained, and a re-anchor
        // does not throw away bytes the caller has yet to take, so it is
        // still at the front of the stream.
        assert_eq!(r.take_stream(), b"AAAX".to_vec());
        // recovery: a fresh segment right after it continues to be
        // delivered immediately instead of being cached forever.
        r.push(5001, b"YES");
        assert_eq!(r.take_stream(), b"YES".to_vec());
    }

    #[test]
    fn stall_guard_resync_delivers_cached_bytes_instead_of_dropping_them() {
        // Regression test for #211: the two tests above re-push the SAME
        // seq after the guard fires, a retransmit shape a one-way passive
        // sniff of a *permanently* lost segment never produces (the real
        // client already ACKed it; the server never sends it again). This
        // reproduces the real shape: a permanent gap, then a run of
        // DISTINCT never-repeated segments past it.
        let mut r = TcpReassembler::new();
        r.push(1000, b"AAA"); // next_seq = 1003
        let _ = r.take_stream();

        // [1003, 2000) is a permanent gap: never pushed, not even once.
        const SEG_LEN: u32 = 100;
        const SEGS: u32 = TcpReassembler::MAX_STALL_PUSHES as u32;
        for i in 0..SEGS {
            let seq = 2000 + i * SEG_LEN;
            // Distinct, non-retransmitted payload per segment, and the
            // segments are laid out contiguously so a correct resync that
            // keeps the cache drains the whole run in one go.
            let payload = vec![(i % 256) as u8; SEG_LEN as usize];
            r.push(seq, &payload);
        }

        // The guard trips on the 256th push. It must re-anchor on the
        // cached segment nearest the gap (seq 2000) *without* discarding
        // it — and its contiguous successors must drain right along with
        // it.
        let delivered = r.take_stream();
        assert_eq!(
            delivered.len(),
            (SEGS * SEG_LEN) as usize,
            "cached segments were dropped instead of delivered"
        );
        assert_eq!(delivered[0], 0u8);
        assert_eq!(delivered[delivered.len() - 1], ((SEGS - 1) % 256) as u8);
        assert!(r.take_loss());

        // The stream must keep working afterwards: a brand-new segment
        // right after the drained chain is delivered immediately, not
        // cached forever.
        let resumed_seq = 2000 + SEGS * SEG_LEN;
        r.push(resumed_seq, b"NEW");
        assert_eq!(r.take_stream(), b"NEW".to_vec());
    }

    #[test]
    fn sequence_wraparound_is_handled() {
        let mut r = TcpReassembler::new();
        // u32::MAX - 2 + 3 wraps past u32::MAX back to 0
        r.push(u32::MAX - 2, b"AAA");
        r.push(0, b"BBB");
        assert_eq!(r.take_stream(), b"AAABBB".to_vec());
    }

    #[test]
    fn buffer_cap_is_honoured() {
        let mut r = TcpReassembler::with_max_buffer(5);
        r.push(0, b"ABCDEFGHIJ");
        let out = r.take_stream();
        assert_eq!(out.len(), 5);
        assert_eq!(out, b"FGHIJ".to_vec());
    }

    #[test]
    fn empty_payload_is_ignored() {
        let mut r = TcpReassembler::new();
        r.push(100, b"");
        assert_eq!(r.take_stream(), Vec::<u8>::new());
    }

    #[test]
    fn resync_clears_cache_and_buffer_and_sets_next_seq() {
        let mut r = TcpReassembler::new();
        r.push(100, b"AAA");
        r.push(200, b"out-of-order");
        r.resync(500);
        assert_eq!(r.take_stream(), Vec::<u8>::new());
        assert_eq!(r.gap_bytes(), 0);
        r.push(500, b"fresh");
        assert_eq!(r.take_stream(), b"fresh".to_vec());
    }

    #[test]
    fn gap_bytes_reports_cached_out_of_order_size() {
        let mut r = TcpReassembler::new();
        r.push(100, b"AAA");
        r.push(200, b"12345");
        assert_eq!(r.gap_bytes(), 5);
    }

    #[test]
    fn partially_overlapping_retransmit_delivers_new_tail_bytes() {
        let mut r = TcpReassembler::new();
        r.push(1000, &[b'A'; 100]); // [1000,1100) -> next_seq = 1100
        let _ = r.take_stream();
        // Coalesced retransmit [1000,1300): re-sends the already-consumed
        // [1000,1100) prefix but also carries genuinely new bytes
        // [1100,1300) that must not be dropped.
        let mut payload = vec![b'A'; 100];
        payload.extend(std::iter::repeat_n(b'B', 200));
        r.push(1000, &payload);
        assert_eq!(r.take_stream(), vec![b'B'; 200]);
    }

    #[test]
    fn fully_consumed_retransmit_still_dropped() {
        let mut r = TcpReassembler::new();
        r.push(1000, &[b'A'; 100]); // next_seq = 1100
        let _ = r.take_stream();
        r.push(1000, &[b'A'; 100]); // fully behind next_seq: nothing new
        assert_eq!(r.take_stream(), Vec::<u8>::new());
    }

    #[test]
    fn in_order_push_reports_no_loss() {
        let mut r = TcpReassembler::new();
        r.push(100, b"abc");
        assert!(!r.take_loss());
    }

    #[test]
    fn buffer_overflow_trim_reports_loss() {
        let mut r = TcpReassembler::with_max_buffer(5);
        r.push(0, b"ABCDEFGHIJ");
        assert!(r.take_loss());
    }

    #[test]
    fn take_loss_resets_flag_after_reporting() {
        let mut r = TcpReassembler::with_max_buffer(5);
        r.push(0, b"ABCDEFGHIJ");
        assert!(r.take_loss());
        assert!(!r.take_loss());
    }

    #[test]
    fn stall_guard_resync_reports_loss() {
        let mut r = TcpReassembler::new();
        r.push(1000, b"AAA");
        for _ in 0..TcpReassembler::MAX_STALL_PUSHES {
            r.push(5000, b"X");
        }
        assert!(r.take_loss());
    }

    #[test]
    fn overlapping_advance_purges_fully_covered_cache_entry_without_loss() {
        let mut r = TcpReassembler::new();
        r.push(100, &[b'A'; 50]); // next_seq = 150
        let _ = r.take_stream();
        r.push(300, b"stale"); // cached out-of-order; gap [150,300) unfilled
        assert_eq!(r.gap_bytes(), 5);
        // This segment covers [150,400), advancing past the cached entry's
        // key (300) without ever landing exactly on it, so drain_contiguous
        // can never reach it and it must be purged. But every byte the entry
        // held was just delivered by the covering segment, so nothing is
        // discarded: reporting loss here would needlessly reset the decoder.
        r.push(150, &[b'B'; 250]);
        assert_eq!(r.gap_bytes(), 0);
        assert_eq!(r.take_stream(), vec![b'B'; 250]);
        assert!(!r.take_loss());
    }

    #[test]
    fn cache_entry_straddling_next_seq_has_its_tail_spliced_without_loss() {
        let mut r = TcpReassembler::new();
        r.push(100, &[b'A'; 50]); // next_seq = 150
        let _ = r.take_stream();
        r.push(300, &[b'C'; 100]); // cached [300,400)
        // [150,350) advances next_seq into the middle of the cached entry.
        // Its tail [350,400) is genuinely new data already in hand: it must
        // be spliced into the stream, not thrown away as "stale".
        r.push(150, &[b'B'; 200]);
        let mut expected = vec![b'B'; 200];
        expected.extend(std::iter::repeat_n(b'C', 50));
        assert_eq!(r.take_stream(), expected);
        assert_eq!(r.gap_bytes(), 0);
        assert!(!r.take_loss());
    }

    #[test]
    fn shorter_retransmit_does_not_truncate_a_longer_cached_segment() {
        let mut r = TcpReassembler::new();
        r.push(100, b"AAA"); // next_seq = 103; [103,300) is an open gap
        let _ = r.take_stream();
        r.push(300, &[b'L'; 100]); // cached [300,400)
        // A repacketized retransmit re-sends only [300,350). Overwriting the
        // cache entry would silently discard the cached [350,400) bytes and
        // stall the stream at 350 until the guard fires.
        r.push(300, &[b'S'; 50]);
        assert_eq!(r.gap_bytes(), 100);
        r.push(103, &[b'G'; 197]); // fills the gap, draining the cached entry
        let mut expected = vec![b'G'; 197];
        expected.extend(std::iter::repeat_n(b'L', 100));
        assert_eq!(r.take_stream(), expected);
        assert!(!r.take_loss());
    }

    #[test]
    fn straddling_splice_handles_sequence_wraparound() {
        let mut r = TcpReassembler::new();
        let base = u32::MAX - 99;
        r.push(base, &[b'A'; 50]); // next_seq = base + 50
        let _ = r.take_stream();
        // [base+100, base+200) wraps past u32::MAX (key 0 in the cache).
        r.push(base.wrapping_add(100), &[b'C'; 100]);
        // advances next_seq to base+150, i.e. into the middle of that entry
        r.push(base.wrapping_add(50), &[b'B'; 100]);
        let mut expected = vec![b'B'; 100];
        expected.extend(std::iter::repeat_n(b'C', 50));
        assert_eq!(r.take_stream(), expected);
        assert_eq!(r.gap_bytes(), 0);
        assert!(!r.take_loss());
    }

    #[test]
    fn stall_guard_resyncs_to_modular_lowest_cached_segment_across_wraparound() {
        let mut r = TcpReassembler::new();
        let base = 0xFFFF_FF00u32;
        r.push(base, b"AAA"); // next_seq = base + 3
        let _ = r.take_stream();
        // A wraparound segment sits far ahead: 0x0000_0100 is the
        // numerically lowest raw u32 in the cache (the first BTreeMap key)
        // yet ~4 GiB *further* ahead of the stream than anything near
        // 0xFFFF_FFxx.
        r.push(0x0000_0100, b"LATER");
        // The live stream resumes at 0xFFFF_FFF0, past a permanent gap. As
        // in the #211 regression test above, these are DISTINCT
        // never-repeated one-byte segments — a passive sniff of a lost
        // segment never sees the server re-send it — laid out contiguously
        // and stopping short of 0x0000_0100 so they stay a separate run.
        // That first push already counted towards the guard, so one fewer
        // segment trips it exactly on the last iteration, with no trailing
        // push left over to mask a wrong re-anchor.
        const SEGS: u32 = TcpReassembler::MAX_STALL_PUSHES as u32 - 1;
        let resumed = 0xFFFF_FFF0u32;
        for i in 0..SEGS {
            r.push(resumed.wrapping_add(i), &[i as u8]);
        }
        // Re-anchoring on 0x0000_0100 would park next_seq ~4 GiB ahead of
        // the live stream, after which every real segment looks like a
        // retransmit and is dropped forever. Re-anchoring on 0xFFFF_FFF0
        // instead delivers the whole cached run in one drain.
        let delivered = r.take_stream();
        assert_eq!(
            delivered.len(),
            SEGS as usize,
            "re-anchored on the wrong cached segment, or dropped the cache"
        );
        assert_eq!(delivered[0], 0u8);
        assert_eq!(delivered[delivered.len() - 1], (SEGS - 1) as u8);
        assert!(r.take_loss());
        // Only the far-ahead wraparound segment is still waiting, and the
        // stream keeps working: the next live segment lands at next_seq.
        assert_eq!(r.gap_bytes(), b"LATER".len());
        r.push(resumed.wrapping_add(SEGS), b"OK");
        assert_eq!(r.take_stream(), b"OK".to_vec());
    }

    #[test]
    fn stall_guard_resync_keeps_bytes_the_caller_has_not_drained_yet() {
        let mut r = TcpReassembler::new();
        // Assembled in order but never taken: a caller that batches its
        // take_stream() calls still owns these bytes when the guard fires,
        // and the re-anchor is about *not* discarding data genuinely in
        // hand — the cache is not the only place such data lives.
        r.push(1000, b"AAA"); // next_seq = 1003
        r.push(1003, b"BBB"); // next_seq = 1006
        // [1006, 5000) is a permanent gap; the run past it never repeats,
        // and its 256th push is what trips the guard.
        const SEGS: u32 = TcpReassembler::MAX_STALL_PUSHES as u32;
        for i in 0..SEGS {
            r.push(5000 + i, b"C");
        }
        // The guard re-anchors on the cached run and drains it, but the
        // earlier "AAABBB" was never handed out: dropping it would lose
        // real contiguous bytes at exactly the moment this path exists to
        // prevent loss. take_loss() is what tells the caller a gap sits
        // between the two halves.
        let delivered = r.take_stream();
        assert_eq!(delivered.len(), 6 + SEGS as usize);
        assert!(
            delivered.starts_with(b"AAABBB"),
            "undrained stream bytes were discarded by the stall-guard resync"
        );
        assert!(r.take_loss());
    }

    #[test]
    fn stall_guard_recovers_when_the_stream_re_anchors_behind_next_seq() {
        let mut r = TcpReassembler::new();
        r.push(0x8000_0000, b"AAA"); // next_seq = 0x8000_0003
        let _ = r.take_stream();
        // The 4-tuple is reused after a reconnect and the fresh ISN lands in
        // the "behind" half of the sequence space (~50% likely). Every
        // segment then looks like an already-consumed retransmit: dropped,
        // never cached, so a cache-only stall guard never fires and capture
        // is permanently dead with no loss reported.
        let fresh = 0x4000_0000u32;
        for i in 0..TcpReassembler::MAX_STALL_PUSHES as u32 {
            r.push(fresh.wrapping_add(i * 3), b"BBB");
        }
        r.push(
            fresh.wrapping_add(TcpReassembler::MAX_STALL_PUSHES as u32 * 3),
            b"CCC",
        );
        assert_eq!(r.take_stream(), b"BBBCCC".to_vec());
        assert!(r.take_loss());
    }

    #[test]
    fn repeated_keepalive_probes_do_not_force_a_resync() {
        let mut r = TcpReassembler::new();
        r.push(1000, b"AAA"); // next_seq = 1003
        let _ = r.take_stream();
        // A TCP keepalive probe is one garbage byte at next_seq - 1. A
        // long-idle connection sends far more of them than the stall
        // threshold; they must not be mistaken for a re-anchored stream.
        for _ in 0..TcpReassembler::MAX_STALL_PUSHES * 2 {
            r.push(1002, b"\0");
        }
        assert!(!r.take_loss());
        assert_eq!(r.take_stream(), Vec::<u8>::new());
        r.push(1003, b"BBB");
        assert_eq!(r.take_stream(), b"BBB".to_vec());
    }

    #[test]
    fn out_of_order_cache_is_bounded_while_the_stream_keeps_progressing() {
        let mut r = TcpReassembler::new();
        r.push(0, b"A"); // next_seq = 1
        // One in-order byte per far-ahead 1 KiB segment: the forward
        // progress pins stall_pushes at 0, so the stall guard never bounds
        // the cache. Only a cache cap can.
        for i in 0..8_000u32 {
            r.push(1 + i, b".");
            r.push(1_000_000 + i * 1024, &[b'X'; 1024]);
        }
        assert!(
            r.gap_bytes() <= 4 * 1024 * 1024,
            "unbounded out-of-order cache: {} bytes",
            r.gap_bytes()
        );
        // Evicting speculative far-ahead bytes does not break contiguity of
        // what was already delivered, so it must not reset the decoder.
        assert!(!r.take_loss());
    }

    #[test]
    fn out_of_order_cache_bounds_a_flood_of_tiny_far_ahead_segments() {
        let mut r = TcpReassembler::with_limits(64 * 1024, 1024);
        r.push(0, b"A"); // next_seq = 1
        for i in 0..5_000u32 {
            r.push(1 + i, b"."); // forward progress: the stall guard never fires
            r.push(1_000_000 + i * 4, b"X"); // 1-byte far-ahead junk
        }
        // A payload-bytes-only cap would still admit ~1000 near-empty
        // entries, each costing far more than its single byte of payload;
        // the per-entry charge bounds entry count too.
        assert!(
            r.gap_segments() <= 32,
            "{} cached segments",
            r.gap_segments()
        );
    }

    #[test]
    fn cache_eviction_keeps_the_segments_nearest_the_gap() {
        let mut r = TcpReassembler::with_limits(64 * 1024, 300);
        r.push(0, &[b'A'; 3]); // next_seq = 3
        let _ = r.take_stream();
        r.push(103, &[b'N'; 100]); // just past a small gap: still drainable
        r.push(10_000, &[b'F'; 100]); // far-ahead junk
        r.push(20_000, &[b'F'; 100]); // far-ahead junk
        // Only one 100-byte entry fits under the cap, so eviction must drop
        // the speculative far-ahead junk, not the segment behind the gap.
        r.push(3, &[b'G'; 100]); // fills [3,103)
        let mut expected = vec![b'G'; 100];
        expected.extend(std::iter::repeat_n(b'N', 100));
        assert_eq!(r.take_stream(), expected);
    }

    /// Issue #213: the reassembler used to contain zero `log::` calls, so
    /// the 24 minutes of dead capture in #211 produced 24 minutes of total
    /// log silence. These drive the real `push`/`resync` paths and assert
    /// on what actually reached the `log` facade.
    ///
    /// `log` allows exactly one logger per process, so the capture buffer
    /// below is shared with every other test in this binary. Assertions on
    /// it must therefore be *positive* ("this exact line was logged") — an
    /// absence says nothing — and each test must use a sequence-number base
    /// no other test in this file uses, so a matched line can only have
    /// come from the test that logged it.
    mod diagnostics {
        use super::*;
        use std::sync::{Mutex, Once};

        static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());
        static CAPTURE_LOGGER: CaptureLogger = CaptureLogger;

        struct CaptureLogger;

        impl log::Log for CaptureLogger {
            fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
                true
            }

            fn log(&self, record: &log::Record<'_>) {
                if let Ok(mut captured) = CAPTURED.lock() {
                    captured.push(record.args().to_string());
                }
            }

            fn flush(&self) {}
        }

        /// Installs [`CAPTURE_LOGGER`] once per process. Idempotent, so any
        /// number of tests can call it, in any order, from any thread.
        fn install_capture() {
            static INSTALL: Once = Once::new();
            INSTALL.call_once(|| {
                let _ = log::set_logger(&CAPTURE_LOGGER);
                log::set_max_level(log::LevelFilter::Trace);
            });
        }

        /// Whether any captured line contains `needle`.
        fn logged(needle: &str) -> bool {
            CAPTURED
                .lock()
                .map(|captured| captured.iter().any(|line| line.contains(needle)))
                .unwrap_or(false)
        }

        /// How many captured lines contain every one of `needles`.
        fn count_logged(needles: &[&str]) -> usize {
            CAPTURED
                .lock()
                .map(|captured| {
                    captured
                        .iter()
                        .filter(|line| needles.iter().all(|needle| line.contains(needle)))
                        .count()
                })
                .unwrap_or(0)
        }

        /// Everything captured so far, for a failing assertion's message.
        fn dump() -> String {
            CAPTURED
                .lock()
                .map(|captured| captured.join("\n"))
                .unwrap_or_default()
        }

        /// The gap that opens ahead of `next_seq` is the first observable
        /// symptom of a stream about to wedge, and nothing used to say so.
        #[test]
        fn a_sequence_gap_opening_is_logged_with_both_ends() {
            install_capture();
            let base: u32 = 0x0121_0000;
            let mut r = TcpReassembler::new();
            r.push(base, b"AAA");
            r.push(base + 0x1000, b"BBB");

            assert!(logged("sequence gap"), "no gap line at all:\n{}", dump());
            assert!(
                logged(&format!("next_seq={}", base + 3)),
                "the gap line must name the byte the stream is waiting on:\n{}",
                dump()
            );
            assert!(
                logged(&format!("seq={}", base + 0x1000)),
                "the gap line must name the segment that arrived early:\n{}",
                dump()
            );
        }

        /// A gap that stays open must not re-log on every segment behind
        /// it — the whole point of #213 is low-volume logging a user can
        /// actually hand over.
        #[test]
        fn a_gap_that_stays_open_is_logged_only_once() {
            install_capture();
            let base: u32 = 0x0221_0000;
            let mut r = TcpReassembler::new();
            r.push(base, b"AAA");
            for i in 0..50u32 {
                r.push(base + 0x1000 + i * 8, b"BBB");
            }

            assert_eq!(
                count_logged(&["sequence gap", &format!("next_seq={}", base + 3)]),
                1,
                "one open gap must produce exactly one line:\n{}",
                dump()
            );
        }

        /// The #211 wedge itself. The trip has to name what it gave up on
        /// and what it re-anchored to, or the log still cannot tell a
        /// reassembly stall from packets never arriving at all.
        #[test]
        fn a_stall_guard_trip_is_logged_with_the_anchor_it_gave_up_on() {
            install_capture();
            let base: u32 = 0x0321_0000;
            let mut r = TcpReassembler::new();
            r.push(base, b"AAA");
            for _ in 0..TcpReassembler::MAX_STALL_PUSHES {
                r.push(base + 0x1000, b"X");
            }

            assert!(
                logged("stall guard"),
                "no stall-guard line at all:\n{}",
                dump()
            );
            assert!(
                logged(&format!("next_seq={}", base + 3)),
                "the trip must name the anchor it gave up on:\n{}",
                dump()
            );
            assert!(
                logged(&format!("nearest={}", base + 0x1000)),
                "the trip must name the cached segment it re-anchors to:\n{}",
                dump()
            );
            assert!(
                logged(&format!(
                    "stall_pushes={}",
                    TcpReassembler::MAX_STALL_PUSHES
                )),
                "the trip must name the counter that fired:\n{}",
                dump()
            );
        }

        /// The new-connection resync in `win.rs` throws away everything it
        /// holds; how much it threw away is exactly what distinguishes a
        /// clean zone change from one that ate a raid's worth of stream.
        #[test]
        fn a_resync_logs_the_old_anchor_the_new_one_and_what_it_discarded() {
            install_capture();
            let base: u32 = 0x0421_0000;
            let mut r = TcpReassembler::new();
            r.push(base, b"AAAAA"); // 5 undrained buffered bytes
            r.push(base + 0x1000, b"BBB"); // 3 cached bytes behind a gap
            r.resync(base + 0x9000);

            assert!(logged("resync"), "no resync line at all:\n{}", dump());
            assert!(
                logged("reason=new_connection"),
                "a resync must say why it happened:\n{}",
                dump()
            );
            assert!(
                logged(&format!("old_next_seq={}", base + 5)),
                "the resync must name the anchor it left:\n{}",
                dump()
            );
            assert!(
                logged(&format!("new_next_seq={}", base + 0x9000)),
                "the resync must name the anchor it took:\n{}",
                dump()
            );
            assert!(
                logged("discarded_bytes=8"),
                "the resync must name what it threw away (5 buffered + 3 cached):\n{}",
                dump()
            );
        }

        /// A buffer-cap trim silently drops the oldest stream bytes and
        /// used to show up only as an unexplained downstream decoder reset.
        #[test]
        fn a_buffer_cap_trim_is_logged_as_loss() {
            install_capture();
            let base: u32 = 0x0521_0000;
            let mut r = TcpReassembler::with_max_buffer(8);
            r.push(base, b"AAAAAAAA");
            r.push(base + 8, b"BBBB");

            assert!(
                logged("buffer cap") && logged("discarded_bytes=4"),
                "expected a trim line naming the bytes dropped:\n{}",
                dump()
            );
        }

        /// Regression test for #283: real gameplay logs showed the stall
        /// guard tripping ~19 times in under 3 minutes, each trip
        /// permanently discarding a few hundred to a few thousand bytes and
        /// starving the decoder — not one clean permanent gap (which the
        /// tests above already cover) but a *cluster* of gaps opening in
        /// quick succession right after each resync. The eventual recovery
        /// (a single 60s heartbeat flushing the whole backlog at once) shows
        /// the "missing" bytes were never actually gone — just late.
        ///
        /// This reproduces that shape: a first cluster of distinct segments
        /// trips the guard once (identical to
        /// `stall_guard_resync_delivers_cached_bytes_instead_of_dropping_them`),
        /// then a second cluster starts immediately after the resync — but
        /// this time the segment that fills the new gap merely arrives
        /// *late* (after more pushes than the old fixed 256-push threshold
        /// allowed, but the stream is still clearly busy and progressing).
        /// The guard must back off after a trip instead of re-arming at the
        /// same fixed threshold, so a merely-late segment still heals the
        /// stream instead of being raced by a second, data-discarding trip.
        #[test]
        fn a_late_arriving_segment_recovers_without_a_second_trip() {
            install_capture();
            let base: u32 = 0x0621_0000;
            let mut r = TcpReassembler::new();
            r.push(base, b"A"); // next_seq = base + 1

            // First cluster: a permanent gap immediately behind `next_seq`,
            // then exactly MAX_STALL_PUSHES distinct, mutually-contiguous
            // segments cached ahead of it. The guard trips once, re-anchors
            // on the nearest cached segment, and (per #211) drains the
            // whole contiguous run right along with it.
            const SEG_LEN: u32 = 4;
            const SEGS: u32 = TcpReassembler::MAX_STALL_PUSHES as u32;
            let cluster1 = base + 0x1000;
            for i in 0..SEGS {
                r.push(cluster1 + i * SEG_LEN, &[b'X'; SEG_LEN as usize]);
            }
            let trip1_marker = format!("next_seq={}", base + 1);
            assert_eq!(
                count_logged(&["stall guard tripped", &trip1_marker]),
                1,
                "expected exactly one trip after the first cluster:\n{}",
                dump()
            );

            // Second cluster: a brand-new gap opens right where the first
            // resync left `next_seq` — exactly the log's observed pattern.
            // Feed more than the old fixed threshold's worth of distinct,
            // far-ahead segments (which keep the stream looking "stalled"
            // by the old no-progress-count metric) before the segment that
            // actually fills the new gap shows up.
            let gap2_next_seq = cluster1 + SEGS * SEG_LEN;
            let cluster2 = gap2_next_seq + 0x1000;
            const LATE_AFTER: u32 = TcpReassembler::MAX_STALL_PUSHES as u32 + 44; // > old threshold
            for i in 0..LATE_AFTER {
                r.push(cluster2 + i * SEG_LEN, &[b'Y'; SEG_LEN as usize]);
            }
            // Old fixed-256 behaviour would have already tripped a second
            // time inside that loop, discarding real, still-arriving data.
            let trip2_marker = format!("next_seq={gap2_next_seq}");
            assert_eq!(
                count_logged(&["stall guard tripped", &trip2_marker]),
                0,
                "a second trip fired before the late segment had a chance to arrive:\n{}",
                dump()
            );

            // The segment that fills the second gap was merely late, not
            // lost: pushing it now must extend the stream immediately, with
            // no data ever discarded for this gap.
            r.push(gap2_next_seq, b"LATE");
            assert_eq!(
                count_logged(&["stall guard tripped", &trip2_marker]),
                0,
                "the late segment's arrival must not itself trigger a trip:\n{}",
                dump()
            );

            let delivered = r.take_stream();
            assert!(
                delivered.windows(4).any(|w| w == b"LATE"),
                "the late-but-not-lost segment must reach the decoder:\n{:?}",
                delivered
            );
        }

        /// Trips the guard once on a permanent gap at `base + 1`, leaving
        /// `r` with an empty cache, `next_seq` just past a drained cluster,
        /// and a doubled stall backoff. Returns the new `next_seq`.
        fn trip_once(r: &mut TcpReassembler, base: u32) -> u32 {
            const SEG_LEN: u32 = 4;
            const SEGS: u32 = TcpReassembler::MAX_STALL_PUSHES as u32;
            r.push(base, b"A"); // next_seq = base + 1
            let cluster = base + 0x1000;
            for i in 0..SEGS {
                r.push(cluster + i * SEG_LEN, &[b'X'; SEG_LEN as usize]);
            }
            assert_eq!(
                count_logged(&["stall guard tripped", &format!("next_seq={}", base + 1)]),
                1,
                "setup expected exactly one trip:\n{}",
                dump()
            );
            assert_eq!(r.gap_segments(), 0, "setup expected a drained cache");
            cluster + SEGS * SEG_LEN
        }

        /// Regression test for the reset in `push`'s tail firing on pushes
        /// that made no progress at all. A fully-consumed retransmit or
        /// duplicate arriving while the cache is empty is neither progress
        /// nor a stall, so it must not erase the backoff a recent trip
        /// earned — otherwise one stray duplicate between two gap clusters
        /// re-arms the guard at the fixed threshold #283 exists to avoid.
        #[test]
        fn a_consumed_duplicate_does_not_reset_the_backoff() {
            install_capture();
            let base: u32 = 0x0721_0000;
            let mut r = TcpReassembler::new();
            let next_seq = trip_once(&mut r, base);
            let _ = r.take_stream();

            // A duplicate of bytes already delivered, arriving with nothing
            // cached: entirely behind `next_seq`, and far too close to it to
            // read as the peer re-anchoring.
            r.push(base + 0x1000, &[b'X'; 4]);

            // The backoff must still be 2, so a fresh cluster of exactly
            // MAX_STALL_PUSHES no-progress pushes stays under the threshold.
            const SEG_LEN: u32 = 4;
            let cluster = next_seq + 0x1000;
            for i in 0..TcpReassembler::MAX_STALL_PUSHES as u32 {
                r.push(cluster + i * SEG_LEN, &[b'Y'; SEG_LEN as usize]);
            }
            assert_eq!(
                count_logged(&["stall guard tripped", &format!("next_seq={next_seq}")]),
                0,
                "the duplicate reset the earned backoff, so the guard tripped early:\n{}",
                dump()
            );

            // And the merely-late segment filling that gap still heals it.
            r.push(next_seq, b"LATE");
            let delivered = r.take_stream();
            assert!(
                delivered.windows(4).any(|w| w == b"LATE"),
                "the late segment must reach the decoder:\n{delivered:?}"
            );
        }

        /// A resync driven from outside (win.rs adopting a brand-new server
        /// connection onto the same reassembler) starts an unrelated flow;
        /// it must not inherit the dead flow's inflated trip threshold.
        #[test]
        fn adopting_a_new_connection_resets_the_backoff() {
            install_capture();
            let base: u32 = 0x0821_0000;
            let mut r = TcpReassembler::new();
            trip_once(&mut r, base);
            let _ = r.take_stream();

            // New connection, fresh ISN.
            let fresh: u32 = 0x0931_0000;
            r.resync(fresh);
            let _ = r.take_stream();

            // Back at the default threshold: a permanent gap here must trip
            // after MAX_STALL_PUSHES pushes, not 2x that.
            const SEG_LEN: u32 = 4;
            let cluster = fresh + 0x1000;
            for i in 0..TcpReassembler::MAX_STALL_PUSHES as u32 {
                r.push(cluster + i * SEG_LEN, &[b'Z'; SEG_LEN as usize]);
            }
            assert_eq!(
                count_logged(&["stall guard tripped", &format!("next_seq={fresh}")]),
                1,
                "the adopted connection inherited the old flow's backoff:\n{}",
                dump()
            );
        }
    }
}
