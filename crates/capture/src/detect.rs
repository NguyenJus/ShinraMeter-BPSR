//! Game-server detection: application-layer payload signature matching.
//!
//! No port filter — the game server is identified by scanning TCP payload
//! bytes for a known signature, with a login-return fallback.

use std::fmt;

/// Signature bytes located at [`SERVER_SIGNATURE_OFFSET`] within a
/// length-prefixed fragment of the TCP payload.
pub const SERVER_SIGNATURE: [u8; 6] = [0x00, 0x63, 0x33, 0x53, 0x42, 0x00];
pub const SERVER_SIGNATURE_OFFSET: usize = 5;

/// Login-return fallback: an exact-length payload with two fixed byte runs.
pub const LOGIN_RETURN_SIGNATURE_1: [u8; 10] = [0, 0, 0, 0x62, 0, 3, 0, 0, 0, 1];
pub const LOGIN_RETURN_SIGNATURE_2: [u8; 6] = [0, 0, 0, 0, 0x0a, 0x4e];
pub const LOGIN_RETURN_SIGNATURE_SIZE: usize = 0x62;

/// Bound on fragments walked per payload scan, so a malformed length field
/// cannot spin the scan indefinitely.
const MAX_SCAN_FRAGMENTS: usize = 1000;

/// A TCP 4-tuple identifying one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Conn {
    pub src: [u8; 4],
    pub src_port: u16,
    pub dst: [u8; 4],
    pub dst_port: u16,
}

impl fmt::Display for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}.{}:{} -> {}.{}.{}.{}:{}",
            self.src[0],
            self.src[1],
            self.src[2],
            self.src[3],
            self.src_port,
            self.dst[0],
            self.dst[1],
            self.dst[2],
            self.dst[3],
            self.dst_port,
        )
    }
}

/// Scans a TCP payload for the game-server signature by walking
/// length-prefixed fragments (BE u32 len; fragment body length = `len - 4`)
/// and comparing each fragment's bytes at [`SERVER_SIGNATURE_OFFSET`]
/// against [`SERVER_SIGNATURE`]. Bounded to `MAX_SCAN_FRAGMENTS` fragments;
/// never panics on malformed input.
pub fn looks_like_game_server(payload: &[u8]) -> bool {
    if payload.len() < 10 || payload[4] != 0 {
        return false;
    }

    let sig_end = SERVER_SIGNATURE_OFFSET + SERVER_SIGNATURE.len();
    let mut offset = 0usize;
    for _ in 0..MAX_SCAN_FRAGMENTS {
        if offset + 4 > payload.len() {
            break;
        }
        let len =
            u32::from_be_bytes([payload[offset], payload[offset + 1], payload[offset + 2], payload[offset + 3]])
                as usize;
        if len < 4 {
            break;
        }
        let frag_len = len - 4;
        let frag_start = offset + 4;
        let Some(frag_end) = frag_start.checked_add(frag_len) else {
            break;
        };
        if frag_end > payload.len() {
            break;
        }
        let frag = &payload[frag_start..frag_end];
        if frag.len() >= sig_end && frag[SERVER_SIGNATURE_OFFSET..sig_end] == SERVER_SIGNATURE {
            return true;
        }
        offset = frag_end;
    }
    false
}

/// Login-return fallback signature check.
pub fn is_login_return(payload: &[u8]) -> bool {
    if payload.len() != LOGIN_RETURN_SIGNATURE_SIZE {
        return false;
    }
    if payload[0..10] != LOGIN_RETURN_SIGNATURE_1 {
        return false;
    }
    payload[14..20] == LOGIN_RETURN_SIGNATURE_2
}

/// Derives the `[a, b]` /16 subnet prefix for a detected server connection,
/// taken from whichever endpoint looks non-RFC1918 (checked by first octet
/// only, matching the reference implementation).
pub fn subnet_prefix(conn: &Conn) -> [u8; 2] {
    if conn.src[0] != 10 && conn.src[0] != 172 && conn.src[0] != 192 {
        [conn.src[0], conn.src[1]]
    } else {
        [conn.dst[0], conn.dst[1]]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frag_with_signature_at(offset: usize) -> Vec<u8> {
        // frag[0] must be 0 (payload[4] requirement); signature placed at
        // the given offset within the fragment.
        let end = offset + SERVER_SIGNATURE.len();
        let mut frag = vec![0u8; end.max(1)];
        frag[0] = 0;
        frag[offset..end].copy_from_slice(&SERVER_SIGNATURE);
        frag
    }

    fn payload_with_frag(frag: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        let len = (frag.len() + 4) as u32;
        payload.extend_from_slice(&len.to_be_bytes());
        payload.extend_from_slice(frag);
        payload
    }

    #[test]
    fn signature_at_offset_5_detects() {
        let frag = frag_with_signature_at(SERVER_SIGNATURE_OFFSET);
        let payload = payload_with_frag(&frag);
        assert!(looks_like_game_server(&payload));
    }

    #[test]
    fn signature_at_offset_4_does_not_detect() {
        let frag = frag_with_signature_at(4);
        let payload = payload_with_frag(&frag);
        assert!(!looks_like_game_server(&payload));
    }

    #[test]
    fn too_short_payload_does_not_detect() {
        assert!(!looks_like_game_server(&[0u8; 4]));
    }

    #[test]
    fn login_return_detects() {
        let mut payload = vec![0u8; LOGIN_RETURN_SIGNATURE_SIZE];
        payload[0..10].copy_from_slice(&LOGIN_RETURN_SIGNATURE_1);
        payload[14..20].copy_from_slice(&LOGIN_RETURN_SIGNATURE_2);
        assert!(is_login_return(&payload));
    }

    #[test]
    fn wrong_length_is_not_login_return() {
        let payload = vec![0u8; LOGIN_RETURN_SIGNATURE_SIZE - 1];
        assert!(!is_login_return(&payload));
    }

    #[test]
    fn right_length_wrong_bytes_is_not_login_return() {
        let payload = vec![0u8; LOGIN_RETURN_SIGNATURE_SIZE];
        assert!(!is_login_return(&payload));
    }

    #[test]
    fn public_ip_src_yields_its_own_prefix() {
        let conn = Conn {
            src: [8, 8, 8, 8],
            src_port: 1234,
            dst: [10, 0, 0, 5],
            dst_port: 80,
        };
        assert_eq!(subnet_prefix(&conn), [8, 8]);
    }

    #[test]
    fn rfc1918_src_yields_dst_prefix() {
        let conn = Conn {
            src: [192, 168, 1, 50],
            src_port: 1234,
            dst: [203, 0, 113, 7],
            dst_port: 443,
        };
        assert_eq!(subnet_prefix(&conn), [203, 0]);
    }
}
