//! Outer frame splitter, fragment dispatch, and zstd handling (plan §0.5).
//!
//! Wire format, repeated back-to-back in the reassembled TCP byte stream:
//! `[total_len: BE u32 (includes itself)][packet_type: BE u16][body...]`.

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
pub const MAX_FRAMEDOWN_DEPTH: usize = 4;
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
    loop {
        let remaining = match stream.get(consumed..) {
            Some(r) => r,
            None => break,
        };
        if remaining.len() < 4 {
            break;
        }
        let reader = Reader::new(remaining);
        let total_len = match reader.peek_u32() {
            Some(v) => v,
            None => break,
        };
        if total_len > MAX_FRAME_LEN {
            // Trust an over-large length enough to skip its body only when the
            // packet type behind it also looks like a real frame header — a
            // random 4-byte prefix must not make us discard megabytes of good
            // stream. Without that evidence (type bytes not buffered yet) the
            // length is not trusted either.
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
fn decompress(payload: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;

    let decoder = zstd::stream::read::Decoder::new(payload).ok()?;
    // One byte past the cap, so "too large" is detectable without ever
    // buffering more than `MAX_FRAME_LEN + 1` bytes.
    let mut limited = decoder.take(u64::from(MAX_FRAME_LEN) + 1);
    let mut out = Vec::new();
    limited.read_to_end(&mut out).ok()?;
    if out.len() > MAX_FRAME_LEN as usize {
        log::debug!("bpsr-protocol: zstd output exceeded MAX_FRAME_LEN, dropping fragment");
        return None;
    }
    Some(out)
}

/// Parses one complete outer frame (as produced by `split_frames`), pushing
/// any decoded `Notify` fragments onto `out`. Handles `Notify` and
/// `FrameDown` only; every other fragment type is a silent no-op. Never
/// panics or propagates an error for malformed bodies — it just drops them.
pub fn parse_frame(frame: &[u8], depth: usize, out: &mut Vec<Notify>) {
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
        FragmentType::Notify => handle_notify(body, is_zstd, out),
        FragmentType::FrameDown => handle_frame_down(body, is_zstd, depth, out),
        _ => {}
    }
}

fn handle_notify(body: &[u8], is_zstd: bool, out: &mut Vec<Notify>) {
    let mut reader = Reader::new(body);
    let service_uuid = match reader.read_u64() {
        Some(v) => v,
        None => return,
    };
    if service_uuid != SERVICE_UUID {
        return;
    }
    let _stub_id = match reader.read_u32() {
        Some(v) => v,
        None => return,
    };
    let method_id = match reader.read_u32() {
        Some(v) => v,
        None => return,
    };
    let raw_payload = reader.read_rest();
    let payload = if is_zstd {
        match decompress(raw_payload) {
            Some(p) => p,
            None => {
                log::debug!("bpsr-protocol: zstd decode failed for Notify payload");
                return;
            }
        }
    } else {
        raw_payload.to_vec()
    };
    out.push(Notify { method_id, payload });
}

fn handle_frame_down(body: &[u8], is_zstd: bool, depth: usize, out: &mut Vec<Notify>) {
    if depth >= MAX_FRAMEDOWN_DEPTH {
        return;
    }
    let mut reader = Reader::new(body);
    let _server_sequence_id = match reader.read_u32() {
        Some(v) => v,
        None => return,
    };
    let raw_nested = reader.read_rest();
    let nested = if is_zstd {
        match decompress(raw_nested) {
            Some(p) => p,
            None => {
                log::debug!("bpsr-protocol: zstd decode failed for FrameDown body");
                return;
            }
        }
    } else {
        raw_nested.to_vec()
    };
    let result = split_frames(&nested);
    if result.desync.is_some() {
        log::debug!("bpsr-protocol: desync while splitting FrameDown nested stream");
    }
    for f in result.frames {
        parse_frame(f, depth + 1, out);
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
        let mut buf = Vec::new();
        buf.extend_from_slice(&SERVICE_UUID.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&method_id.to_be_bytes());
        buf.extend_from_slice(payload);
        buf
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
        parse_frame(&frame, 0, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].method_id, 0x06);
        assert_eq!(out[0].payload, b"hello");
    }

    #[test]
    fn zstd_notify_decompresses() {
        let frame = build_notify_frame(0x15, b"payload-data-payload-data", true);
        let mut out = Vec::new();
        parse_frame(&frame, 0, &mut out);
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
        parse_frame(&frame, 0, &mut out);
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
        parse_frame(&current, 0, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn unknown_fragment_type_skipped() {
        let frame = build_frame(99, false, b"whatever");
        let mut out = Vec::new();
        parse_frame(&frame, 0, &mut out);
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
        parse_frame(&frame, 0, &mut out);
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
        parse_frame(&frame, 0, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload.len(), payload.len());
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

    /// An over-large length whose packet type has not arrived yet cannot be
    /// corroborated, so it is not trusted for skipping either.
    #[test]
    fn oversized_len_without_type_bytes_is_unrecoverable() {
        let buf = (MAX_FRAME_LEN + 1).to_be_bytes();
        let result = split_frames(&buf);
        assert_eq!(result.desync, Some(Desync::Unrecoverable));
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
        parse_frame(&frame, 0, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].method_id, 1);
        assert_eq!(out[1].method_id, 2);
    }
}
