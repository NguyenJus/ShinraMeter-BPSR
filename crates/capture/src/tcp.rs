//! OS-independent TCP stream reassembly.
//!
//! Turns possibly out-of-order / retransmitted TCP segments into an ordered
//! byte stream. Pure logic, no sockets — host-testable.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

/// Default cap on the accumulated (undrained) byte stream: 10 MiB.
const DEFAULT_MAX_BUFFER: usize = 10 * 1024 * 1024;

/// Reassembles a TCP byte stream from possibly out-of-order / retransmitted
/// segments, handling 32-bit sequence-number wraparound and recovering from
/// a permanent gap (e.g. after a reconnect or zone change) via a stall guard.
pub struct TcpReassembler {
    /// Out-of-order segments waiting for the gap ahead of them to fill in.
    cache: BTreeMap<u32, Vec<u8>>,
    /// Next sequence number expected to extend `buffer`.
    next_seq: Option<u32>,
    /// Contiguous, ordered bytes ready to be handed to the decoder.
    buffer: Vec<u8>,
    /// Upper bound on `buffer`'s size; oldest bytes are dropped past this.
    max_buffer: usize,
    /// Consecutive pushes that made no progress while segments are cached —
    /// once this hits `MAX_STALL_PUSHES`, the reassembler gives up on the
    /// gap and resyncs to the lowest cached sequence number.
    stall_pushes: usize,
    /// Set whenever stream bytes are discarded in a way that breaks byte
    /// contiguity with what a caller (e.g. a stateful protocol decoder) has
    /// already consumed: a buffer-cap trim or a stall-guard resync. Cleared
    /// by [`Self::take_loss`].
    loss: bool,
}

impl TcpReassembler {
    /// Pushes made with no progress (cache non-empty, `next_seq` unmoved)
    /// before the stall guard forces a resync to the lowest cached segment.
    pub const MAX_STALL_PUSHES: usize = 256;

    pub fn new() -> Self {
        Self::with_max_buffer(DEFAULT_MAX_BUFFER)
    }

    pub fn with_max_buffer(max_buffer: usize) -> Self {
        Self {
            cache: BTreeMap::new(),
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
            match self.cache.entry(seq) {
                Entry::Vacant(slot) => {
                    slot.insert(payload.to_vec());
                }
                Entry::Occupied(mut slot) => {
                    if payload.len() > slot.get().len() {
                        slot.insert(payload.to_vec());
                    }
                }
            }
        }

        self.reconcile_stale_cache();

        let after = self.next_seq.expect("set above");
        if after == before && !self.cache.is_empty() {
            self.stall_pushes += 1;
            if self.stall_pushes >= Self::MAX_STALL_PUSHES {
                if let Some(&lowest) = self.cache.keys().next() {
                    self.resync(lowest);
                    self.loss = true;
                }
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
            let Some(seq) = self.cache.keys().copied().find(|&seq| (seq.wrapping_sub(next) as i32) < 0) else {
                return;
            };
            let data = self.cache.remove(&seq).expect("key just found");
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
            let Some(data) = self.cache.remove(&next) else {
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
        self.buffer.clear();
        self.next_seq = Some(seq);
        self.stall_pushes = 0;
        self.loss = false;
    }

    /// Total bytes currently held in out-of-order cache, waiting on a gap.
    pub fn gap_bytes(&self) -> usize {
        self.cache.values().map(Vec::len).sum()
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
}
