//! Game-server detection: application-layer payload signature matching.
//!
//! No port filter — the game server is identified by scanning TCP payload
//! bytes for a known signature, with a login-return fallback.

use std::collections::HashSet;
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
        let len = u32::from_be_bytes([
            payload[offset],
            payload[offset + 1],
            payload[offset + 2],
            payload[offset + 3],
        ]) as usize;
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

/// True if `conn` is the same TCP connection as `known`, regardless of which
/// direction it was captured in — a client→server packet and the matching
/// server→client packet describe one connection but are distinct (reversed)
/// [`Conn`] tuples. Used to recognize "this is still the adopted server
/// connection, just seen from the other direction" without re-running
/// detection (which would otherwise ping-pong-adopt the connection on every
/// packet: see the win.rs adoption bug this exists to prevent).
pub fn same_connection(conn: &Conn, known: &Conn) -> bool {
    conn == known
        || (conn.src == known.dst
            && conn.src_port == known.dst_port
            && conn.dst == known.src
            && conn.dst_port == known.src_port)
}

/// What an observed packet's 4-tuple means relative to the adopted
/// game-server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnStreamRole {
    /// Exactly the adopted direction (server → client). Its payload *is* the
    /// server byte stream and belongs in the reassembler.
    Adopted,
    /// The other direction of the adopted connection (client → server).
    /// Recognized, so it must not re-run detection, but its payload lives in
    /// that direction's own 32-bit sequence space: feeding it to the
    /// server-stream reassembler would bloat the out-of-order cache with
    /// unreachable segments and eventually splice client bytes into the
    /// server stream. Its payload must be dropped.
    Reverse,
    /// Not the adopted connection — or nothing adopted yet. Detection runs.
    Unrelated,
}

/// Classifies `conn` against the currently adopted server connection.
pub fn classify_connection(conn: &Conn, known: Option<&Conn>) -> ConnStreamRole {
    match known {
        Some(known) if conn == known => ConnStreamRole::Adopted,
        Some(known) if same_connection(conn, known) => ConnStreamRole::Reverse,
        _ => ConnStreamRole::Unrelated,
    }
}

/// Derives the `[a, b]` /16 subnet prefix for a detected server connection,
/// taken from whichever endpoint looks non-RFC1918 (checked by first octet
/// only, matching the reference implementation).
pub fn subnet_prefix(conn: &Conn) -> [u8; 2] {
    if is_private(conn.src) {
        [conn.dst[0], conn.dst[1]]
    } else {
        [conn.src[0], conn.src[1]]
    }
}

/// RFC1918-ish check by first octet only, matching the reference
/// implementation (and deliberately coarse: 172.x / 192.x outside the
/// reserved ranges are treated as private too, which only ever costs a
/// missed adoption, never a wrong one).
fn is_private(addr: [u8; 4]) -> bool {
    matches!(addr[0], 10 | 172 | 192)
}

/// Whether a packet may be adopted as the game-server stream purely because
/// it sits in the known /16 subnet.
///
/// Two requirements beyond the subnet match, both load-bearing:
///
/// * **Direction.** The packet's *source* must be the non-RFC1918 endpoint
///   whose /16 is `known_subnet`, i.e. it is a server→client packet. The
///   first packet of a new connection after a channel switch is the client's
///   SYN; adopting that client→server tuple makes every subsequent
///   server→client packet classify as [`ConnStreamRole::Reverse`] and be
///   dropped, with detection never running again — capture silently dead.
/// * **Payload.** Control packets (SYN / SYN-ACK / pure ACK) carry no stream
///   bytes and their raw sequence number is one below the first data byte
///   (SYN consumes a sequence number), so resyncing the reassembler onto one
///   opens a phantom 1-byte gap that stalls the stream.
pub fn subnet_adoption_eligible(conn: &Conn, payload: &[u8], known_subnet: [u8; 2]) -> bool {
    !payload.is_empty() && !is_private(conn.src) && [conn.src[0], conn.src[1]] == known_subnet
}

/// Whether the signature-scan paths (`looks_like_game_server`,
/// `is_login_return`) may treat `conn` as a candidate server connection.
///
/// A blanket non-RFC1918 source guard would reject legitimate LAN- or
/// proxy-hosted servers, so this keys off the one address the process can
/// ever be sure about instead: its own. Once any connection has been
/// adopted, [`ServerDetector`] remembers that connection's client-side
/// address (`local_endpoint`) for the rest of the session — it does not
/// change just because the server connection is later torn down and
/// re-detected. A payload whose *source* is that address is client→server
/// traffic, not a server response, no matter what its bytes happen to look
/// like (e.g. the client echoing signature-shaped bytes back); adopting it
/// would reverse the tracked direction and blind capture until restart.
///
/// Before any connection has ever been adopted, `local_endpoint` is `None`
/// and this always allows the match — there is nothing yet to compare
/// against, and the signature bytes are still the primary evidence.
fn signature_direction_ok(conn: &Conn, local_endpoint: Option<[u8; 4]>) -> bool {
    local_endpoint != Some(conn.src)
}

/// Whether a packet with `role` relative to the currently adopted server
/// connection, carrying `fin`/`rst`, marks that connection's natural
/// teardown.
///
/// Only [`ConnStreamRole::Adopted`] and [`ConnStreamRole::Reverse`] belong to
/// the tracked flow at all (`Unrelated` is a different connection
/// entirely — its FIN/RST says nothing about the one being tracked). A FIN or
/// RST from either direction of that flow means it is ending; the caller
/// must clear its `known_server` so the subnet-reconnect fallback can re-arm
/// instead of waiting on an explicit restart.
pub fn is_teardown_of_known(role: ConnStreamRole, fin: bool, rst: bool) -> bool {
    matches!(role, ConnStreamRole::Adopted | ConnStreamRole::Reverse) && (fin || rst)
}

/// Tracks the game-server detection state that outlives a single packet: the
/// /16 subnet the server was last seen in, and which connections inside that
/// subnet have already been tried by the reconnect path.
///
/// Lives here rather than in `win.rs` so the whole decision — including the
/// direction and payload requirements that keep a client SYN from being
/// adopted as the server stream — is host-testable.
#[derive(Debug, Default)]
pub struct ServerDetector {
    known_subnet: Option<[u8; 2]>,
    subnet_candidates: HashSet<Conn>,
    /// The client-side address (`dst` of the last adopted server→client
    /// tuple), learned once any connection is adopted and kept across
    /// `known_server` clears (a reconnect must not forget its own address).
    /// Only [`Self::reset`] forgets it. See [`signature_direction_ok`].
    local_endpoint: Option<[u8; 4]>,
}

impl ServerDetector {
    /// Bound on how many distinct connections within the known server's /16
    /// subnet get auto-adopted as the new game-server connection (the
    /// "reconnect to the same datacenter" path) before that path gives up
    /// and only the payload-signature scan is tried.
    pub const MAX_SUBNET_CONNECTIONS: usize = 16;

    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets everything learned about the server, so detection re-runs from
    /// scratch (user-requested restart).
    pub fn reset(&mut self) {
        self.known_subnet = None;
        self.subnet_candidates.clear();
        self.local_endpoint = None;
    }

    /// Game-server detection per §0.7: payload signature scan, then the
    /// login-return fallback, then (once a server has ever been seen) the
    /// subnet-tracking path for reconnects to the same datacenter.
    ///
    /// `server_adopted` must be `true` iff a server connection is currently
    /// adopted (win.rs's `known_server.is_some()`). The subnet-reconnect path
    /// is a *reconnect* mechanism only — it must not run while a server is
    /// already adopted, or a co-located non-game connection in the same /16
    /// (e.g. a CDN endpoint) can be adopted over the real, still-live game
    /// connection. The signature paths above are unaffected by this flag:
    /// they legitimately detect channel switches while a server is adopted.
    ///
    /// A `true` result must be followed by [`Self::adopt`].
    pub fn detects(&mut self, conn: &Conn, payload: &[u8], server_adopted: bool) -> bool {
        if signature_direction_ok(conn, self.local_endpoint)
            && (looks_like_game_server(payload) || is_login_return(payload))
        {
            return true;
        }

        if server_adopted {
            return false;
        }

        let Some(prefix) = self.known_subnet else {
            return false;
        };
        if !subnet_adoption_eligible(conn, payload, prefix) || self.subnet_candidates.contains(conn)
        {
            return false;
        }
        if self.subnet_candidates.len() >= Self::MAX_SUBNET_CONNECTIONS {
            return false;
        }
        self.subnet_candidates.insert(*conn);
        true
    }

    /// Records `conn` as the adopted server connection.
    pub fn adopt(&mut self, conn: &Conn) {
        self.local_endpoint = Some(conn.dst);
        let prefix = subnet_prefix(conn);
        // Candidates are keyed by connection alone, so they only mean
        // anything relative to the subnet they were gathered in: on a move to
        // a different /16 they are stale and would otherwise keep consuming
        // the new subnet's cap until the reconnect path is dead. Within one
        // subnet they must survive — they are what stops two connections in
        // the same /16 from re-adopting each other forever, each swap
        // resetting the decoder and wiping the meter.
        if self.known_subnet != Some(prefix) {
            self.subnet_candidates.clear();
        }
        self.known_subnet = Some(prefix);
        // Bounded for the same reason `detects` is: the payload-signature
        // paths adopt without consulting the cap, so a long session of channel
        // switches inside one /16 would otherwise grow this set for the
        // process lifetime. Dropping the insert once the set is full costs
        // nothing — at the cap `detects` already refuses every subnet-path
        // candidate, so there is no re-adoption left for the record to block.
        if self.subnet_candidates.len() < Self::MAX_SUBNET_CONNECTIONS {
            self.subnet_candidates.insert(*conn);
        }
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

    #[test]
    fn same_connection_matches_identical_tuple() {
        let conn = Conn {
            src: [1, 2, 3, 4],
            src_port: 100,
            dst: [10, 0, 0, 5],
            dst_port: 200,
        };
        assert!(same_connection(&conn, &conn));
    }

    #[test]
    fn same_connection_matches_reversed_direction() {
        let server_to_client = Conn {
            src: [1, 2, 3, 4],
            src_port: 100,
            dst: [10, 0, 0, 5],
            dst_port: 200,
        };
        let client_to_server = Conn {
            src: [10, 0, 0, 5],
            src_port: 200,
            dst: [1, 2, 3, 4],
            dst_port: 100,
        };
        assert!(same_connection(&client_to_server, &server_to_client));
    }

    #[test]
    fn same_connection_rejects_a_different_connection() {
        let a = Conn {
            src: [1, 2, 3, 4],
            src_port: 100,
            dst: [10, 0, 0, 5],
            dst_port: 200,
        };
        let b = Conn {
            src: [1, 2, 3, 4],
            src_port: 101,
            dst: [10, 0, 0, 5],
            dst_port: 200,
        };
        assert!(!same_connection(&a, &b));
    }

    fn adopted() -> Conn {
        // server -> client, the direction whose payload carried the signature
        Conn {
            src: [1, 2, 3, 4],
            src_port: 100,
            dst: [10, 0, 0, 5],
            dst_port: 200,
        }
    }

    fn reversed() -> Conn {
        Conn {
            src: [10, 0, 0, 5],
            src_port: 200,
            dst: [1, 2, 3, 4],
            dst_port: 100,
        }
    }

    #[test]
    fn adopted_direction_is_the_server_stream() {
        assert_eq!(
            classify_connection(&adopted(), Some(&adopted())),
            ConnStreamRole::Adopted
        );
    }

    #[test]
    fn reverse_direction_is_recognized_but_not_the_server_stream() {
        // Must not be `Unrelated` (that would re-run detection and
        // ping-pong-adopt), and must not be `Adopted` (client->server bytes
        // live in a different 32-bit sequence space and would corrupt the
        // server reassembler).
        assert_eq!(
            classify_connection(&reversed(), Some(&adopted())),
            ConnStreamRole::Reverse
        );
    }

    #[test]
    fn other_connection_is_unrelated() {
        let other = Conn {
            src: [1, 2, 3, 4],
            src_port: 101,
            dst: [10, 0, 0, 5],
            dst_port: 200,
        };
        assert_eq!(
            classify_connection(&other, Some(&adopted())),
            ConnStreamRole::Unrelated
        );
    }

    #[test]
    fn nothing_adopted_yet_is_unrelated() {
        assert_eq!(
            classify_connection(&adopted(), None),
            ConnStreamRole::Unrelated
        );
    }

    /// server → client packet from a public address, as the adopted stream
    /// direction always looks.
    fn server_to_client(src: [u8; 4], src_port: u16) -> Conn {
        Conn {
            src,
            src_port,
            dst: [192, 168, 1, 50],
            dst_port: 55_000,
        }
    }

    /// client → server packet: the reverse tuple of the above.
    fn client_to_server(dst: [u8; 4], dst_port: u16) -> Conn {
        Conn {
            src: [192, 168, 1, 50],
            src_port: 55_000,
            dst,
            dst_port,
        }
    }

    fn detector_knowing(subnet_of: &Conn) -> ServerDetector {
        let mut d = ServerDetector::new();
        d.adopt(subnet_of);
        d
    }

    #[test]
    fn subnet_path_adopts_a_server_to_client_data_packet() {
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));
        assert!(d.detects(&server_to_client([203, 0, 113, 9], 5001), b"payload", false));
    }

    #[test]
    fn subnet_path_ignores_the_client_to_server_direction() {
        // On a channel switch the *first* packet of the new connection is the
        // client's SYN — a client→server tuple. Adopting it makes every real
        // server→client packet classify as Reverse and get dropped forever.
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));
        assert!(!d.detects(&client_to_server([203, 0, 113, 9], 5001), b"payload", false));
    }

    #[test]
    fn subnet_path_ignores_payload_less_control_packets() {
        // A SYN/ACK carries no stream bytes, and its raw seq is one below the
        // first data byte: adopting on it resyncs the reassembler onto a
        // phantom 1-byte gap.
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));
        assert!(!d.detects(&server_to_client([203, 0, 113, 9], 5001), b"", false));
    }

    #[test]
    fn payload_signature_paths_never_fire_on_an_empty_payload() {
        // Guarantees no adoption path can resync the reassembler onto a
        // payload-less packet.
        assert!(!looks_like_game_server(b""));
        assert!(!is_login_return(b""));
    }

    #[test]
    fn subnet_candidates_are_capped() {
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));
        for port in 0..(ServerDetector::MAX_SUBNET_CONNECTIONS as u16 + 4) {
            let _ = d.detects(
                &server_to_client([203, 0, 113, 8], 6000 + port),
                b"payload",
                false,
            );
        }
        assert!(!d.detects(&server_to_client([203, 0, 113, 8], 9999), b"payload", false));
    }

    #[test]
    fn signature_path_adoptions_cannot_grow_candidates_without_bound() {
        // `detects` returns `true` from the payload-signature path without
        // consulting the cap (a signature match is proof, not a guess), so
        // every channel switch inside one /16 reaches `adopt` with a fresh
        // 4-tuple. If `adopt` inserted unconditionally the set would grow for
        // the process lifetime and silently eat the reconnect path's budget.
        let mut d = ServerDetector::new();
        for port in 0..(ServerDetector::MAX_SUBNET_CONNECTIONS as u16 * 4) {
            d.adopt(&server_to_client([203, 0, 113, 7], 5000 + port));
        }
        assert!(
            d.subnet_candidates.len() <= ServerDetector::MAX_SUBNET_CONNECTIONS,
            "subnet_candidates grew to {}, past the {} cap",
            d.subnet_candidates.len(),
            ServerDetector::MAX_SUBNET_CONNECTIONS,
        );
    }

    #[test]
    fn changing_subnet_clears_stale_candidates() {
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));
        for port in 0..(ServerDetector::MAX_SUBNET_CONNECTIONS as u16 + 4) {
            let _ = d.detects(
                &server_to_client([203, 0, 113, 8], 6000 + port),
                b"payload",
                false,
            );
        }
        // A signature match in a different /16 adopts a server elsewhere; the
        // candidates accumulated under the old subnet must not keep consuming
        // the new subnet's cap.
        d.adopt(&server_to_client([198, 51, 100, 7], 7000));
        assert!(d.detects(
            &server_to_client([198, 51, 100, 9], 7001),
            b"payload",
            false
        ));
    }

    #[test]
    fn same_subnet_adoption_keeps_candidates() {
        // Clearing on every adoption lets two connections in one /16 re-adopt
        // each other forever, resetting the decoder each time.
        let first = server_to_client([203, 0, 113, 7], 5000);
        let mut d = detector_knowing(&first);
        let second = server_to_client([203, 0, 113, 8], 6000);
        assert!(d.detects(&second, b"payload", false));
        d.adopt(&second);
        assert!(!d.detects(&first, b"payload", false));
    }

    #[test]
    fn reset_forgets_the_known_subnet() {
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));
        d.reset();
        assert!(!d.detects(&server_to_client([203, 0, 113, 9], 5001), b"payload", false));
    }

    #[test]
    fn subnet_path_only_runs_when_no_server_currently_adopted() {
        // Regression for: a co-located non-game connection in the same /16
        // (e.g. a CDN endpoint) must not be adopted onto the reassembler
        // while the real game-server connection is still adopted.
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));
        let candidate = server_to_client([203, 0, 113, 200], 6000);
        assert!(!d.detects(&candidate, b"payload", true));
        assert!(d.detects(&candidate, b"payload", false));
    }

    // --- signature-path direction guard (issue #1, item 1) ---

    #[test]
    fn signature_direction_ok_rejects_known_local_source() {
        let local = [192, 168, 1, 50];
        let candidate = client_to_server([1, 2, 3, 4], 80); // src == local
        assert!(!signature_direction_ok(&candidate, Some(local)));
    }

    #[test]
    fn signature_direction_ok_allows_when_local_endpoint_unknown() {
        let candidate = client_to_server([1, 2, 3, 4], 80);
        assert!(signature_direction_ok(&candidate, None));
    }

    #[test]
    fn signature_direction_ok_allows_a_different_source() {
        let local = [192, 168, 1, 50];
        let candidate = server_to_client([1, 2, 3, 4], 100); // src != local
        assert!(signature_direction_ok(&candidate, Some(local)));
    }

    #[test]
    fn detects_rejects_a_signature_match_sourced_from_the_known_local_endpoint() {
        // Once a server connection has been adopted, the client's own IP is
        // known. A later packet whose *source* is that same IP can never be
        // a real server response, even if its payload coincidentally
        // matches the signature (e.g. the client echoing bytes back) --
        // adopting it would reverse the tracked direction and blind capture
        // until restart.
        let mut d = ServerDetector::new();
        d.adopt(&server_to_client([203, 0, 113, 7], 5000)); // dst = 192.168.1.50 (client)

        let frag = frag_with_signature_at(SERVER_SIGNATURE_OFFSET);
        let payload = payload_with_frag(&frag);
        let bogus = Conn {
            src: [192, 168, 1, 50],
            src_port: 9999,
            dst: [1, 2, 3, 4],
            dst_port: 80,
        };
        assert!(!d.detects(&bogus, &payload, false));
    }

    #[test]
    fn detects_accepts_a_signature_match_before_any_local_endpoint_is_known() {
        let mut d = ServerDetector::new();
        let frag = frag_with_signature_at(SERVER_SIGNATURE_OFFSET);
        let payload = payload_with_frag(&frag);
        assert!(d.detects(&server_to_client([203, 0, 113, 7], 5000), &payload, false));
    }

    #[test]
    fn reset_forgets_the_learned_local_endpoint() {
        let mut d = ServerDetector::new();
        d.adopt(&server_to_client([203, 0, 113, 7], 5000));
        d.reset();

        let frag = frag_with_signature_at(SERVER_SIGNATURE_OFFSET);
        let payload = payload_with_frag(&frag);
        let bogus = Conn {
            src: [192, 168, 1, 50],
            src_port: 9999,
            dst: [1, 2, 3, 4],
            dst_port: 80,
        };
        assert!(d.detects(&bogus, &payload, false));
    }

    // --- FIN/RST teardown detection (issue #1, item 2) ---

    #[test]
    fn fin_on_the_adopted_connection_is_a_teardown() {
        assert!(is_teardown_of_known(ConnStreamRole::Adopted, true, false));
    }

    #[test]
    fn rst_on_the_adopted_connection_is_a_teardown() {
        assert!(is_teardown_of_known(ConnStreamRole::Adopted, false, true));
    }

    #[test]
    fn fin_on_the_reverse_direction_is_a_teardown() {
        assert!(is_teardown_of_known(ConnStreamRole::Reverse, true, false));
    }

    #[test]
    fn adopted_connection_without_fin_or_rst_is_not_a_teardown() {
        assert!(!is_teardown_of_known(ConnStreamRole::Adopted, false, false));
    }

    #[test]
    fn fin_on_an_unrelated_connection_is_not_a_teardown() {
        assert!(!is_teardown_of_known(ConnStreamRole::Unrelated, true, true));
    }
}
