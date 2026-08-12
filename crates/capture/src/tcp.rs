//! OS-independent TCP stream reassembly.
//!
//! Turns possibly out-of-order / retransmitted TCP segments into an ordered
//! byte stream. Pure logic, no sockets — host-testable.

use std::collections::BTreeMap;

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
        }
    }

    /// Feeds one TCP segment (`seq`, `payload`) into the reassembler.
    ///
    /// Empty payloads are ignored. The first segment ever pushed establishes
    /// the baseline `next_seq`. Segments behind `next_seq` (already
    /// consumed) are dropped as retransmits; segments at `next_seq` extend
    /// `buffer` and drain any now-contiguous cached segments; segments ahead
    /// of `next_seq` are cached until the gap fills in.
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
            // Already-consumed retransmit — drop it.
        } else if diff == 0 {
            self.append_to_buffer(payload);
            self.next_seq = Some(before.wrapping_add(payload.len() as u32));
            self.drain_contiguous();
        } else {
            self.cache.insert(seq, payload.to_vec());
        }

        let after = self.next_seq.expect("set above");
        if after == before && !self.cache.is_empty() {
            self.stall_pushes += 1;
            if self.stall_pushes >= Self::MAX_STALL_PUSHES {
                if let Some(&lowest) = self.cache.keys().next() {
                    self.resync(lowest);
                }
            }
        } else {
            self.stall_pushes = 0;
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
        }
    }

    /// Hands the accumulated, ordered bytes to the caller (typically the
    /// protocol decoder) and clears the internal buffer.
    pub fn take_stream(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buffer)
    }

    /// Clears cached and buffered state and re-anchors the stream at `seq`.
    /// Called on server change / zone change / stall recovery.
    pub fn resync(&mut self, seq: u32) {
        self.cache.clear();
        self.buffer.clear();
        self.next_seq = Some(seq);
        self.stall_pushes = 0;
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
}
