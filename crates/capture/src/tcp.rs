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
            loss: false,
        }
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
        if after == before && (!self.cache.is_empty() || re_anchored) {
            self.stall_pushes += 1;
            if self.stall_pushes >= Self::MAX_STALL_PUSHES {
                match self.nearest_cached_seq() {
                    // Give up on the gap and restart from the cached segment
                    // nearest ahead of it, in modular order.
                    Some(nearest) => self.resync(nearest),
                    // Nothing cached: the live stream is behind `next_seq`,
                    // so re-anchor on this segment and deliver it.
                    None => {
                        self.resync(seq);
                        self.advance_with(payload);
                    }
                }
                self.loss = true;
            }
        } else {
            self.stall_pushes = 0;
        }
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
    /// Called on server change / zone change / stall recovery.
    pub fn resync(&mut self, seq: u32) {
        self.cache.clear();
        self.cache_cost = 0;
        self.buffer.clear();
        self.next_seq = Some(seq);
        self.stall_pushes = 0;
        self.loss = false;
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
        // recovery: after the stall guard resyncs to the stuck seq, a fresh
        // push at that seq must append immediately instead of being cached
        // forever.
        r.push(5000, b"YES");
        assert_eq!(r.take_stream(), b"YES".to_vec());
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
        payload.extend(std::iter::repeat(b'B').take(200));
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
        expected.extend(std::iter::repeat(b'C').take(50));
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
        expected.extend(std::iter::repeat(b'L').take(100));
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
        expected.extend(std::iter::repeat(b'C').take(50));
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
        // Two cached segments sit ahead of a gap that never fills. In
        // modular sequence order the nearest is 0xFFFF_FFF0; 0x0000_0100 is
        // a wraparound *further* ahead, even though it is the numerically
        // lowest raw u32 (i.e. the first BTreeMap key).
        r.push(0x0000_0100, b"LATER");
        for _ in 0..TcpReassembler::MAX_STALL_PUSHES {
            r.push(0xFFFF_FFF0, b"SOON");
        }
        // Resyncing to 0x0000_0100 would park next_seq ~4 GiB ahead of the
        // live stream, after which every real segment looks like a
        // retransmit and is dropped forever. Resyncing to 0xFFFF_FFF0
        // instead lets the retries land and the stream continue.
        assert!(r.take_stream().ends_with(b"SOON"));
        assert!(r.take_loss());
        r.push(0xFFFF_FFF4, b"OK");
        assert_eq!(r.take_stream(), b"OK".to_vec());
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
        expected.extend(std::iter::repeat(b'N').take(100));
        assert_eq!(r.take_stream(), expected);
    }
}
