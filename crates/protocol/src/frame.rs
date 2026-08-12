//! Outer frame splitter, fragment dispatch, and zstd handling (plan §0.5).
//!
//! Wire format, repeated back-to-back in the reassembled TCP byte stream:
//! `[total_len: BE u32 (includes itself)][packet_type: BE u16][body...]`.

use crate::reader::Reader;

pub const COMPRESSION_FLAG: u16 = 0x8000;
pub const TYPE_MASK: u16 = 0x7FFF;
pub const MIN_FRAME_LEN: u32 = 6;
pub const MAX_FRAME_LEN: u32 = 10 * 1024 * 1024;
/// `SERVICE_UUID = 0x0000_0000_6333_5342` — Notify fragments carrying any
/// other service uuid are dropped.
pub const SERVICE_UUID: u64 = 0x0000_0000_6333_5342;
pub const MAX_FRAMEDOWN_DEPTH: usize = 4;

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

/// Result of `split_frames`: the frames found and bytes consumed before
/// either running out of complete frames or hitting a desync. `frames` and
/// `consumed` cover everything parsed *before* the desync point — a desync
/// must not discard frames already parsed earlier in the same buffer.
#[derive(Debug)]
pub struct SplitFrames<'a> {
    pub frames: Vec<&'a [u8]>,
    pub consumed: usize,
    pub desync: bool,
}

/// Splits `stream` into complete outer frames, returning the frames found and
/// the number of bytes consumed (the caller keeps whatever tail remains for
/// the next push). A `total_len` outside `[MIN_FRAME_LEN, MAX_FRAME_LEN]`
/// sets `desync: true`; `frames`/`consumed` still reflect everything parsed
/// before that point, so the caller can keep the good frames and drop only
/// the tail from `consumed` onward.
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
        if total_len < MIN_FRAME_LEN || total_len > MAX_FRAME_LEN {
            return SplitFrames {
                frames,
                consumed,
                desync: true,
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
        desync: false,
    }
}

fn decompress(payload: &[u8]) -> Option<Vec<u8>> {
    zstd::stream::decode_all(payload).ok()
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
    if result.desync {
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
        assert!(!result.desync);
        assert_eq!(result.frames.len(), 2);
        assert_eq!(result.consumed, stream.len());
    }

    #[test]
    fn partial_frame_yields_nothing() {
        let full = build_notify_frame(1, b"abcdef", false);
        let partial = &full[..full.len() - 1];
        let result = split_frames(partial);
        assert!(!result.desync);
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

    #[test]
    fn total_len_too_small_is_desync() {
        let buf = 5u32.to_be_bytes().to_vec();
        let result = split_frames(&buf);
        assert!(result.desync);
        assert!(result.frames.is_empty());
        assert_eq!(result.consumed, 0);
    }

    #[test]
    fn total_len_max_is_desync() {
        let buf = 0xFFFF_FFFFu32.to_be_bytes().to_vec();
        let result = split_frames(&buf);
        assert!(result.desync);
        assert!(result.frames.is_empty());
        assert_eq!(result.consumed, 0);
    }

    #[test]
    fn desync_after_good_frames_preserves_frames_and_consumed() {
        let mut stream = build_notify_frame(1, b"a", false);
        stream.extend(build_notify_frame(2, b"bb", false));
        let good_len = stream.len();
        stream.extend_from_slice(&5u32.to_be_bytes()); // garbage: below MIN_FRAME_LEN
        let result = split_frames(&stream);
        assert!(result.desync);
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
