//! "No panic on malformed input" robustness tests (plan T1.5). Uses a
//! small deterministic xorshift PRNG seeded from a fixed constant instead of
//! an external fuzz dependency, so runs are reproducible.

mod common;

use bpsr_protocol::Decoder;
use common::*;

/// A tiny deterministic xorshift64 PRNG. Not cryptographic — just needs to
/// be fast, seedable, and reproducible across runs.
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        // xorshift requires a non-zero state.
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    fn next_byte(&mut self) -> u8 {
        (self.next_u64() & 0xFF) as u8
    }
}

const SEED: u64 = 0xBADC_0FFE_E0DD_F00D;

/// A structurally valid, moderately complex stream: a zstd FrameDown
/// wrapping one Notify, followed by a plain Notify.
fn valid_stream() -> Vec<u8> {
    let mut nested = Vec::new();
    nested.extend(damage_notify_frame(
        (6i64 << 16) | 64,
        base_damage((5i64 << 16) | 640, 1, 42),
        false,
    ));
    let mut stream = framedown(&nested, true);
    stream.extend(notify(
        bpsr_protocol::decode::opcode::SYNC_CONTAINER_DATA,
        &sync_container_data_payload(7, "Ari", 2),
        false,
    ));
    stream
}

/// 50k fully random byte buffers of random length, pushed through a reused
/// `Decoder` (simulating a continuous garbage stream): must never panic, and
/// must keep its buffer bounded *without* any `reset()` — production has no
/// equivalent hook, so the no-reset path is the one that matters.
#[test]
fn fifty_thousand_random_buffers_never_panic_or_grow_unbounded() {
    let mut rng = XorShift64::new(SEED);
    let mut decoder = Decoder::new();
    for i in 0..50_000u64 {
        let len = (rng.next_u32() % 300) as usize;
        let buf: Vec<u8> = (0..len).map(|_| rng.next_byte()).collect();
        let _ = decoder.push_stream(&buf, i);
        assert!(
            decoder.pending_len() <= bpsr_protocol::frame::MAX_TAIL_LEN,
            "buffered tail exceeded MAX_TAIL_LEN at iteration {i}: {}",
            decoder.pending_len()
        );
    }
}

/// 50k truncations of a structurally valid frame stream, each pushed into a
/// fresh `Decoder`: must never panic, regardless of where the cut lands.
#[test]
fn fifty_thousand_truncations_never_panic() {
    let stream = valid_stream();
    assert!(!stream.is_empty());
    for i in 0..50_000usize {
        let cut = i % (stream.len() + 1);
        let mut decoder = Decoder::new();
        let _ = decoder.push_stream(&stream[..cut], i as u64);
    }
}

/// Deterministic pseudo-fuzz: 50k random byte-mutations of a valid stream,
/// each pushed into a fresh `Decoder`: must never panic.
#[test]
fn fifty_thousand_byte_mutations_of_valid_stream_never_panic() {
    let base = valid_stream();
    let mut rng = XorShift64::new(SEED ^ 0x1234_5678_9abc_def0);
    for i in 0..50_000u64 {
        let mut mutated = base.clone();
        let mutations = 1 + (rng.next_u32() % 4);
        for _ in 0..mutations {
            if mutated.is_empty() {
                break;
            }
            let idx = (rng.next_u32() as usize) % mutated.len();
            mutated[idx] = rng.next_byte();
        }
        let mut decoder = Decoder::new();
        let _ = decoder.push_stream(&mutated, i);
    }
}

/// A Notify with the zstd flag set but a payload that is not valid zstd data
/// at all: decompression fails, the fragment is dropped, no panic.
#[test]
fn zstd_flag_with_garbage_payload_does_not_panic() {
    let mut body = Vec::new();
    body.extend_from_slice(&bpsr_protocol::frame::SERVICE_UUID.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(&bpsr_protocol::decode::opcode::SYNC_NEAR_DELTA_INFO.to_be_bytes());
    body.extend_from_slice(b"definitely-not-a-valid-zstd-frame-at-all");
    let stream = frame(2, true, &body); // Notify, compression flag set, garbage payload

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 0);
    assert!(events.is_empty());
}

/// `total_len == u32::MAX`: rejected as `Desync` inside `split_frames`; the
/// `Decoder` clears its tail and returns no events, without panicking.
#[test]
fn total_len_u32_max_does_not_panic() {
    let stream = 0xFFFF_FFFFu32.to_be_bytes().to_vec();
    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 0);
    assert!(events.is_empty());
}

/// `total_len` just below the minimum valid frame length (6): also a
/// `Desync`, not a panic.
#[test]
fn total_len_below_minimum_does_not_panic() {
    let stream = 5u32.to_be_bytes().to_vec();
    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 0);
    assert!(events.is_empty());
}

/// A length prefix inside the legal range whose body dribbles in over many
/// pushes: the decoder must buffer it without ever exceeding `MAX_TAIL_LEN`,
/// and must still decode the next real frame once that body has gone by — all
/// on the production path, with no `reset()` anywhere.
#[test]
fn in_range_claim_dribbled_in_stays_bounded_and_then_recovers() {
    const CLAIM: u32 = bpsr_protocol::frame::MAX_FRAME_LEN;
    let mut decoder = Decoder::new();
    let mut header = CLAIM.to_be_bytes().to_vec();
    header.extend_from_slice(&2u16.to_be_bytes()); // Notify
    assert!(decoder.push_stream(&header, 0).is_empty());

    let chunk = vec![0u8; 64 * 1024];
    let mut pushed = header.len();
    while pushed < CLAIM as usize {
        let n = chunk.len().min(CLAIM as usize - pushed);
        assert!(decoder.push_stream(&chunk[..n], 0).is_empty());
        pushed += n;
        assert!(
            decoder.pending_len() <= bpsr_protocol::frame::MAX_TAIL_LEN,
            "buffered tail exceeded MAX_TAIL_LEN: {}",
            decoder.pending_len()
        );
    }
    assert_eq!(decoder.pending_len(), 0, "completed frame must be released");

    let good = damage_notify_frame(
        (2i64 << 16) | 64,
        base_damage((1i64 << 16) | 640, 1, 10),
        false,
    );
    assert_eq!(decoder.push_stream(&good, 1).len(), 1);
}

/// A `total_len` far beyond anything the protocol could ever produce is
/// garbage, not a real over-large frame: the decoder must realign on the very
/// next push rather than swallowing the gigabytes the prefix claims.
#[test]
fn absurd_total_len_does_not_swallow_the_following_stream() {
    let mut decoder = Decoder::new();
    assert!(
        decoder
            .push_stream(&0xFFFF_FFFFu32.to_be_bytes(), 0)
            .is_empty()
    );
    let good = damage_notify_frame(
        (2i64 << 16) | 64,
        base_damage((1i64 << 16) | 640, 1, 10),
        false,
    );
    assert_eq!(decoder.push_stream(&good, 1).len(), 1);
}

/// A FrameDown that nests itself 100 levels deep: the depth cap (4) stops
/// recursion far short of the innermost Notify, so no stack overflow and no
/// events reach `out`.
#[test]
fn framedown_nested_100_deep_does_not_panic() {
    let mut current = damage_notify_frame(
        (2i64 << 16) | 64,
        base_damage((1i64 << 16) | 640, 1, 10),
        false,
    );
    for _ in 0..100 {
        current = framedown(&current, false);
    }

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&current, 0);
    assert!(events.is_empty());
}

/// Truncating just the outer frame header (fewer than 4 bytes buffered at
/// all) must not panic and must not consume anything.
#[test]
fn header_only_partial_bytes_do_not_panic() {
    for n in 0..4usize {
        let stream = valid_stream();
        let mut decoder = Decoder::new();
        let events = decoder.push_stream(&stream[..n], 0);
        assert!(events.is_empty());
    }
}
