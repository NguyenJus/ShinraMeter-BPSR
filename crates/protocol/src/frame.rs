//! Outer frame splitter, fragment dispatch, and zstd handling (plan §0.5).
//!
//! Wire format, repeated back-to-back in the reassembled TCP byte stream:
//! `[total_len: BE u32 (includes itself)][packet_type: BE u16][body...]`.

use crate::inspect::InspectSink;
use crate::reader::Reader;

pub const COMPRESSION_FLAG: u16 = 0x8000;
pub const TYPE_MASK: u16 = 0x7FFF;
pub const MIN_FRAME_LEN: u32 = 6;
pub const MAX_FRAME_LEN: u32 = 10 * 1024 * 1024;
/// Upper bound on a length prefix still treated as a real (if over-large)
/// frame whose body can be skipped to re-align. A length modestly past the
/// ceiling is plausibly a genuine frame; beyond this the prefix is assumed to
/// be garbage, because skipping the gigabytes a random 4-byte value claims
/// would wedge the stream far longer than re-aligning on the next push does.
pub const MAX_SKIPPABLE_FRAME_LEN: u32 = 2 * MAX_FRAME_LEN;
/// Hard cap on the bytes a stream consumer may buffer while waiting for a
/// frame to complete. A frame can never legitimately need more than
/// `MAX_FRAME_LEN` buffered, so this is a backstop that keeps the bound a
/// stated policy rather than a side effect of `MAX_FRAME_LEN`.
pub const MAX_TAIL_LEN: usize = MAX_FRAME_LEN as usize;
/// `SERVICE_UUID = 0x0000_0000_6333_5342` — Notify fragments carrying any
/// other service uuid are dropped.
pub const SERVICE_UUID: u64 = 0x0000_0000_6333_5342;
/// `EServiceId.GrpcTeamNtf` — a second protobuf service riding the same
/// already-adopted TCP connection as `SERVICE_UUID` (issue #146). Notify
/// fragments carrying this uuid are accepted alongside `SERVICE_UUID`; every
/// other service uuid is still dropped.
pub const TEAM_NTF_SERVICE_UUID: u64 = 0x0000_0000_399F_CA69;
pub const MAX_FRAMEDOWN_DEPTH: usize = 4;

/// Total decoded payload/work allowance per `Decoder::push_stream` call,
/// including every outer and nested FrameDown and Notify. Per-fragment
/// limits alone allow sibling compressed frames to multiply memory/CPU use
/// at each nesting level. This permits a maximum-sized payload at every
/// supported depth along one path.
const MAX_EXPANDED_BYTES: usize = MAX_FRAME_LEN as usize * (MAX_FRAMEDOWN_DEPTH + 1);
/// Work charged for starting one zstd decoder, even when the payload is tiny
/// or malformed before producing output. This prevents a frame full of
/// zero-output failures from bypassing the shared expansion budget without
/// treating each cheap header error as if it expanded a full 10 MiB.
const DECOMPRESSION_ATTEMPT_COST: usize = 64 * 1024;
/// Highest raw fragment-type discriminant the wire format defines. Used to
/// sanity-check a header before trusting its length prefix.
pub const MAX_FRAGMENT_TYPE: u16 = FragmentType::FrameDown as u16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FragmentType {
    None,
    Call,
    Notify,
    Return,
    Echo,
    FrameUp,
    FrameDown,
}

impl From<u16> for FragmentType {
    fn from(v: u16) -> Self {
        match v {
            0 => FragmentType::None,
            1 => FragmentType::Call,
            2 => FragmentType::Notify,
            3 => FragmentType::Return,
            4 => FragmentType::Echo,
            5 => FragmentType::FrameUp,
            6 => FragmentType::FrameDown,
            _ => FragmentType::None,
        }
    }
}

/// A decoded Notify fragment: opcode + payload (already decompressed if the
/// source frame carried the zstd flag).
#[derive(Clone, Debug, PartialEq)]
pub struct Notify {
    pub service_uuid: u64,
    pub method_id: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame length desync")]
    Desync,
    #[error("frame exceeds maximum length")]
    TooLarge,
    #[error("frame truncated")]
    Truncated,
    #[error("zstd decompression failed")]
    Zstd,
}

/// Why `split_frames` stopped: the stream no longer parses as frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desync {
    /// The length prefix is nonsense — below `MIN_FRAME_LEN`, or so large it
    /// cannot be a real frame. There is nothing trustworthy to skip: the
    /// caller drops its buffer and re-synchronises on whatever arrives next.
    Unrecoverable,
    /// The length prefix is well-formed but larger than `MAX_FRAME_LEN`. The
    /// frame is refused, yet its length is plausible enough to trust for
    /// re-alignment: the caller must discard `total_len` bytes counted from
    /// the desync offset (`consumed`) to land on the next frame boundary.
    /// Dropping only the buffered tail would resume mid-body and turn every
    /// following length prefix into garbage.
    Oversized { total_len: u32 },
}

/// Result of `split_frames`: the frames found and bytes consumed before
/// either running out of complete frames or hitting a desync. `frames` and
/// `consumed` cover everything parsed *before* the desync point — a desync
/// must not discard frames already parsed earlier in the same buffer.
#[derive(Debug)]
pub struct SplitFrames<'a> {
    pub frames: Vec<&'a [u8]>,
    pub consumed: usize,
    pub desync: Option<Desync>,
}

/// Splits `stream` into complete outer frames, returning the frames found and
/// the number of bytes consumed (the caller keeps whatever tail remains for
/// the next push). A `total_len` outside `[MIN_FRAME_LEN, MAX_FRAME_LEN]`
/// sets `desync`; `frames`/`consumed` still reflect everything parsed before
/// that point, so the caller can keep the good frames and drop only the tail
/// from `consumed` onward. See `Desync` for how the caller must re-align.
pub fn split_frames(stream: &[u8]) -> SplitFrames<'_> {
    let mut frames = Vec::new();
    let mut consumed = 0usize;
    while let Some(remaining) = stream.get(consumed..) {
        if remaining.len() < 4 {
            break;
        }
        let reader = Reader::new(remaining);
        let total_len = match reader.peek_u32() {
            Some(v) => v,
            None => break,
        };
        if total_len > MAX_FRAME_LEN {
            // TCP may split the six-byte header after its length word.
            // Wait for the type before deciding whether a plausible length
            // is skippable; absence of bytes is not evidence of corruption.
            if total_len <= MAX_SKIPPABLE_FRAME_LEN && remaining.len() < MIN_FRAME_LEN as usize {
                break;
            }
            // Trust an over-large length enough to skip its body only when the
            // packet type behind it also looks like a real frame header — a
            // random 4-byte prefix must not make us discard megabytes of good
            // stream.
            let packet_type = remaining
                .get(4..6)
                .and_then(|b| <[u8; 2]>::try_from(b).ok())
                .map(u16::from_be_bytes);
            let plausible = total_len <= MAX_SKIPPABLE_FRAME_LEN
                && packet_type.is_some_and(|t| t & TYPE_MASK <= MAX_FRAGMENT_TYPE);
            return SplitFrames {
                frames,
                consumed,
                desync: Some(if plausible {
                    Desync::Oversized { total_len }
                } else {
                    Desync::Unrecoverable
                }),
            };
        }
        if total_len < MIN_FRAME_LEN {
            return SplitFrames {
                frames,
                consumed,
                desync: Some(Desync::Unrecoverable),
            };
        }
        let total_len = total_len as usize;
        if remaining.len() < total_len {
            // Partial frame; wait for more bytes on the next push.
            break;
        }
        let frame = match remaining.get(..total_len) {
            Some(f) => f,
            None => break,
        };
        frames.push(frame);
        consumed += total_len;
    }
    SplitFrames {
        frames,
        consumed,
        desync: None,
    }
}

/// Decompresses a zstd payload with the output hard-capped at
/// `MAX_FRAME_LEN`. The cap is what keeps a zstd bomb — a ~1 MB frame can
/// expand to tens of GB, and `FrameDown` nests up to `MAX_FRAMEDOWN_DEPTH`
/// decompressions — from OOM-killing the process. Exceeding it is a decode
/// failure (the fragment is dropped), never a panic.
enum DecompressOutcome {
    Decoded(Vec<u8>),
    Failed {
        // Includes the requested read capacity if that read failed, because
        // zstd may write output before reporting a checksum or stream error.
        produced: usize,
        hit_output_limit: bool,
    },
}

fn decompress(payload: &[u8], max_output: usize) -> DecompressOutcome {
    use std::io::Read;

    let mut decoder = match zstd::stream::read::Decoder::new(payload) {
        Ok(decoder) => decoder,
        Err(_) => {
            return DecompressOutcome::Failed {
                produced: 0,
                hit_output_limit: false,
            };
        }
    };
    // The output cap does not bound zstd's internal history window. A
    // 16 MiB window accommodates any permitted 10 MiB payload.
    if decoder.window_log_max(24).is_err() {
        return DecompressOutcome::Failed {
            produced: 0,
            hit_output_limit: false,
        };
    }
    // One byte past the cap, so "too large" is detectable without ever
    // buffering more than `MAX_FRAME_LEN + 1` bytes.
    let mut out = Vec::new();
    let mut chunk = [0; DECOMPRESSION_ATTEMPT_COST];
    loop {
        let requested = chunk.len().min(max_output + 1 - out.len());
        match decoder.read(&mut chunk[..requested]) {
            Ok(0) => return DecompressOutcome::Decoded(out),
            Ok(read) => {
                out.extend_from_slice(&chunk[..read]);
                if out.len() > max_output {
                    log::debug!(
                        "bpsr-protocol: zstd output exceeded expansion budget, dropping fragment"
                    );
                    return DecompressOutcome::Failed {
                        produced: max_output,
                        hit_output_limit: true,
                    };
                }
            }
            Err(_) => {
                // Read errors do not report how many bytes zstd wrote into
                // the buffer. Charge the full request as well as earlier
                // successful reads so corrupt trailers cannot hide work.
                let produced = (out.len() + requested).min(max_output);
                return DecompressOutcome::Failed {
                    produced,
                    hit_output_limit: produced == max_output,
                };
            }
        }
    }
}

/// Expansion/work allowance shared by every outer and nested frame decoded
/// from one `Decoder::push_stream` call. The public `parse_frame` convenience
/// wrapper creates its own allowance because it parses one independent frame.
pub(crate) struct ExpansionBudget {
    remaining: usize,
}

impl ExpansionBudget {
    pub(crate) fn new() -> Self {
        Self {
            remaining: MAX_EXPANDED_BYTES,
        }
    }

    fn spend(&mut self, amount: usize) {
        self.remaining = self.remaining.saturating_sub(amount);
    }
}

/// Parses one complete outer frame (as produced by `split_frames`), pushing
/// any decoded `Notify` fragments onto `out`. Handles `Notify` and
/// `FrameDown` only; every other fragment type is a silent no-op. Never
/// panics or propagates an error for malformed bodies — it just drops them.
///
/// `sink`/`now_ms` are the diagnostic-mode observation hook (issue #25 slice
/// A): `None` reproduces the pre-#25 behavior exactly (see `handle_notify`).
pub fn parse_frame(
    frame: &[u8],
    depth: usize,
    out: &mut Vec<Notify>,
    sink: Option<&dyn InspectSink>,
    now_ms: u64,
) {
    let mut budget = ExpansionBudget::new();
    parse_frame_with_budget(frame, depth, out, sink, now_ms, &mut budget);
}

pub(crate) fn parse_frame_with_budget(
    frame: &[u8],
    depth: usize,
    out: &mut Vec<Notify>,
    sink: Option<&dyn InspectSink>,
    now_ms: u64,
    budget: &mut ExpansionBudget,
) {
    if budget.remaining == 0 {
        return;
    }
    let mut reader = Reader::new(frame);
    let _total_len = match reader.read_u32() {
        Some(v) => v,
        None => return,
    };
    let packet_type = match reader.read_u16() {
        Some(v) => v,
        None => return,
    };
    let is_zstd = packet_type & COMPRESSION_FLAG != 0;
    let fragment_type = FragmentType::from(packet_type & TYPE_MASK);
    let body = reader.read_rest();

    match fragment_type {
        FragmentType::Notify => handle_notify(body, is_zstd, out, sink, now_ms, budget),
        FragmentType::FrameDown => {
            handle_frame_down(body, is_zstd, depth, out, sink, now_ms, budget)
        }
        _ => {}
    }
}

/// Shared zstd-or-passthrough payload step; `None` means "drop the
/// fragment" (a decompression failure), matching the pre-#25 behavior.
fn decode_payload(
    raw_payload: &[u8],
    is_zstd: bool,
    budget: &mut ExpansionBudget,
) -> Option<Vec<u8>> {
    if is_zstd {
        let limit = budget.remaining.min(MAX_FRAME_LEN as usize);
        match decompress(raw_payload, limit) {
            DecompressOutcome::Decoded(p) => {
                budget.spend(p.len().max(DECOMPRESSION_ATTEMPT_COST).min(limit));
                Some(p)
            }
            DecompressOutcome::Failed {
                produced,
                hit_output_limit,
            } => {
                let charge = if hit_output_limit {
                    limit
                } else {
                    produced.max(DECOMPRESSION_ATTEMPT_COST).min(limit)
                };
                budget.spend(charge);
                log::debug!("bpsr-protocol: zstd payload decode failed");
                None
            }
        }
    } else {
        if raw_payload.len() > budget.remaining {
            budget.remaining = 0;
            return None;
        }
        budget.spend(raw_payload.len());
        Some(raw_payload.to_vec())
    }
}

/// A Notify body's header fields plus its payload bytes, still exactly as
/// they arrived (compressed, if the outer frame carried the zstd flag).
struct NotifyBody<'a> {
    service_uuid: u64,
    method_id: u32,
    raw_payload: &'a [u8],
}

/// Splits a Notify body into its header fields and payload; `None` on a
/// truncated body. One parse for both modes — normal and diagnostic differ
/// only in what they do with the result, never in how they read it.
fn parse_notify_body(body: &[u8]) -> Option<NotifyBody<'_>> {
    let mut reader = Reader::new(body);
    let service_uuid = reader.read_u64()?;
    let _stub_id = reader.read_u32()?;
    let method_id = reader.read_u32()?;
    Some(NotifyBody {
        service_uuid,
        method_id,
        raw_payload: reader.read_rest(),
    })
}

/// Whether `service_uuid` is one of the services this crate accepts onto
/// `out` — `SERVICE_UUID` or `TEAM_NTF_SERVICE_UUID` (issue #146). Both
/// guards in `handle_notify` call this single predicate so they cannot
/// drift apart.
fn is_accepted_service(service_uuid: u64) -> bool {
    service_uuid == SERVICE_UUID || service_uuid == TEAM_NTF_SERVICE_UUID
}

fn handle_notify(
    body: &[u8],
    is_zstd: bool,
    out: &mut Vec<Notify>,
    sink: Option<&dyn InspectSink>,
    now_ms: u64,
    budget: &mut ExpansionBudget,
) {
    let Some(body) = parse_notify_body(body) else {
        return;
    };
    // Without a diagnostic sink this is the pre-#25 code path: an
    // unaccepted service uuid returns before any decompression happens, so
    // a normal run pays nothing extra for traffic it was always going to
    // drop. With a sink, we decompress regardless of service uuid so
    // `sink.on_notify` observes every Notify-shaped fragment — including the
    // ones a normal run drops right here.
    if sink.is_none() && !is_accepted_service(body.service_uuid) {
        return;
    }
    let payload = decode_payload(body.raw_payload, is_zstd, budget);
    if let Some(sink) = sink {
        // A payload we failed to decompress is still reported, as the raw
        // undecompressed bytes flagged `payload_decoded = false` — malformed
        // or foreign-codec traffic is exactly what the diagnostic mode
        // exists to surface, so it must not vanish here.
        let (observed, payload_decoded) = match payload.as_deref() {
            Some(p) => (p, true),
            None => (body.raw_payload, false),
        };
        sink.on_notify(
            body.service_uuid,
            body.method_id,
            observed,
            payload_decoded,
            now_ms,
        );
    }
    if !is_accepted_service(body.service_uuid) {
        return;
    }
    // A decompression failure drops the fragment in both modes: `out` only
    // ever carries payloads the decoder can actually read.
    let Some(payload) = payload else {
        return;
    };
    out.push(Notify {
        service_uuid: body.service_uuid,
        method_id: body.method_id,
        payload,
    });
}

fn handle_frame_down(
    body: &[u8],
    is_zstd: bool,
    depth: usize,
    out: &mut Vec<Notify>,
    sink: Option<&dyn InspectSink>,
    now_ms: u64,
    budget: &mut ExpansionBudget,
) {
    if depth >= MAX_FRAMEDOWN_DEPTH {
        return;
    }
    let mut reader = Reader::new(body);
    let _server_sequence_id = match reader.read_u32() {
        Some(v) => v,
        None => return,
    };
    let raw_nested = reader.read_rest();
    let Some(nested) = decode_payload(raw_nested, is_zstd, budget) else {
        return;
    };
    let result = split_frames(&nested);
    if result.desync.is_some() {
        log::debug!("bpsr-protocol: desync while splitting FrameDown nested stream");
    }
    for f in result.frames {
        if budget.remaining == 0 {
            break;
        }
        parse_frame_with_budget(f, depth + 1, out, sink, now_ms, budget);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_frame(fragment_type: u16, compressed: bool, body: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        let total_len = 4 + 2 + body.len() as u32;
        buf.extend_from_slice(&total_len.to_be_bytes());
        let packet_type = fragment_type | if compressed { COMPRESSION_FLAG } else { 0 };
        buf.extend_from_slice(&packet_type.to_be_bytes());
        buf.extend_from_slice(body);
        buf
    }

    fn build_notify_body(method_id: u32, payload: &[u8]) -> Vec<u8> {
        build_notify_body_with_service(SERVICE_UUID, method_id, payload)
    }

    fn build_notify_body_with_service(
        service_uuid: u64,
        method_id: u32,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&service_uuid.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&method_id.to_be_bytes());
        buf.extend_from_slice(payload);
        buf
    }

    /// `(service_uuid, method_id, payload, payload_decoded, now_ms)`, as
    /// recorded by `RecordingSink::on_notify`.
    type RecordedNotify = (u64, u32, Vec<u8>, bool, u64);

    /// Test-only `InspectSink` that just records every `on_notify` call, in
    /// order, for assertions.
    struct RecordingSink {
        notifies: std::sync::Mutex<Vec<RecordedNotify>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                notifies: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl InspectSink for RecordingSink {
        fn on_notify(
            &self,
            service_uuid: u64,
            method_id: u32,
            payload: &[u8],
            payload_decoded: bool,
            now_ms: u64,
        ) {
            self.notifies.lock().unwrap().push((
                service_uuid,
                method_id,
                payload.to_vec(),
                payload_decoded,
                now_ms,
            ));
        }

        fn on_attr(&self, _uid: i64, _attr_id: i32, _raw: &[u8], _known: bool) {}
    }

    fn build_notify_frame(method_id: u32, payload: &[u8], compressed: bool) -> Vec<u8> {
        let raw = if compressed {
            zstd::stream::encode_all(payload, 0).unwrap()
        } else {
            payload.to_vec()
        };
        let body = build_notify_body(method_id, &raw);
        build_frame(2, compressed, &body)
    }

    fn build_framedown_frame(seq: u32, nested: &[u8], compressed: bool) -> Vec<u8> {
        let raw = if compressed {
            zstd::stream::encode_all(nested, 0).unwrap()
        } else {
            nested.to_vec()
        };
        let mut body = Vec::new();
        body.extend_from_slice(&seq.to_be_bytes());
        body.extend_from_slice(&raw);
        build_frame(6, compressed, &body)
    }

    #[test]
    fn uncompressed_notify_parses() {
        let frame = build_notify_frame(0x06, b"hello", false);
        let mut out = Vec::new();
        parse_frame(&frame, 0, &mut out, None, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].method_id, 0x06);
        assert_eq!(out[0].payload, b"hello");
    }

    #[test]
    fn zstd_notify_decompresses() {
        let frame = build_notify_frame(0x15, b"payload-data-payload-data", true);
        let mut out = Vec::new();
        parse_frame(&frame, 0, &mut out, None, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].method_id, 0x15);
        assert_eq!(out[0].payload, b"payload-data-payload-data");
    }

    #[test]
    fn two_frames_in_one_buffer() {
        let mut stream = build_notify_frame(1, b"a", false);
        stream.extend(build_notify_frame(2, b"bb", false));
        let result = split_frames(&stream);
        assert!(result.desync.is_none());
        assert_eq!(result.frames.len(), 2);
        assert_eq!(result.consumed, stream.len());
    }

    #[test]
    fn partial_frame_yields_nothing() {
        let full = build_notify_frame(1, b"abcdef", false);
        let partial = &full[..full.len() - 1];
        let result = split_frames(partial);
        assert!(result.desync.is_none());
        assert_eq!(result.frames.len(), 0);
        assert_eq!(result.consumed, 0);
    }

    #[test]
    fn framedown_wraps_two_notifies() {
        let mut nested = build_notify_frame(1, b"one", false);
        nested.extend(build_notify_frame(2, b"two", false));
        let frame = build_framedown_frame(42, &nested, false);
        let mut out = Vec::new();
        parse_frame(&frame, 0, &mut out, None, 0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].method_id, 1);
        assert_eq!(out[1].method_id, 2);
    }

    #[test]
    fn framedown_depth_cap_stops_recursion() {
        let mut current = build_notify_frame(0x2d, b"deep", false);
        // wrap 5 levels of FrameDown; depth cap (4) must prevent the innermost notify
        for _ in 0..5 {
            current = build_framedown_frame(1, &current, false);
        }
        let mut out = Vec::new();
        parse_frame(&current, 0, &mut out, None, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn unknown_fragment_type_skipped() {
        let frame = build_frame(99, false, b"whatever");
        let mut out = Vec::new();
        parse_frame(&frame, 0, &mut out, None, 0);
        assert!(out.is_empty());
    }

    /// A zstd "bomb": a tiny on-the-wire payload that expands far past
    /// `MAX_FRAME_LEN`. Decompression must be hard-capped, so the fragment is
    /// dropped instead of materialising the full expansion.
    #[test]
    fn zstd_bomb_exceeding_max_frame_len_is_dropped() {
        let huge = vec![0u8; MAX_FRAME_LEN as usize + 1024];
        let frame = build_notify_frame(0x15, &huge, true);
        assert!(
            frame.len() < 4096,
            "bomb frame should be tiny on the wire: {}",
            frame.len()
        );
        let mut out = Vec::new();
        parse_frame(&frame, 0, &mut out, None, 0);
        assert!(
            out.is_empty(),
            "a payload expanding past MAX_FRAME_LEN must be dropped"
        );
    }

    /// The output cap must not break legitimately large compressed payloads
    /// that stay under `MAX_FRAME_LEN`.
    #[test]
    fn zstd_payload_under_cap_still_decompresses() {
        let payload = vec![7u8; 1024 * 1024];
        let frame = build_notify_frame(0x15, &payload, true);
        let mut out = Vec::new();
        parse_frame(&frame, 0, &mut out, None, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload.len(), payload.len());
    }

    #[test]
    fn zstd_window_is_bounded_independently_of_output_size() {
        // Valid zstd frame: unknown content size, one last raw block "abc".
        // Its advertised history window need not match its tiny output.
        let mut compressed = vec![0x28, 0xb5, 0x2f, 0xfd, 0, (24 - 10) << 3, 25, 0, 0];
        compressed.extend_from_slice(b"abc");
        assert!(matches!(
            decompress(&compressed, 3),
            DecompressOutcome::Decoded(payload) if payload == b"abc"
        ));
        compressed[5] = (25 - 10) << 3; // 32 MiB history for three output bytes
        assert!(matches!(
            decompress(&compressed, 3),
            DecompressOutcome::Failed { .. }
        ));
    }

    #[test]
    fn compressed_siblings_share_one_outer_frame_expansion_budget() {
        // Every individual fragment satisfies the old 10 MiB cap, and
        // nesting depth is only two. Without an aggregate budget, adding
        // more siblings scales retained payloads without a useful bound.
        let payload = vec![0; MAX_FRAME_LEN as usize];
        let notify = build_notify_frame(1, &payload, true);
        let inner = build_framedown_frame(1, &notify.repeat(3), true);
        let outer = build_framedown_frame(1, &inner.repeat(2), true);
        assert!(outer.len() < 4096);
        let mut out = Vec::new();
        parse_frame(&outer, 0, &mut out, None, 0);
        assert!(!out.is_empty());
        assert!(out.iter().map(|n| n.payload.len()).sum::<usize>() <= MAX_EXPANDED_BYTES);

        // Refusing the excessive expansion does not poison a new frame.
        out.clear();
        parse_frame(&build_notify_frame(2, b"ok", true), 0, &mut out, None, 0);
        assert_eq!(out[0].payload, b"ok");
    }

    #[test]
    fn failed_expansions_also_spend_the_shared_budget() {
        let payload = vec![0; MAX_FRAME_LEN as usize + 1];
        let notify = build_notify_frame(1, &payload, true);
        let outer = build_framedown_frame(1, &notify.repeat(8), false);
        let sink = RecordingSink::new();
        let mut out = Vec::new();
        parse_frame(&outer, 0, &mut out, Some(&sink), 0);
        assert!(out.is_empty());
        // At most five limited decompressions, even for invalid siblings.
        assert_eq!(sink.notifies.lock().unwrap().len(), 5);
    }

    #[test]
    fn cheap_zstd_errors_do_not_spend_fictitious_full_expansions() {
        let invalid = build_frame(2, true, &build_notify_body(1, b"x"));
        let valid = build_notify_frame(2, b"valid", false);
        let mut nested = invalid.repeat(5);
        nested.extend(valid);
        let outer = build_framedown_frame(1, &nested, false);

        let mut out = Vec::new();
        parse_frame(&outer, 0, &mut out, None, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload, b"valid");
    }

    fn compressed_with_corrupt_checksum(payload_len: usize) -> Vec<u8> {
        use std::io::Write;

        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 0).unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.write_all(&vec![0; payload_len]).unwrap();
        let mut compressed = encoder.finish().unwrap();
        *compressed.last_mut().unwrap() ^= 1;
        compressed
    }

    #[test]
    fn checksum_errors_charge_output_written_during_the_failed_read() {
        for payload_len in [
            DECOMPRESSION_ATTEMPT_COST - 1,
            DECOMPRESSION_ATTEMPT_COST,
            DECOMPRESSION_ATTEMPT_COST + 1,
            2 * DECOMPRESSION_ATTEMPT_COST - 1,
            2 * DECOMPRESSION_ATTEMPT_COST,
            2 * DECOMPRESSION_ATTEMPT_COST + 1,
            MAX_FRAME_LEN as usize - 1,
            MAX_FRAME_LEN as usize,
        ] {
            let compressed = compressed_with_corrupt_checksum(payload_len);
            let mut budget = ExpansionBudget::new();
            assert!(decode_payload(&compressed, true, &mut budget).is_none());
            let spent = MAX_EXPANDED_BYTES - budget.remaining;
            assert!(
                spent >= payload_len,
                "checksum failure undercharged {payload_len} output bytes: {spent}"
            );
            assert!(
                spent <= (payload_len + DECOMPRESSION_ATTEMPT_COST).min(MAX_FRAME_LEN as usize),
                "checksum failure overcharged {payload_len} output bytes: {spent}"
            );
        }
    }

    #[test]
    fn checksum_failures_exhaust_the_shared_budget_after_five_full_expansions() {
        let compressed = compressed_with_corrupt_checksum(MAX_FRAME_LEN as usize);
        let frame = build_frame(2, true, &build_notify_body(1, &compressed));
        let mut budget = ExpansionBudget::new();
        let sink = RecordingSink::new();
        let mut out = Vec::new();
        for _ in 0..6 {
            parse_frame_with_budget(&frame, 0, &mut out, Some(&sink), 0, &mut budget);
        }
        assert!(out.is_empty());
        assert_eq!(budget.remaining, 0);
        assert_eq!(sink.notifies.lock().unwrap().len(), 5);
    }

    #[test]
    fn total_len_too_small_is_desync() {
        let buf = 5u32.to_be_bytes().to_vec();
        let result = split_frames(&buf);
        assert!(result.desync.is_some());
        assert!(result.frames.is_empty());
        assert_eq!(result.consumed, 0);
    }

    #[test]
    fn total_len_max_is_unrecoverable_desync() {
        let buf = 0xFFFF_FFFFu32.to_be_bytes().to_vec();
        let result = split_frames(&buf);
        assert_eq!(result.desync, Some(Desync::Unrecoverable));
        assert!(result.frames.is_empty());
        assert_eq!(result.consumed, 0);
    }

    /// A length just over `MAX_FRAME_LEN` behind a well-formed packet type is
    /// plausibly a real frame, so the caller is told how many bytes to skip to
    /// re-align.
    #[test]
    fn oversized_len_with_valid_type_is_skippable_desync() {
        let total_len = MAX_FRAME_LEN + 1;
        let mut buf = total_len.to_be_bytes().to_vec();
        buf.extend_from_slice(&2u16.to_be_bytes()); // Notify
        let result = split_frames(&buf);
        assert_eq!(result.desync, Some(Desync::Oversized { total_len }));
        assert_eq!(result.consumed, 0);
    }

    /// The same over-large length behind a nonsense packet type is garbage:
    /// skipping megabytes of stream on it would be worse than re-aligning.
    #[test]
    fn oversized_len_with_unknown_type_is_unrecoverable() {
        let mut buf = (MAX_FRAME_LEN + 1).to_be_bytes().to_vec();
        buf.extend_from_slice(&0x2BAB_u16.to_be_bytes());
        let result = split_frames(&buf);
        assert_eq!(result.desync, Some(Desync::Unrecoverable));
    }

    /// A plausible over-large length waits for its fragmented type bytes.
    #[test]
    fn oversized_len_without_type_bytes_waits_for_the_header() {
        let buf = (MAX_FRAME_LEN + 1).to_be_bytes();
        let result = split_frames(&buf);
        assert_eq!(result.desync, None);
        assert_eq!(result.consumed, 0);
    }

    #[test]
    fn desync_after_good_frames_preserves_frames_and_consumed() {
        let mut stream = build_notify_frame(1, b"a", false);
        stream.extend(build_notify_frame(2, b"bb", false));
        let good_len = stream.len();
        stream.extend_from_slice(&5u32.to_be_bytes()); // garbage: below MIN_FRAME_LEN
        let result = split_frames(&stream);
        assert!(result.desync.is_some());
        assert_eq!(result.frames.len(), 2);
        assert_eq!(result.consumed, good_len);
    }

    #[test]
    fn framedown_desync_after_good_notifies_preserves_them() {
        let mut nested = build_notify_frame(1, b"one", false);
        nested.extend(build_notify_frame(2, b"two", false));
        nested.extend_from_slice(&5u32.to_be_bytes()); // garbage: below MIN_FRAME_LEN
        let frame = build_framedown_frame(42, &nested, false);
        let mut out = Vec::new();
        parse_frame(&frame, 0, &mut out, None, 0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].method_id, 1);
        assert_eq!(out[1].method_id, 2);
    }

    // -- InspectSink observation (issue #25 slice A) ----------------------

    /// The headline case: a Notify on a service uuid we don't recognize is
    /// still dropped from `out` (normal behavior preserved), but a supplied
    /// sink observes it instead of it vanishing silently.
    #[test]
    fn unrecognized_service_uuid_is_observed_via_sink_but_still_dropped() {
        let other_service = SERVICE_UUID.wrapping_add(1);
        let body = build_notify_body_with_service(other_service, 0x42, b"hello");
        let frame = build_frame(2, false, &body);
        let sink = RecordingSink::new();
        let mut out = Vec::new();

        parse_frame(&frame, 0, &mut out, Some(&sink), 123);

        assert!(
            out.is_empty(),
            "an unrecognized service must still be dropped from decoded notifies"
        );
        let seen = sink.notifies.lock().unwrap();
        assert_eq!(
            *seen,
            vec![(other_service, 0x42, b"hello".to_vec(), true, 123)]
        );
    }

    /// Second-service acceptance (issue #146): a Notify on
    /// `TEAM_NTF_SERVICE_UUID` survives `handle_notify` — even without a
    /// diagnostic sink — and keeps its `service_uuid` intact, while a third,
    /// unrelated uuid is still dropped exactly as before.
    #[test]
    fn team_ntf_service_uuid_is_accepted_alongside_the_main_service() {
        let body = build_notify_body_with_service(TEAM_NTF_SERVICE_UUID, 0x3, b"roster");
        let frame = build_frame(2, false, &body);
        let mut out = Vec::new();

        parse_frame(&frame, 0, &mut out, None, 0);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].service_uuid, TEAM_NTF_SERVICE_UUID);
        assert_eq!(out[0].method_id, 0x3);
        assert_eq!(out[0].payload, b"roster");

        let unrelated = TEAM_NTF_SERVICE_UUID.wrapping_add(1);
        let other_body = build_notify_body_with_service(unrelated, 0x3, b"roster");
        let other_frame = build_frame(2, false, &other_body);
        let mut other_out = Vec::new();

        parse_frame(&other_frame, 0, &mut other_out, None, 0);

        assert!(
            other_out.is_empty(),
            "an unrelated third service must still be dropped"
        );
    }

    /// A recognized service is *also* observed via the sink (this is what
    /// feeds the slice A item 4 raw-frame dump), in addition to still
    /// reaching `out` as before.
    #[test]
    fn recognized_service_is_also_observed_via_sink() {
        let body = build_notify_body(0x07, b"payload");
        let frame = build_frame(2, false, &body);
        let sink = RecordingSink::new();
        let mut out = Vec::new();

        parse_frame(&frame, 0, &mut out, Some(&sink), 55);

        assert_eq!(out.len(), 1);
        let seen = sink.notifies.lock().unwrap();
        assert_eq!(
            *seen,
            vec![(SERVICE_UUID, 0x07, b"payload".to_vec(), true, 55)]
        );
    }

    /// `sink = None` reproduces the exact pre-#25 dropping behavior: no
    /// observation, and an unrecognized service is still silently dropped.
    #[test]
    fn no_sink_means_no_observation_and_unchanged_dropping() {
        let other_service = SERVICE_UUID.wrapping_add(1);
        let body = build_notify_body_with_service(other_service, 0x42, b"hello");
        let frame = build_frame(2, false, &body);
        let mut out = Vec::new();

        parse_frame(&frame, 0, &mut out, None, 0);

        assert!(out.is_empty());
    }

    /// The sink also sees Notify fragments nested inside `FrameDown`, not
    /// just top-level ones.
    #[test]
    fn unrecognized_service_nested_in_framedown_is_observed() {
        let other_service = SERVICE_UUID.wrapping_add(1);
        let body = build_notify_body_with_service(other_service, 0x9, b"nested");
        let nested = build_frame(2, false, &body);
        let frame = build_framedown_frame(1, &nested, false);
        let sink = RecordingSink::new();
        let mut out = Vec::new();

        parse_frame(&frame, 0, &mut out, Some(&sink), 7);

        assert!(out.is_empty());
        let seen = sink.notifies.lock().unwrap();
        assert_eq!(
            *seen,
            vec![(other_service, 0x9, b"nested".to_vec(), true, 7)]
        );
    }

    /// A Notify whose payload we cannot decompress is still handed to the
    /// sink — as the raw undecompressed bytes, flagged `payload_decoded =
    /// false` — because malformed/foreign-codec traffic is exactly what the
    /// diagnostic mode exists to surface. It is still kept out of `out`.
    #[test]
    fn notify_whose_payload_fails_to_decompress_still_reaches_the_sink() {
        let garbage = b"not-actually-zstd";
        let body = build_notify_body(0x15, garbage);
        let frame = build_frame(2, true, &body); // claims zstd, isn't
        let sink = RecordingSink::new();
        let mut out = Vec::new();

        parse_frame(&frame, 0, &mut out, Some(&sink), 99);

        assert!(
            out.is_empty(),
            "an undecodable payload must still be dropped from decoded notifies"
        );
        let seen = sink.notifies.lock().unwrap();
        assert_eq!(
            *seen,
            vec![(SERVICE_UUID, 0x15, garbage.to_vec(), false, 99)]
        );
    }

    /// The same undecodable fragment without a sink is dropped in silence,
    /// exactly as before — the diagnostic seam must not change what a normal
    /// run decodes.
    #[test]
    fn undecodable_payload_without_a_sink_is_dropped_silently() {
        let body = build_notify_body(0x15, b"not-actually-zstd");
        let frame = build_frame(2, true, &body);
        let mut out = Vec::new();

        parse_frame(&frame, 0, &mut out, None, 0);

        assert!(out.is_empty());
    }
}
