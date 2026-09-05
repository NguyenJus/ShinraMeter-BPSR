//! Process-ownership lookup for candidate TCP streams (issue #337).
//!
//! `detect.rs`'s payload-signature and subnet-reconnect paths have no idea
//! *which process* owns a candidate connection — they only ever look at
//! bytes on the wire. That is enough to mis-adopt a secondary connection
//! that happens to carry signature-shaped bytes, or a subnet-reconnect
//! candidate that is actually some other, co-located process reusing the
//! known datacenter's /16. BPSR-ZDPS's `TcpHelper` (see
//! `/tmp/refs/BPSR-ZDPS/BPSR-ZDPSLib/TcpHelper.cs`) closes that gap with
//! `GetExtendedTcpTable(AF_INET, TCP_TABLE_OWNER_PID_ALL)`, which maps a TCP
//! 4-tuple to the owning process id; this module is the same idea, kept
//! host-testable behind [`StreamOwnerLookup`] so `detect.rs`'s adoption
//! logic can be exercised off Windows with a fake.
//!
//! [`SystemOwnerLookup`] is the real, OS-backed implementation: on Windows
//! it walks `GetExtendedTcpTable` for the owning pid and
//! `CreateToolhelp32Snapshot` for the game's own pid (matched by exe name);
//! off Windows — where no capture ever runs — it always reports "unknown",
//! which the adoption gate below treats as fail-open.

use std::net::SocketAddr;

/// Looks up the process id that owns one end of a TCP connection.
///
/// `local`/`remote` follow the same convention `detect.rs` uses for an
/// adoption candidate: `local` is this machine's (the game client's) side,
/// `remote` is the far side (the game server). Implementations are free to
/// ignore either address if the underlying OS API only needs one to
/// disambiguate (Windows' TCP table is keyed by the full 4-tuple, so both
/// are used).
///
/// Returns `None` when the owner cannot be determined — a race with the
/// connection closing, an OS API failure, or (off Windows) simply "not
/// implemented". Callers must treat `None` as *unknown*, not *rejected*:
/// see the fail-open contract on [`crate::detect::decide_packet`].
pub trait StreamOwnerLookup {
    fn owner_pid(&self, local: SocketAddr, remote: SocketAddr) -> Option<u32>;
}

/// A [`StreamOwnerLookup`] that never identifies an owner. The default for
/// platforms (and tests) that have no ownership evidence to offer — paired
/// with `decide_packet`'s fail-open contract, this reproduces pre-#337
/// behavior exactly: every candidate that clears the existing signature/
/// subnet checks still adopts.
pub struct NoOwnerFilter;

impl StreamOwnerLookup for NoOwnerFilter {
    fn owner_pid(&self, _local: SocketAddr, _remote: SocketAddr) -> Option<u32> {
        None
    }
}

/// Executable names (with the `.exe` suffix; the match against a running
/// process's image name is case-insensitive on Windows, so this list is
/// compared case-insensitively too) the game ships under, per BPSR-ZDPS's
/// `NetCapConfig`/`Utils.GetGameCapturePreference`
/// (`/tmp/refs/BPSR-ZDPS/BPSR-ZDPS/Utils.cs`, `EGameCapturePreference.Auto`).
/// Kept in one place so a future store/launcher variant is a one-line
/// addition here rather than a hunt through the capture pipeline.
pub const GAME_PROCESS_NAMES: &[&str] = &[
    "BPSR.exe",
    "BPSR_STEAM.exe",
    "BPSR_EPIC.exe",
    "StarSEA.exe",
    "StarASIA.exe",
    "StarSEA_STEAM.exe",
    "StarASIA_STEAM.exe",
    "Star.exe",
];

/// `szExeFile` is a fixed-size, NUL-terminated UTF-16 buffer (as
/// `PROCESSENTRY32W` on Windows carries it); this slices at the first NUL
/// (or the whole buffer, if somehow unterminated) before decoding and
/// comparing case-insensitively against [`GAME_PROCESS_NAMES`]. Pure, so it
/// is kept out of `#[cfg(windows)]` and exercised directly on any host.
// Only `windows_impl` (and this module's tests) call it; off Windows the
// non-test build has no caller, exactly like `error.rs`'s Windows-only
// helpers.
#[cfg_attr(not(windows), allow(dead_code))]
fn exe_name_matches_game(exe_file: &[u16; 260]) -> bool {
    let len = exe_file
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(exe_file.len());
    let name = String::from_utf16_lossy(&exe_file[..len]);
    GAME_PROCESS_NAMES
        .iter()
        .any(|game_name| name.eq_ignore_ascii_case(game_name))
}

/// Size in bytes of one `MIB_TCPROW_OWNER_PID` row: six back-to-back
/// native-endian `u32` fields — state, localAddr, localPort, remoteAddr,
/// remotePort, owningPid — with no padding between them, per
/// `GetExtendedTcpTable(TCP_TABLE_OWNER_PID_ALL)`'s documented
/// `MIB_TCPTABLE_OWNER_PID` output layout.
#[cfg_attr(not(windows), allow(dead_code))]
const TCP_ROW_SIZE: usize = 6 * 4;

/// Walks a `MIB_TCPTABLE_OWNER_PID`-shaped `buffer` — a `u32` entry count at
/// offset 0, immediately followed by that many [`TCP_ROW_SIZE`]-byte rows —
/// for the row matching `(local_ip, local_port, remote_ip, remote_port)` and
/// returns its owning pid.
///
/// Pure, ordinary byte parsing with no unsafe code of its own — the only
/// unsafe part of the real lookup is getting `buffer` populated by
/// `GetExtendedTcpTable` in the first place (see
/// `windows_impl::owner_pid_from_table`) — so this half is kept out of
/// `#[cfg(windows)]` and unit-tested against a hand-built buffer on any
/// host.
#[cfg_attr(not(windows), allow(dead_code))]
fn find_owner_pid(
    buffer: &[u8],
    local_ip: [u8; 4],
    local_port: u16,
    remote_ip: [u8; 4],
    remote_port: u16,
) -> Option<u32> {
    if buffer.len() < 4 {
        return None;
    }
    let num_entries = u32::from_ne_bytes(buffer[0..4].try_into().unwrap()) as usize;
    let rows = buffer[4..].chunks_exact(TCP_ROW_SIZE);
    let num_entries = num_entries.min(rows.len());

    for row in rows.take(num_entries) {
        // Row layout: state, localAddr, localPort, remoteAddr, remotePort,
        // owningPid, each a native-endian u32. `localAddr`/`remoteAddr` are
        // stored in network byte order inside that u32, so `to_ne_bytes`
        // reads back exactly the bytes the API wrote (already in address
        // order); `*Port` packs the 16-bit network-order port in the low
        // bits, which `from_be` undoes.
        let row_local_addr = u32::from_ne_bytes(row[4..8].try_into().unwrap());
        let row_local_port = u32::from_ne_bytes(row[8..12].try_into().unwrap());
        let row_remote_addr = u32::from_ne_bytes(row[12..16].try_into().unwrap());
        let row_remote_port = u32::from_ne_bytes(row[16..20].try_into().unwrap());
        let row_owning_pid = u32::from_ne_bytes(row[20..24].try_into().unwrap());

        if row_local_addr.to_ne_bytes() == local_ip
            && u16::from_be(row_local_port as u16) == local_port
            && row_remote_addr.to_ne_bytes() == remote_ip
            && u16::from_be(row_remote_port as u16) == remote_port
        {
            return Some(row_owning_pid);
        }
    }
    None
}

#[cfg(windows)]
pub use windows_impl::SystemOwnerLookup;
#[cfg(windows)]
pub use windows_impl::find_game_pids;

#[cfg(not(windows))]
pub use stub_impl::SystemOwnerLookup;
#[cfg(not(windows))]
pub use stub_impl::find_game_pids;

/// Off-Windows stand-in: capture never runs here (see `crate::stub`), so
/// there is no TCP table or process list to query. Exists purely so
/// `detect.rs`'s production call sites — which are host-tested — compile
/// and behave identically to the fail-open default on every platform.
#[cfg(not(windows))]
mod stub_impl {
    use super::StreamOwnerLookup;
    use std::net::SocketAddr;

    #[derive(Default)]
    pub struct SystemOwnerLookup;

    impl SystemOwnerLookup {
        pub fn new() -> Self {
            Self
        }
    }

    impl StreamOwnerLookup for SystemOwnerLookup {
        fn owner_pid(&self, _local: SocketAddr, _remote: SocketAddr) -> Option<u32> {
            None
        }
    }

    /// Always "not found" off Windows — see the module doc.
    pub fn find_game_pids() -> Vec<u32> {
        Vec::new()
    }
}

/// The real, `GetExtendedTcpTable`/`CreateToolhelp32Snapshot`-backed
/// implementation. Runtime behavior cannot be exercised on this
/// (non-Windows) development box — this module only has to satisfy
/// `cargo check`/`clippy` for `--target x86_64-pc-windows-gnu`; see the PR
/// description for the manual verification this still needs on real
/// Windows.
#[cfg(windows)]
mod windows_impl {
    use super::{StreamOwnerLookup, exe_name_matches_game, find_owner_pid};
    use std::net::{IpAddr, SocketAddr};
    use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, TCP_TABLE_OWNER_PID_ALL,
    };
    use windows::Win32::Networking::WinSock::AF_INET;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    #[derive(Default)]
    pub struct SystemOwnerLookup;

    impl SystemOwnerLookup {
        pub fn new() -> Self {
            Self
        }
    }

    impl StreamOwnerLookup for SystemOwnerLookup {
        fn owner_pid(&self, local: SocketAddr, remote: SocketAddr) -> Option<u32> {
            let IpAddr::V4(local_ip) = local.ip() else {
                return None;
            };
            let IpAddr::V4(remote_ip) = remote.ip() else {
                return None;
            };
            owner_pid_from_table(
                local_ip.octets(),
                local.port(),
                remote_ip.octets(),
                remote.port(),
            )
        }
    }

    /// Finds the pids of every running process whose image name matches
    /// [`GAME_PROCESS_NAMES`] (case-insensitive), by walking a
    /// `TH32CS_SNAPPROCESS` snapshot — the same "enumerate every process,
    /// filter by name" approach BPSR-ZDPS's `Utils.GetProcessesFromList`
    /// uses, chosen over `OpenProcess` + name query so this never needs a
    /// per-process access right that a protected game process might deny.
    ///
    /// Returns every match, not just the first (issue #337, O5):
    /// [`GAME_PROCESS_NAMES`] includes generic names (e.g. `Star.exe`) that
    /// more than one running process can share, and latching onto whichever
    /// one happens to be listed first in the snapshot can pick the wrong
    /// pid — which then fails every real candidate's ownership check
    /// closed. Returning the whole matching set and checking membership in
    /// it (see `crate::detect::OwnershipFilter`) is a strict superset of
    /// filtering against a single guess, so it can only allow adoptions a
    /// wrong single pick would have rejected, keeping the fail-open
    /// contract intact.
    pub fn find_game_pids() -> Vec<u32> {
        // SAFETY: `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)` snapshots
        // every process in the system; the returned handle is closed below
        // on every return path.
        let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
            return Vec::new();
        };
        let found = find_game_pids_in_snapshot(snapshot);
        // SAFETY: `snapshot` is a live handle opened just above and not used
        // again after this call.
        unsafe {
            let _ = CloseHandle(snapshot);
        }
        found
    }

    fn find_game_pids_in_snapshot(snapshot: HANDLE) -> Vec<u32> {
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = Vec::new();
        // SAFETY: `snapshot` is a live TH32CS_SNAPPROCESS handle and `entry`
        // is a valid, correctly-sized out-parameter (`dwSize` set above, as
        // the API requires).
        let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
        while ok {
            if exe_name_matches_game(&entry.szExeFile) {
                found.push(entry.th32ProcessID);
            }
            // SAFETY: same `snapshot`/`entry` as above; `Process32NextW`
            // reuses the same out-parameter contract as `Process32FirstW`.
            ok = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
        }
        found
    }

    /// Walks `GetExtendedTcpTable(AF_INET, TCP_TABLE_OWNER_PID_ALL)` for the
    /// row matching `(local_ip, local_port, remote_ip, remote_port)` and
    /// returns its owning pid.
    ///
    /// `MIB_TCPTABLE_OWNER_PID` is a C flexible-array-member struct, so this
    /// deliberately does not model it as a windows-rs type at all: the raw
    /// bytes `GetExtendedTcpTable` writes into `buffer` are handed straight
    /// to [`find_owner_pid`], which parses the row count and every row
    /// (`dwNumEntries` can be, and often is, greater than one) directly out
    /// of the byte slice.
    fn owner_pid_from_table(
        local_ip: [u8; 4],
        local_port: u16,
        remote_ip: [u8; 4],
        remote_port: u16,
    ) -> Option<u32> {
        let mut size: u32 = 0;
        // SAFETY: a null table pointer with `size = 0` is the documented way
        // to ask `GetExtendedTcpTable` for the required buffer size; it
        // writes that size into `size` and returns `ERROR_INSUFFICIENT_BUFFER`
        // without touching any table memory.
        let first = unsafe {
            GetExtendedTcpTable(
                None,
                &mut size,
                false,
                AF_INET.0 as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        };
        if first != ERROR_INSUFFICIENT_BUFFER.0 || size == 0 {
            return None;
        }

        // The table can grow between the sizing call and the real one (a
        // connection opening concurrently); retry a bounded number of times
        // rather than looping forever on a pathologically busy table.
        for _ in 0..4 {
            let mut buffer = vec![0u8; size as usize];
            // SAFETY: `buffer` is `size` bytes, exactly what the previous
            // call reported as sufficient (or a subsequent
            // `ERROR_INSUFFICIENT_BUFFER` grew it, in the same units).
            let status = unsafe {
                GetExtendedTcpTable(
                    Some(buffer.as_mut_ptr() as *mut core::ffi::c_void),
                    &mut size,
                    false,
                    AF_INET.0 as u32,
                    TCP_TABLE_OWNER_PID_ALL,
                    0,
                )
            };
            if status == ERROR_INSUFFICIENT_BUFFER.0 {
                continue;
            }
            if status != 0 {
                return None;
            }
            // `buffer` was just filled by a successful `GetExtendedTcpTable`
            // call using `TCP_TABLE_OWNER_PID_ALL`, which documents its
            // output as a `MIB_TCPTABLE_OWNER_PID`: a `u32` row count
            // followed by that many `MIB_TCPROW_OWNER_PID` rows, back to
            // back — exactly the layout `find_owner_pid` parses.
            return find_owner_pid(&buffer, local_ip, local_port, remote_ip, remote_port);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one `MIB_TCPROW_OWNER_PID`-shaped row (24 bytes): a 4-byte
    /// `state` (unused by `find_owner_pid`, left zeroed), `local_ip`/
    /// `remote_ip` written as plain octets (addresses are already in
    /// network byte order, so no `htons`-style byte swap applies to them),
    /// and each port written the way `htons` actually lays a 16-bit value
    /// out in memory: the big-endian two-byte encoding in the field's low
    /// two bytes, zero-padded to fill the 4-byte `u32` slot.
    fn build_row(
        local_ip: [u8; 4],
        local_port: u16,
        remote_ip: [u8; 4],
        remote_port: u16,
        pid: u32,
    ) -> Vec<u8> {
        let mut row = Vec::with_capacity(TCP_ROW_SIZE);
        row.extend_from_slice(&[0u8; 4]); // state, unused
        row.extend_from_slice(&local_ip);
        row.extend_from_slice(&local_port.to_be_bytes());
        row.extend_from_slice(&[0u8; 2]);
        row.extend_from_slice(&remote_ip);
        row.extend_from_slice(&remote_port.to_be_bytes());
        row.extend_from_slice(&[0u8; 2]);
        row.extend_from_slice(&pid.to_ne_bytes());
        assert_eq!(row.len(), TCP_ROW_SIZE);
        row
    }

    fn build_table(rows: &[Vec<u8>]) -> Vec<u8> {
        let mut buffer = (rows.len() as u32).to_ne_bytes().to_vec();
        for row in rows {
            buffer.extend_from_slice(row);
        }
        buffer
    }

    #[test]
    fn find_owner_pid_matches_the_row_with_the_requested_four_tuple() {
        // Port 0x1F90 (8080) is stored in network byte order: htons(8080)
        // is the byte pair 0x1F, 0x90 — the low two bytes of the row's
        // `dwLocalPort`/`dwRemotePort` field — which is exactly what
        // `build_row` writes and `find_owner_pid` must undo with
        // `u16::from_be` to recover 8080.
        let row = build_row([127, 0, 0, 1], 8080, [93, 184, 216, 34], 443, 4321);
        let buffer = build_table(&[row]);

        let found = find_owner_pid(&buffer, [127, 0, 0, 1], 8080, [93, 184, 216, 34], 443);
        assert_eq!(found, Some(4321));
    }

    #[test]
    fn find_owner_pid_does_not_confuse_local_and_remote() {
        // Swapping local/remote in the query must not still match — this
        // would catch an accidental local/remote argument-order swap in
        // `find_owner_pid` or its caller.
        let row = build_row([127, 0, 0, 1], 8080, [93, 184, 216, 34], 443, 4321);
        let buffer = build_table(&[row]);

        let found = find_owner_pid(&buffer, [93, 184, 216, 34], 443, [127, 0, 0, 1], 8080);
        assert_eq!(found, None);
    }

    #[test]
    fn find_owner_pid_scans_past_the_first_row() {
        let first = build_row([10, 0, 0, 1], 1000, [10, 0, 0, 2], 2000, 111);
        let second = build_row([127, 0, 0, 1], 8080, [93, 184, 216, 34], 443, 4321);
        let buffer = build_table(&[first, second]);

        let found = find_owner_pid(&buffer, [127, 0, 0, 1], 8080, [93, 184, 216, 34], 443);
        assert_eq!(found, Some(4321));
    }

    #[test]
    fn find_owner_pid_returns_none_when_nothing_matches() {
        let row = build_row([10, 0, 0, 1], 1000, [10, 0, 0, 2], 2000, 111);
        let buffer = build_table(&[row]);

        let found = find_owner_pid(&buffer, [127, 0, 0, 1], 8080, [93, 184, 216, 34], 443);
        assert_eq!(found, None);
    }

    #[test]
    fn find_owner_pid_ignores_an_entry_count_beyond_the_buffer() {
        // A malformed/truncated `dwNumEntries` must not be trusted past what
        // the buffer actually holds.
        let row = build_row([127, 0, 0, 1], 8080, [93, 184, 216, 34], 443, 4321);
        let mut buffer = build_table(&[row]);
        buffer[0..4].copy_from_slice(&99u32.to_ne_bytes());

        let found = find_owner_pid(&buffer, [127, 0, 0, 1], 8080, [93, 184, 216, 34], 443);
        assert_eq!(found, Some(4321));
    }

    fn utf16_exe_name(name: &str) -> [u16; 260] {
        let mut buf = [0u16; 260];
        for (dst, src) in buf.iter_mut().zip(name.encode_utf16()) {
            *dst = src;
        }
        buf
    }

    #[test]
    fn exe_name_matches_game_is_case_insensitive() {
        assert!(exe_name_matches_game(&utf16_exe_name("bpsr.exe")));
        assert!(exe_name_matches_game(&utf16_exe_name("BPSR.EXE")));
    }

    #[test]
    fn exe_name_matches_game_rejects_unrelated_names() {
        assert!(!exe_name_matches_game(&utf16_exe_name("explorer.exe")));
    }

    #[test]
    fn exe_name_matches_game_stops_at_the_nul_terminator() {
        // The buffer is fixed-size and NUL-padded; trailing garbage past the
        // terminator must not affect the match.
        let mut buf = utf16_exe_name("BPSR.exe");
        buf[20] = 'X' as u16;
        assert!(exe_name_matches_game(&buf));
    }
}
