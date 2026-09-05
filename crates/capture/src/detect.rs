//! Game-server detection: application-layer payload signature matching.
//!
//! No port filter — the game server is identified by scanning TCP payload
//! bytes for a known signature, with a login-return fallback.

use std::collections::HashSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::owner::{NoOwnerFilter, StreamOwnerLookup};

/// Signature bytes located at [`SERVER_SIGNATURE_OFFSET`] within a
/// length-prefixed fragment of the TCP payload.
pub const SERVER_SIGNATURE: [u8; 6] = [0x00, 0x63, 0x33, 0x53, 0x42, 0x00];
pub const SERVER_SIGNATURE_OFFSET: usize = 5;

/// Login-return fallback: an exact-length payload with two fixed byte runs.
pub const LOGIN_RETURN_SIGNATURE_1: [u8; 10] = [0, 0, 0, 0x62, 0, 3, 0, 0, 0, 1];
pub const LOGIN_RETURN_SIGNATURE_2: [u8; 6] = [0, 0, 0, 0, 0x0a, 0x4e];
pub const LOGIN_RETURN_SIGNATURE_SIZE: usize = 0x62;

/// Bound on candidate signature matches checked per payload scan, so a
/// payload that happens to contain many literal occurrences of
/// [`SERVER_SIGNATURE`] cannot spin the scan indefinitely.
const MAX_SIGNATURE_CANDIDATES: usize = 1000;

/// Payload-size ceiling for the subnet-reconnect path (issue #258).
///
/// The subnet path has no protocol evidence at all — "same /16, some
/// payload" is all it requires — so it is otherwise wide open to adopting
/// the first thing that sends *any* bytes from the known datacenter after a
/// teardown, including an unrelated, co-located service (CDN, patch server)
/// racing the real reconnect. Every legitimate reconnect in the field logs
/// that prompted this issue arrived at 98 bytes ([`LOGIN_RETURN_SIGNATURE_SIZE`])
/// or smaller (real handshake/control fragments); the one mis-adopt carried
/// a full 1400-byte MTU payload — bulk data, not a handshake. This cap sits
/// well above the largest size ever seen from a genuine reconnect and well
/// below the smallest observed false positive, so it closes the race
/// instead of merely papering over it.
///
/// The evidence behind 512 is thin: a single field-log session (11
/// adoptions, one false positive at 1400 bytes) — plausible, not proven.
/// This is a hard, unconditional reject with no fallback: a legitimate
/// first reconnect packet that exceeds it (NIC offload/GRO coalescing
/// several TCP segments into one payload, a VPN changing effective MTU, a
/// protocol revision that grows the handshake) *and* fails the
/// size-independent signature scan tried first
/// ([`looks_like_game_server`], [`is_login_return`]) is rejected outright —
/// the subnet-reconnect path silently never fires for that connection, and
/// capture stays dead until a manual restart. This has not been observed in
/// the field. If it starts happening, [`ServerDetector::detects`] logs the
/// rejection (`log::debug!`, once per connection) precisely so a future
/// field log can settle whether the cap needs to move, instead of leaving a
/// dead capture with no trace.
pub const SUBNET_ADOPTION_MAX_PAYLOAD: usize = 512;

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

/// Scans a TCP payload for the game-server signature.
///
/// Rather than walking length-prefixed fragments (BE u32 len; fragment body
/// length = `len - 4`) starting strictly at `offset = 0`, this locates every
/// literal occurrence of [`SERVER_SIGNATURE`] in the payload directly and,
/// for each one, checks whether the 4 bytes immediately in front of it form
/// a length header describing a fragment that both contains the signature
/// at [`SERVER_SIGNATURE_OFFSET`] and fits inside this payload. That is
/// exactly the local evidence a valid frame boundary would produce, and
/// finding it doesn't require any frame *before* the signature-bearing one
/// to be present or valid.
///
/// `offset = 0` is only guaranteed to be a real frame boundary when capture
/// has observed the connection from its start; a capture that attaches
/// mid-connection (issue #282) sees its first packets at an arbitrary byte
/// offset into the frame stream. Locating the boundary from the signature
/// backward, instead of assuming it forward from `offset = 0`, is what lets
/// this recognize the game server either way. Bounded to
/// `MAX_SIGNATURE_CANDIDATES` candidate matches; never panics on malformed
/// input.
///
/// Returns `Some(header_start)` — the payload-relative byte offset of the
/// validated fragment's length header — rather than just `true` (issue
/// #293). Locating the boundary is exactly what this function already does
/// to validate the match; discarding that position and reporting only a
/// bool left the caller with no way to resync the reassembler onto the
/// boundary it found, so an adoption mid-frame (the same #282 mid-connection
/// attach this function already accounts for by scanning, rather than
/// assuming `offset = 0`) still started the decoder at the packet's own,
/// possibly mid-frame, sequence number. `win.rs`'s `sniff_loop` adds this
/// offset to the adopting packet's TCP sequence number before calling
/// `TcpReassembler::resync`, matching the alignment gate BPSR-ZDPS's
/// `TcpReassembler.AddPacket` applies (checking the stream's very first
/// payload begins on a length-prefix boundary) rather than trusting an
/// arbitrary attach point.
pub fn looks_like_game_server(payload: &[u8]) -> Option<usize> {
    let sig_len = SERVER_SIGNATURE.len();
    let min_len = SERVER_SIGNATURE_OFFSET + sig_len;
    if payload.len() < min_len {
        return None;
    }

    let mut checked = 0usize;
    for sig_pos in 0..=(payload.len() - sig_len) {
        if payload[sig_pos..sig_pos + sig_len] != SERVER_SIGNATURE {
            continue;
        }
        checked += 1;
        if checked > MAX_SIGNATURE_CANDIDATES {
            break;
        }

        let Some(frag_start) = sig_pos.checked_sub(SERVER_SIGNATURE_OFFSET) else {
            continue;
        };
        let Some(header_start) = frag_start.checked_sub(4) else {
            continue;
        };
        if payload[frag_start] != 0 {
            continue;
        }
        let len = u32::from_be_bytes([
            payload[header_start],
            payload[header_start + 1],
            payload[header_start + 2],
            payload[header_start + 3],
        ]) as usize;
        if len < 4 {
            continue;
        }
        let frag_len = len - 4;
        let Some(frag_end) = frag_start.checked_add(frag_len) else {
            continue;
        };
        if frag_end > payload.len() {
            continue;
        }
        if frag_end - frag_start >= SERVER_SIGNATURE_OFFSET + sig_len {
            return Some(header_start);
        }
    }
    None
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
/// Three requirements beyond the subnet match, all load-bearing:
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
/// * **Size.** Capped at [`SUBNET_ADOPTION_MAX_PAYLOAD`] (issue #258): a
///   full-MTU-sized payload on a brand-new connection is bulk data from an
///   unrelated, co-located service, not the small handshake fragment a real
///   reconnect sends first.
pub fn subnet_adoption_eligible(conn: &Conn, payload: &[u8], known_subnet: [u8; 2]) -> bool {
    !payload.is_empty()
        && payload.len() <= SUBNET_ADOPTION_MAX_PAYLOAD
        && !is_private(conn.src)
        && [conn.src[0], conn.src[1]] == known_subnet
}

/// True if `conn`/`payload` satisfy every [`subnet_adoption_eligible`]
/// requirement *except* the payload-size cap — i.e. [`SUBNET_ADOPTION_MAX_PAYLOAD`]
/// is the sole reason the candidate is being turned away. Used only to
/// decide whether a rejection is worth a log line (see
/// [`ServerDetector::detects`]); has no bearing on the adoption decision
/// itself.
fn subnet_adoption_rejected_only_by_size(
    conn: &Conn,
    payload: &[u8],
    known_subnet: [u8; 2],
) -> bool {
    payload.len() > SUBNET_ADOPTION_MAX_PAYLOAD
        && !is_private(conn.src)
        && [conn.src[0], conn.src[1]] == known_subnet
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
/// entirely — its FIN/RST says nothing about the one being tracked).
///
/// FIN and RST are not equivalent here because TCP half-close is a real
/// thing: a FIN on [`ConnStreamRole::Reverse`] (client → server) only says
/// "the client is done *sending*" — the server may still have bytes left to
/// deliver on the still-open server → client half, and treating that as a
/// full teardown clears `known_server` while the connection is alive. The
/// next legitimate server → client packet then classifies as `Unrelated`,
/// re-enters `detects()`, and is silently dropped (ordinary continuation
/// bytes match neither the signature nor the subnet-reconnect path, whose
/// candidate set already contains this connection from the original
/// `adopt()`). So only a FIN on [`ConnStreamRole::Adopted`] (server → client)
/// counts: the server itself has no more to send. RST, by contrast, aborts
/// both halves of the connection regardless of which direction carried it,
/// so it is a teardown from either role.
pub fn is_teardown_of_known(role: ConnStreamRole, fin: bool, rst: bool) -> bool {
    match role {
        ConnStreamRole::Adopted => fin || rst,
        ConnStreamRole::Reverse => rst,
        ConnStreamRole::Unrelated => false,
    }
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
    /// Subnet-path candidates already logged as rejected solely by
    /// [`SUBNET_ADOPTION_MAX_PAYLOAD`], so a connection that keeps sending
    /// oversized packets logs once instead of once per packet. Purely an
    /// observability aid — never consulted by the adoption decision — so,
    /// unlike `subnet_candidates`, dropping an insert once bounded costs
    /// nothing but a rare missed log line. Cleared on the same triggers as
    /// `subnet_candidates` (see [`Self::adopt`], [`Self::reset`]).
    size_capped_candidates: HashSet<Conn>,
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
        self.size_capped_candidates.clear();
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
    ///
    /// [`signature_direction_ok`] guards *both* adoption paths below, not
    /// just the signature scan: a client whose own address happens to be
    /// globally routable and to fall in the server's /16 (no NAT between it
    /// and the server) would otherwise satisfy
    /// [`subnet_adoption_eligible`] on its own outbound traffic — non-private
    /// source, matching subnet, non-empty payload — and get mis-adopted,
    /// reversing the tracked direction.
    pub fn detects(&mut self, conn: &Conn, payload: &[u8], server_adopted: bool) -> bool {
        self.detects_with(conn, payload, server_adopted, &|| true)
    }

    /// As [`Self::detects`], but the ownership check (or any other final
    /// gate) is threaded through as `allow`, called only once the cheap
    /// signature/evidence checks below have already passed and this
    /// connection is about to become a candidate -- never for a connection
    /// that fails those checks on its own.
    ///
    /// This is what lets a caller (`decide_packet`) defer an expensive
    /// per-packet ownership lookup (issue #337) until it is actually needed:
    /// running it for every `Unrelated` packet regardless of whether the
    /// signature scan even matched wastes two `GetExtendedTcpTable` syscalls
    /// and a full-table allocation on packets that were never going to
    /// adopt anyway.
    ///
    /// `allow` is called immediately before each `true` return (and before
    /// the subnet-candidate insertion that return implies), so a candidate
    /// `allow` rejects never consumes a `subnet_candidates` slot -- the same
    /// property the ownership-first ordering in `decide_packet` existed to
    /// preserve.
    pub fn detects_with(
        &mut self,
        conn: &Conn,
        payload: &[u8],
        server_adopted: bool,
        allow: &dyn Fn() -> bool,
    ) -> bool {
        if signature_direction_ok(conn, self.local_endpoint)
            && (looks_like_game_server(payload).is_some() || is_login_return(payload))
        {
            return allow();
        }

        if server_adopted {
            return false;
        }

        let Some(prefix) = self.known_subnet else {
            return false;
        };
        if !signature_direction_ok(conn, self.local_endpoint) {
            return false;
        }
        if !subnet_adoption_eligible(conn, payload, prefix) {
            // Observability for issue #258's size cap (see
            // SUBNET_ADOPTION_MAX_PAYLOAD docs): if this ever fires in the
            // field it means the fallback silently gave up on a real
            // reconnect, which otherwise leaves no trace at all. Logged at
            // most once per connection, and only when the size cap is the
            // sole reason for rejection — a candidate failing for another
            // reason (wrong subnet, empty payload, wrong direction) isn't
            // evidence the cap itself is wrong.
            if subnet_adoption_rejected_only_by_size(conn, payload, prefix)
                && self.size_capped_candidates.len() < Self::MAX_SUBNET_CONNECTIONS
                && self.size_capped_candidates.insert(*conn)
            {
                log::debug!(
                    "capture: subnet-reconnect candidate {conn} rejected solely by the \
                     {SUBNET_ADOPTION_MAX_PAYLOAD}-byte payload cap ({} bytes); see \
                     SUBNET_ADOPTION_MAX_PAYLOAD docs if this recurs",
                    payload.len(),
                );
            }
            return false;
        }
        if self.subnet_candidates.contains(conn) {
            return false;
        }
        if self.subnet_candidates.len() >= Self::MAX_SUBNET_CONNECTIONS {
            return false;
        }
        if !allow() {
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
            self.size_capped_candidates.clear();
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

/// One packet's effect on the adopted-server state machine, decided without
/// touching the reassembler, decoder, or event channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdoptionDecision {
    /// The packet's relationship to the (possibly now-stale) adopted
    /// connection, as classified before any teardown/adoption in this call.
    pub role: ConnStreamRole,
    /// A FIN/RST on this packet tore down the previously-tracked connection
    /// (see [`is_teardown_of_known`]); `known_server` has already been
    /// cleared to reflect it.
    pub torn_down: bool,
    /// The packet must not be reassembled at all (`Reverse`, or `Unrelated`
    /// with no adoption).
    pub skip: bool,
    /// This call is the one that adopted `conn` as the server connection.
    /// The caller must resync the reassembler to `seq + frame_offset` (see
    /// [`frame_offset`](Self::frame_offset)) — not the packet's bare `seq`
    /// — reset the decoder, and emit exactly one
    /// `ProtocolEvent::ServerChanged`, on every `true` here. Never `true` on
    /// any decision but this one, which is what keeps a still-`Adopted`
    /// packet from re-triggering either.
    pub newly_adopted: bool,
    /// The payload-relative byte offset of the frame boundary the adopting
    /// packet was matched at (issue #293), meaningful only when
    /// `newly_adopted` is `true`; `0` otherwise. The caller must resync the
    /// reassembler to `seq + frame_offset`, not the packet's bare `seq`,
    /// or a mid-frame adoption (attaching while the connection is already
    /// mid-stream, issue #282) starts the decoder mid-frame. Sourced from
    /// [`looks_like_game_server`]'s located `header_start` when the
    /// signature-scan path is what matched; the login-return and
    /// subnet-reconnect paths carry no such byte-level evidence, so they
    /// fall back to `0` — the packet's own `seq`, exactly today's
    /// pre-#293 behavior for those two paths.
    pub frame_offset: usize,
}

/// Issue #337's process-ownership filter, bundled into one value so
/// [`decide_packet`] doesn't need two separate parameters for it (and stays
/// under clippy's `too_many_arguments`).
///
/// `game_pids` is empty when the game's own pid(s) have not (yet) been
/// identified — e.g. `owner::find_game_pids` hasn't found any, or ownership
/// lookups are unsupported on this platform — in which case
/// [`owner_allows_adoption`] always allows adoption; there is nothing to
/// filter against.
///
/// This is a set, not a single pid: [`GAME_PROCESS_NAMES`](crate::owner::GAME_PROCESS_NAMES)
/// includes generic names (e.g. `Star.exe`) that more than one running
/// process can share, so `owner::find_game_pids` can legitimately resolve
/// to several candidates at once. Filtering against the whole set is a
/// strict superset of filtering against any one of them — it can only
/// *allow* an adoption a single wrong pick would have rejected — so it
/// keeps the same fail-open contract.
pub struct OwnershipFilter<'a> {
    pub lookup: &'a dyn StreamOwnerLookup,
    pub game_pids: &'a [u32],
}

impl OwnershipFilter<'static> {
    /// No filtering at all: every candidate that clears
    /// `ServerDetector::detects` still adopts, exactly pre-#337 behavior.
    /// `&NoOwnerFilter` is promoted to a `'static` reference to a
    /// zero-sized unit struct, so this needs no lifetime of its own to
    /// borrow from.
    pub fn none() -> Self {
        Self {
            lookup: &NoOwnerFilter,
            game_pids: &[],
        }
    }
}

/// Whether ownership evidence permits adopting `conn` — see
/// [`OwnershipFilter`] for the fail-open contract this implements: an
/// unknown owner (`owner.owner_pid` returning `None`) or an empty
/// `game_pids` set never blocks adoption, and only a *known* owner absent
/// from that set is rejected. This asymmetry is deliberate (issue #337): a
/// false negative here silently leaves capture dead until a manual restart,
/// which is worse than the false positive it would take to adopt a stray
/// secondary stream.
fn owner_allows_adoption(conn: &Conn, ownership: &OwnershipFilter<'_>) -> bool {
    if ownership.game_pids.is_empty() {
        return true;
    }
    // The adoption candidate is server→client (`signature_direction_ok`
    // already gates on this): `dst` is this machine's side, `src` the
    // server's.
    let local = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(conn.dst)), conn.dst_port);
    let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(conn.src)), conn.src_port);
    match ownership.lookup.owner_pid(local, remote) {
        None => true,
        Some(pid) => ownership.game_pids.contains(&pid),
    }
}

/// Decides what a captured packet means for the adopted-server state
/// machine: role classification, teardown detection, and (for an unrelated
/// connection) whether it adopts.
///
/// Pulled out of win.rs's `#[cfg(windows)]` recv loop as a host-testable
/// seam: `classify_connection` -> `detector.detects` -> `detector.adopt` ->
/// "caller must emit `ServerChanged`" is exactly the sequence that a prior
/// bug in this class (fixed in 6363e90 via `classify_connection` /
/// [`ConnStreamRole::Reverse`]) got wrong, and it had no regression test.
/// This function *is* that sequence — everything Win32-specific (WinDivert
/// recv, etherparse slicing, `TcpReassembler`, `Decoder`, the
/// `crossbeam_channel::Sender`) stays in win.rs, which only needs to act on
/// the returned [`AdoptionDecision`]. See win.rs's `sniff_loop` for the call
/// site and how it maps `torn_down`/`skip`/`newly_adopted` onto those side
/// effects.
///
/// `ownership` is issue #337's process-ownership filter — see
/// [`OwnershipFilter`]/[`owner_allows_adoption`] for its fail-open contract.
/// Pass [`OwnershipFilter::none`] to disable filtering entirely (every
/// candidate that passes `detector.detects` still adopts, exactly pre-#337
/// behavior).
pub fn decide_packet(
    detector: &mut ServerDetector,
    known_server: &mut Option<Conn>,
    conn: &Conn,
    payload: &[u8],
    fin: bool,
    rst: bool,
    ownership: &OwnershipFilter<'_>,
) -> AdoptionDecision {
    let role = classify_connection(conn, known_server.as_ref());
    let torn_down = is_teardown_of_known(role, fin, rst);
    if torn_down {
        *known_server = None;
    }
    match role {
        ConnStreamRole::Reverse => AdoptionDecision {
            role,
            torn_down,
            skip: true,
            newly_adopted: false,
            frame_offset: 0,
        },
        ConnStreamRole::Adopted => AdoptionDecision {
            role,
            torn_down,
            skip: false,
            newly_adopted: false,
            frame_offset: 0,
        },
        ConnStreamRole::Unrelated => {
            // Ownership is checked *lazily*, via `detects_with`'s `allow`
            // callback, rather than up front for every packet: the
            // ownership lookup costs two `GetExtendedTcpTable` syscalls
            // plus a full-table allocation (issue #337), and with
            // `game_pids` non-empty the WinDivert filter
            // (`!loopback && ip && tcp`) hands this branch every unrelated
            // TCP packet on the box, not just plausible candidates. Only
            // running it once the cheap signature/evidence checks in
            // `detects_with` have already passed keeps that cost off the
            // packets that were never going to adopt anyway.
            //
            // It still runs *before* the subnet-candidate insertion that a
            // `true` return implies (see `detects_with`'s docs): a
            // candidate ownership rejects must not permanently spend one of
            // `MAX_SUBNET_CONNECTIONS` slots — it could never be
            // reconsidered even after ownership later allowed it (e.g. the
            // game's pid changes, or `find_game_pids` resolves to a
            // different, correct set).
            if !detector.detects_with(conn, payload, known_server.is_some(), &|| {
                owner_allows_adoption(conn, ownership)
            }) {
                AdoptionDecision {
                    role,
                    torn_down,
                    skip: true,
                    newly_adopted: false,
                    frame_offset: 0,
                }
            } else {
                *known_server = Some(*conn);
                detector.adopt(conn);
                // Recovers the frame boundary `detector.detects` already
                // found internally but didn't surface (its contract is a
                // bool covering three different evidence paths, only one of
                // which has a byte-exact offset to report). Re-running the
                // signature scan here is cheap — one packet, at adoption
                // time only, never per-packet on an already-adopted
                // connection — and keeps `ServerDetector::detects`'s
                // existing bool contract, and its ~20 direct callers below,
                // untouched.
                let frame_offset = looks_like_game_server(payload).unwrap_or(0);
                AdoptionDecision {
                    role,
                    torn_down,
                    skip: false,
                    newly_adopted: true,
                    frame_offset,
                }
            }
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
        // `payload_with_frag` puts the length header at the very start of
        // `payload` (issue #293: the returned offset is where that header
        // begins).
        assert_eq!(looks_like_game_server(&payload), Some(0));
    }

    #[test]
    fn signature_at_offset_4_does_not_detect() {
        let frag = frag_with_signature_at(4);
        let payload = payload_with_frag(&frag);
        assert_eq!(looks_like_game_server(&payload), None);
    }

    #[test]
    fn too_short_payload_does_not_detect() {
        assert_eq!(looks_like_game_server(&[0u8; 4]), None);
    }

    #[test]
    fn mid_stream_attach_before_the_frame_boundary_still_detects() {
        // Simulates attaching capture after the game connection is already
        // open (issue #282): the first payload the detector ever sees begins
        // at an arbitrary byte offset into the frame stream -- partway
        // through a frame that started before capture began -- rather than
        // at a true frame boundary the way it would if capture had observed
        // the connection from its start.
        let frag = frag_with_signature_at(SERVER_SIGNATURE_OFFSET);

        let mut full_stream = Vec::new();
        // A preceding frame that would have arrived before capture attached.
        full_stream.extend_from_slice(&20u32.to_be_bytes()); // 4-byte header + 16-byte frag
        full_stream.extend_from_slice(&[0xAAu8; 16]);
        // The signature-bearing frame, immediately after it.
        full_stream.extend_from_slice(&payload_with_frag(&frag));

        // Drop the first 7 bytes: capture joins mid-way through the
        // preceding frame's body, not at offset 0 of any frame.
        let joined_mid_stream = &full_stream[7..];

        // The signature-bearing frame's header sat at absolute offset 20 in
        // `full_stream` (4-byte preceding header + 16-byte preceding body);
        // dropping the first 7 bytes moves it to 13 (issue #293: this is
        // the offset a caller must resync onto, not `0`).
        assert_eq!(looks_like_game_server(joined_mid_stream), Some(13));
    }

    #[test]
    fn non_game_traffic_is_still_rejected() {
        // Guard against false positives from the mid-stream scan: a stream
        // that never carries the signature bytes anywhere must not detect,
        // no matter how many candidate start offsets get tried.
        let mut noise = Vec::with_capacity(600);
        let mut state: u32 = 0x1234_5678;
        for _ in 0..600 {
            // Simple xorshift PRNG -- deterministic, no extra dependency.
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            noise.push((state & 0xFF) as u8);
        }
        assert_eq!(looks_like_game_server(&noise), None);
    }

    #[test]
    fn scan_terminates_when_candidates_exceed_the_cap() {
        // Guard against a future off-by-one or misplaced increment in the
        // MAX_SIGNATURE_CANDIDATES cap logic: build a payload with more
        // literal SERVER_SIGNATURE occurrences than the cap allows, none of
        // which form a valid frame header, and assert the scan still
        // terminates and correctly reports no detection instead of, say,
        // panicking or looping past the payload's bounds.
        let block_len = SERVER_SIGNATURE.len() + 10; // signature + 0xFF filler
        let block_count = MAX_SIGNATURE_CANDIDATES + 200;
        let mut payload = Vec::with_capacity(block_len * block_count);
        for _ in 0..block_count {
            payload.extend_from_slice(&SERVER_SIGNATURE);
            payload.extend_from_slice(&[0xFFu8; 10]);
        }

        assert_eq!(looks_like_game_server(&payload), None);
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
        assert_eq!(looks_like_game_server(b""), None);
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

    // --- subnet-path payload-size ceiling (issue #258) ---

    #[test]
    fn subnet_path_rejects_a_full_mtu_payload_even_with_matching_subnet_and_direction() {
        // Regression for issue #258: a co-located non-game connection in the
        // known subnet (e.g. a CDN/patch server sharing the datacenter's
        // /16) can race the real reconnect and win purely because it sent
        // *some* payload first. Every legitimate reconnect observed in the
        // field logs came in at 98 bytes or smaller; the mis-adopt that
        // prompted this issue carried a full 1400-byte MTU payload. Capping
        // the subnet path's payload size keeps it from ever winning that
        // race, rather than merely cleaning up after it wins.
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));
        let big_payload = vec![0u8; SUBNET_ADOPTION_MAX_PAYLOAD + 1];
        assert!(!d.detects(
            &server_to_client([203, 0, 113, 9], 5001),
            &big_payload,
            false
        ));
    }

    #[test]
    fn subnet_path_accepts_a_payload_at_the_size_cap() {
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));
        let payload = vec![0u8; SUBNET_ADOPTION_MAX_PAYLOAD];
        assert!(d.detects(&server_to_client([203, 0, 113, 9], 5001), &payload, false));
    }

    #[test]
    fn subnet_path_rejects_a_full_mtu_decoy_ahead_of_the_real_login_return() {
        // The exact issue #258 shape: a teardown re-arms detection
        // (`server_adopted = false`), a decoy connection in the known
        // subnet arrives first with a full-MTU payload, and the real
        // reconnect's login-return packet follows immediately after. Only
        // the second call may return `true` — win.rs sends
        // `ProtocolEvent::ServerChanged` exactly once per `true` result, so
        // this is the host-testable equivalent of "exactly one
        // ServerChanged is emitted, and it adopts the real flow."
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));

        let decoy = server_to_client([203, 0, 113, 200], 5003);
        let decoy_payload = vec![0xABu8; 1400];
        assert!(
            !d.detects(&decoy, &decoy_payload, false),
            "a full-MTU payload from a brand-new subnet connection must not win the reconnect race"
        );

        let mut real_payload = vec![0u8; LOGIN_RETURN_SIGNATURE_SIZE];
        real_payload[0..10].copy_from_slice(&LOGIN_RETURN_SIGNATURE_1);
        real_payload[14..20].copy_from_slice(&LOGIN_RETURN_SIGNATURE_2);
        let real = server_to_client([203, 0, 113, 201], 10137);
        assert!(d.detects(&real, &real_payload, false));
    }

    #[test]
    fn reset_forgets_the_known_subnet() {
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));
        d.reset();
        assert!(!d.detects(&server_to_client([203, 0, 113, 9], 5001), b"payload", false));
    }

    #[test]
    fn subnet_path_rejects_a_packet_sourced_from_the_local_endpoint_even_in_matching_subnet() {
        // Non-NAT case: the client's own address is globally routable and
        // happens to fall in the server's /16. Without the direction guard
        // on the subnet-reconnect path, the client's own outbound packet to
        // some other host in that /16 satisfies `subnet_adoption_eligible`
        // (non-private source, matching subnet, non-empty payload) and would
        // get mis-adopted, reversing the tracked direction and corrupting
        // `local_endpoint` (`adopt()` sets it to `conn.dst`).
        let mut d = ServerDetector::new();
        d.adopt(&Conn {
            src: [203, 0, 113, 7],
            src_port: 5000,
            dst: [203, 0, 113, 50], // client's own public IP, same /16 as server
            dst_port: 55_000,
        });

        let client_own_outbound_packet = Conn {
            src: [203, 0, 113, 50], // == local_endpoint
            src_port: 55_001,
            dst: [203, 0, 113, 99],
            dst_port: 443,
        };
        assert!(!d.detects(&client_own_outbound_packet, b"payload", false));
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
    fn fin_on_the_reverse_direction_is_not_a_teardown() {
        // Half-close: a client FIN only means "I'm done sending" — the
        // server's half of the connection may still be open. Treating it as
        // a full teardown was the bug (see module doc on
        // `is_teardown_of_known`).
        assert!(!is_teardown_of_known(ConnStreamRole::Reverse, true, false));
    }

    #[test]
    fn rst_on_the_reverse_direction_is_still_a_teardown() {
        // Unlike FIN, RST aborts both halves of the connection regardless of
        // which direction carried it.
        assert!(is_teardown_of_known(ConnStreamRole::Reverse, false, true));
    }

    #[test]
    fn adopted_connection_without_fin_or_rst_is_not_a_teardown() {
        assert!(!is_teardown_of_known(ConnStreamRole::Adopted, false, false));
    }

    #[test]
    fn fin_on_an_unrelated_connection_is_not_a_teardown() {
        assert!(!is_teardown_of_known(ConnStreamRole::Unrelated, true, true));
    }

    // --- `decide_packet` (win.rs adopt/emit sequence, extracted host-testable) ---

    fn login_return_payload() -> Vec<u8> {
        let mut payload = vec![0u8; LOGIN_RETURN_SIGNATURE_SIZE];
        payload[0..10].copy_from_slice(&LOGIN_RETURN_SIGNATURE_1);
        payload[14..20].copy_from_slice(&LOGIN_RETURN_SIGNATURE_2);
        payload
    }

    #[test]
    fn decide_packet_adopts_once_then_never_re_emits_for_the_same_connection() {
        // The double-ServerChanged case: win.rs must send exactly one
        // `ServerChanged` per adoption, never one per packet on an
        // already-adopted connection.
        let mut detector = ServerDetector::new();
        let mut known_server = None;
        let conn = server_to_client([203, 0, 113, 7], 5000);
        let payload = login_return_payload();

        let first = decide_packet(
            &mut detector,
            &mut known_server,
            &conn,
            &payload,
            false,
            false,
            &OwnershipFilter::none(),
        );
        assert_eq!(first.role, ConnStreamRole::Unrelated);
        assert!(!first.skip);
        assert!(first.newly_adopted);
        assert_eq!(known_server, Some(conn));

        // Same connection, next packet: now classifies as `Adopted`, must
        // not re-adopt or signal a second `ServerChanged`.
        let second = decide_packet(
            &mut detector,
            &mut known_server,
            &conn,
            b"more bytes",
            false,
            false,
            &OwnershipFilter::none(),
        );
        assert_eq!(second.role, ConnStreamRole::Adopted);
        assert!(!second.skip);
        assert!(!second.newly_adopted);
        assert_eq!(known_server, Some(conn));
    }

    #[test]
    fn decide_packet_skips_the_reverse_direction_without_touching_known_server() {
        let mut detector = ServerDetector::new();
        let adopted = server_to_client([203, 0, 113, 7], 5000);
        let mut known_server = Some(adopted);
        detector.adopt(&adopted);

        // The client→server half of the same connection: the reversed tuple.
        let reverse = Conn {
            src: adopted.dst,
            src_port: adopted.dst_port,
            dst: adopted.src,
            dst_port: adopted.src_port,
        };
        let decision = decide_packet(
            &mut detector,
            &mut known_server,
            &reverse,
            b"client bytes",
            false,
            false,
            &OwnershipFilter::none(),
        );
        assert_eq!(decision.role, ConnStreamRole::Reverse);
        assert!(decision.skip);
        assert!(!decision.newly_adopted);
        assert!(!decision.torn_down);
        assert_eq!(known_server, Some(adopted));
    }

    #[test]
    fn decide_packet_reports_teardown_and_clears_known_server() {
        let mut detector = ServerDetector::new();
        let adopted = server_to_client([203, 0, 113, 7], 5000);
        let mut known_server = Some(adopted);
        detector.adopt(&adopted);

        let decision = decide_packet(
            &mut detector,
            &mut known_server,
            &adopted,
            b"",
            true,
            false,
            &OwnershipFilter::none(),
        );
        assert_eq!(decision.role, ConnStreamRole::Adopted);
        assert!(decision.torn_down);
        assert!(!decision.skip);
        assert_eq!(known_server, None);
    }

    #[test]
    fn decide_packet_leaves_unrelated_non_matching_traffic_alone() {
        let mut detector = ServerDetector::new();
        let mut known_server = None;
        let conn = server_to_client([203, 0, 113, 7], 5000);
        let decision = decide_packet(
            &mut detector,
            &mut known_server,
            &conn,
            b"not a signature",
            false,
            false,
            &OwnershipFilter::none(),
        );
        assert_eq!(decision.role, ConnStreamRole::Unrelated);
        assert!(decision.skip);
        assert!(!decision.newly_adopted);
        assert_eq!(known_server, None);
    }

    /// issue #337 perf follow-up: the ownership lookup `detects_with`'s
    /// `allow` callback wraps must never run for a packet that fails the
    /// cheap signature/evidence checks on its own -- that's the whole point
    /// of deferring it (two `GetExtendedTcpTable` syscalls plus a
    /// full-table allocation, once per `Unrelated` packet, is exactly what
    /// running it unconditionally cost before this fix). It must still run
    /// for a packet that does pass those checks, so ownership can still
    /// reject it.
    #[test]
    fn detects_with_only_calls_allow_once_the_cheap_checks_pass() {
        use std::cell::Cell;

        let calls = Cell::new(0u32);
        let allow = || {
            calls.set(calls.get() + 1);
            true
        };

        let mut detector = ServerDetector::new();
        let conn = server_to_client([203, 0, 113, 7], 5000);

        // Fails the cheap signature/login-return/subnet checks: `allow`
        // must not be called at all.
        let missed = detector.detects_with(&conn, b"not a signature", false, &allow);
        assert!(!missed);
        assert_eq!(calls.get(), 0, "allow() must not run for a rejected packet");

        // Passes the signature scan: `allow` must be consulted exactly
        // once, immediately before the `true` return.
        let matched = detector.detects_with(&conn, &login_return_payload(), false, &allow);
        assert!(matched);
        assert_eq!(
            calls.get(),
            1,
            "allow() must run once a candidate is otherwise about to be accepted"
        );
    }

    /// issue #293: an adoption whose evidence is the signature scan must
    /// report the frame boundary it actually found, not `0` — otherwise a
    /// capture that attaches mid-connection (issue #282, the exact case
    /// `looks_like_game_server`'s own mid-stream-attach test covers) starts
    /// the decoder at the packet's raw `seq`, which can land mid-frame.
    #[test]
    fn decide_packet_reports_the_signature_paths_frame_offset_on_adoption() {
        let frag = frag_with_signature_at(SERVER_SIGNATURE_OFFSET);

        let mut full_stream = Vec::new();
        // A preceding frame that would have arrived before capture attached
        // — same construction as
        // `mid_stream_attach_before_the_frame_boundary_still_detects`.
        full_stream.extend_from_slice(&20u32.to_be_bytes());
        full_stream.extend_from_slice(&[0xAAu8; 16]);
        full_stream.extend_from_slice(&payload_with_frag(&frag));
        let joined_mid_stream = &full_stream[7..];

        let mut detector = ServerDetector::new();
        let mut known_server = None;
        let conn = server_to_client([203, 0, 113, 7], 5000);
        let decision = decide_packet(
            &mut detector,
            &mut known_server,
            &conn,
            joined_mid_stream,
            false,
            false,
            &OwnershipFilter::none(),
        );

        assert!(decision.newly_adopted);
        assert_eq!(
            decision.frame_offset, 13,
            "the caller must resync to seq + 13, not the packet's bare seq"
        );
    }

    /// The login-return and subnet-reconnect paths carry no byte-level frame
    /// evidence at all — `frame_offset` must fall back to `0` (today's
    /// pre-#293 behavior: resync to the packet's own `seq`) rather than, say,
    /// a leftover value from a previous call.
    #[test]
    fn decide_packet_frame_offset_is_zero_for_a_login_return_adoption() {
        let mut detector = ServerDetector::new();
        let mut known_server = None;
        let conn = server_to_client([203, 0, 113, 7], 5000);
        let decision = decide_packet(
            &mut detector,
            &mut known_server,
            &conn,
            &login_return_payload(),
            false,
            false,
            &OwnershipFilter::none(),
        );

        assert!(decision.newly_adopted);
        assert_eq!(decision.frame_offset, 0);
    }

    // --- size-cap rejection logging bookkeeping (finding: observability) ---

    #[test]
    fn size_capped_rejection_bookkeeping_does_not_consume_the_real_candidate_budget() {
        // `size_capped_candidates` exists purely so a rejection gets logged
        // once instead of once per packet; it must not share state with
        // `subnet_candidates`, which gates how many connections the
        // reconnect path is actually willing to try.
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));
        let big_payload = vec![0u8; SUBNET_ADOPTION_MAX_PAYLOAD + 1];

        // Repeatedly reject more oversized candidates than the real budget
        // (`MAX_SUBNET_CONNECTIONS`) would allow.
        for port in 5001..5001 + ServerDetector::MAX_SUBNET_CONNECTIONS as u16 * 2 {
            assert!(!d.detects(
                &server_to_client([203, 0, 113, 200], port),
                &big_payload,
                false
            ));
        }

        // The real adoption budget is untouched: legitimately small
        // candidates still succeed up to the real cap. `detector_knowing`
        // already consumed one slot via its own `adopt`, so only
        // `MAX_SUBNET_CONNECTIONS - 1` more fit.
        for port in 6000..6000 + (ServerDetector::MAX_SUBNET_CONNECTIONS as u16 - 1) {
            assert!(d.detects(&server_to_client([203, 0, 113, 201], port), b"ok", false));
        }
    }

    #[test]
    fn size_capped_rejection_is_not_relogged_for_the_same_connection() {
        // Calling `detects` again for the exact same over-cap connection must
        // not grow `size_capped_candidates` a second time (keeps the "once
        // per connection" log promise cheap to verify by construction).
        let mut d = detector_knowing(&server_to_client([203, 0, 113, 7], 5000));
        let big_payload = vec![0u8; SUBNET_ADOPTION_MAX_PAYLOAD + 1];
        let candidate = server_to_client([203, 0, 113, 200], 5001);
        assert!(!d.detects(&candidate, &big_payload, false));
        assert_eq!(d.size_capped_candidates.len(), 1);
        assert!(!d.detects(&candidate, &big_payload, false));
        assert_eq!(d.size_capped_candidates.len(), 1);
    }

    // --- issue #337: process-ownership filter ---

    /// A [`StreamOwnerLookup`] fake that reports a fixed pid for every
    /// lookup (or `None`, for "unknown owner") — the fail-open/known-other-
    /// owner contract `owner_allows_adoption` implements is otherwise
    /// untestable off Windows, where the only real implementation lives.
    struct FakeOwnerLookup(Option<u32>);

    impl StreamOwnerLookup for FakeOwnerLookup {
        fn owner_pid(&self, _local: SocketAddr, _remote: SocketAddr) -> Option<u32> {
            self.0
        }
    }

    #[test]
    fn reconnect_to_the_same_endpoint_after_teardown_still_adopts() {
        // A teardown followed by a fresh TCP connection to the *same*
        // server address:port (a plain reconnect, e.g. the client's own
        // ephemeral port changed but the server did not) must still adopt
        // — the reassembler/decoder need to resync onto the new stream.
        // encounter.rs's `ProtocolEvent::ServerChanged` arm keeps
        // players/totals and only performs the session invalidation a
        // reconnect requires either way, so `decide_packet` does not need
        // to (and no longer does) distinguish a reconnect from a genuine
        // server switch here.
        let mut detector = ServerDetector::new();
        let mut known_server = None;
        let first_conn = server_to_client([203, 0, 113, 7], 5000);

        let first = decide_packet(
            &mut detector,
            &mut known_server,
            &first_conn,
            &login_return_payload(),
            false,
            false,
            &OwnershipFilter::none(),
        );
        assert!(first.newly_adopted);

        // Torn down, then a new connection reconnects to the same server
        // address but a different client-side (ephemeral) port.
        let teardown = decide_packet(
            &mut detector,
            &mut known_server,
            &first_conn,
            b"",
            true,
            false,
            &OwnershipFilter::none(),
        );
        assert!(teardown.torn_down);
        assert_eq!(known_server, None);

        let reconnect_conn = Conn {
            src: first_conn.src,
            src_port: first_conn.src_port,
            dst: first_conn.dst,
            dst_port: first_conn.dst_port.wrapping_add(1),
        };
        let reconnect = decide_packet(
            &mut detector,
            &mut known_server,
            &reconnect_conn,
            &login_return_payload(),
            false,
            false,
            &OwnershipFilter::none(),
        );
        assert!(
            reconnect.newly_adopted,
            "must still resync the reassembler/decoder"
        );
    }

    #[test]
    fn a_candidate_stream_owned_by_a_non_game_process_is_not_adopted() {
        // issue #337: a secondary stream that satisfies the payload
        // signature but is demonstrably owned by some other process (a
        // known, non-game pid) must not be adopted at all.
        let mut detector = ServerDetector::new();
        let mut known_server = None;
        let conn = server_to_client([203, 0, 113, 7], 5000);
        let owner = FakeOwnerLookup(Some(9999));
        let game_pids = [1234u32];

        let decision = decide_packet(
            &mut detector,
            &mut known_server,
            &conn,
            &login_return_payload(),
            false,
            false,
            &OwnershipFilter {
                lookup: &owner,
                game_pids: &game_pids,
            },
        );
        assert!(!decision.newly_adopted);
        assert!(decision.skip);
        assert_eq!(known_server, None);
    }

    #[test]
    fn a_candidate_stream_with_an_unknown_owner_is_still_adopted() {
        // The ownership filter must fail open: `owner_pid` returning `None`
        // (a lookup race, an unsupported platform, ...) must never itself
        // block an otherwise-valid adoption.
        let mut detector = ServerDetector::new();
        let mut known_server = None;
        let conn = server_to_client([203, 0, 113, 7], 5000);
        let owner = FakeOwnerLookup(None);
        let game_pids = [1234u32];

        let decision = decide_packet(
            &mut detector,
            &mut known_server,
            &conn,
            &login_return_payload(),
            false,
            false,
            &OwnershipFilter {
                lookup: &owner,
                game_pids: &game_pids,
            },
        );
        assert!(decision.newly_adopted);
        assert_eq!(known_server, Some(conn));
    }

    #[test]
    fn a_candidate_stream_owned_by_the_game_process_is_adopted() {
        let mut detector = ServerDetector::new();
        let mut known_server = None;
        let conn = server_to_client([203, 0, 113, 7], 5000);
        let owner = FakeOwnerLookup(Some(1234));
        let game_pids = [1234u32];

        let decision = decide_packet(
            &mut detector,
            &mut known_server,
            &conn,
            &login_return_payload(),
            false,
            false,
            &OwnershipFilter {
                lookup: &owner,
                game_pids: &game_pids,
            },
        );
        assert!(decision.newly_adopted);
        assert_eq!(known_server, Some(conn));
    }

    #[test]
    fn a_candidate_stream_owned_by_any_pid_in_the_game_pid_set_is_adopted() {
        // Issue #337 (O5): `GAME_PROCESS_NAMES` includes generic names that
        // more than one running process can match, so `game_pids` can carry
        // several candidates at once. Ownership must allow a connection
        // owned by *any* of them, not just the first one resolved.
        let mut detector = ServerDetector::new();
        let mut known_server = None;
        let conn = server_to_client([203, 0, 113, 7], 5000);
        let owner = FakeOwnerLookup(Some(5678));
        let game_pids = [1234u32, 5678u32];

        let decision = decide_packet(
            &mut detector,
            &mut known_server,
            &conn,
            &login_return_payload(),
            false,
            false,
            &OwnershipFilter {
                lookup: &owner,
                game_pids: &game_pids,
            },
        );
        assert!(decision.newly_adopted);
        assert_eq!(known_server, Some(conn));
    }

    #[test]
    fn a_subnet_candidate_rejected_by_ownership_is_later_adoptable() {
        // Issue #337 (O3): `detects` records every subnet-path candidate it
        // sees in `subnet_candidates` so the same connection is never
        // re-adopted twice — but that record must not be made for a
        // candidate the ownership filter is about to reject anyway, or the
        // connection is permanently blacklisted (and burns one of
        // `MAX_SUBNET_CONNECTIONS` slots) even though ownership would allow
        // it moments later, e.g. once `find_game_pids` resolves correctly.
        let known_conn = server_to_client([203, 0, 113, 7], 5000);
        let mut detector = detector_knowing(&known_conn);
        let mut known_server = None; // torn down; the subnet-reconnect path is armed
        let candidate = server_to_client([203, 0, 113, 9], 5001);
        let payload = b"payload";

        let owner = FakeOwnerLookup(Some(9999));
        let game_pids = [1234u32];
        let rejected = decide_packet(
            &mut detector,
            &mut known_server,
            &candidate,
            payload,
            false,
            false,
            &OwnershipFilter {
                lookup: &owner,
                game_pids: &game_pids,
            },
        );
        assert!(!rejected.newly_adopted);
        assert!(rejected.skip);
        assert_eq!(known_server, None);

        // Ownership now allows it (e.g. the owning pid now matches). The
        // exact same candidate must still be adoptable.
        let owner = FakeOwnerLookup(Some(1234));
        let allowed = decide_packet(
            &mut detector,
            &mut known_server,
            &candidate,
            payload,
            false,
            false,
            &OwnershipFilter {
                lookup: &owner,
                game_pids: &game_pids,
            },
        );
        assert!(
            allowed.newly_adopted,
            "a candidate rejected only by ownership must remain adoptable once ownership allows it"
        );
        assert_eq!(known_server, Some(candidate));
    }
}
