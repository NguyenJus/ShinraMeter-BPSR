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

/// Executable names (without the `.exe` suffix match is case-insensitive on
/// Windows, so this list is compared case-insensitively) the game ships
/// under, per BPSR-ZDPS's `NetCapConfig`/`Utils.GetGameCapturePreference`
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

#[cfg(windows)]
pub use windows_impl::SystemOwnerLookup;
#[cfg(windows)]
pub use windows_impl::find_game_pid;

#[cfg(not(windows))]
pub use stub_impl::SystemOwnerLookup;
#[cfg(not(windows))]
pub use stub_impl::find_game_pid;

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
    pub fn find_game_pid() -> Option<u32> {
        None
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
    use super::{GAME_PROCESS_NAMES, StreamOwnerLookup};
    use std::mem::size_of;
    use std::net::{IpAddr, SocketAddr};
    use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
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

    /// Finds the pid of the first running process whose image name matches
    /// [`GAME_PROCESS_NAMES`] (case-insensitive), by walking a
    /// `TH32CS_SNAPPROCESS` snapshot — the same "enumerate every process,
    /// filter by name" approach BPSR-ZDPS's `Utils.GetProcessesFromList`
    /// uses, chosen over `OpenProcess` + name query so this never needs a
    /// per-process access right that a protected game process might deny.
    pub fn find_game_pid() -> Option<u32> {
        // SAFETY: `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)` snapshots
        // every process in the system; the returned handle is closed below
        // on every return path.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
        let found = find_game_pid_in_snapshot(snapshot);
        // SAFETY: `snapshot` is a live handle opened just above and not used
        // again after this call.
        unsafe {
            let _ = CloseHandle(snapshot);
        }
        found
    }

    fn find_game_pid_in_snapshot(snapshot: HANDLE) -> Option<u32> {
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        // SAFETY: `snapshot` is a live TH32CS_SNAPPROCESS handle and `entry`
        // is a valid, correctly-sized out-parameter (`dwSize` set above, as
        // the API requires).
        let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
        while ok {
            if exe_name_matches_game(&entry.szExeFile) {
                return Some(entry.th32ProcessID);
            }
            // SAFETY: same `snapshot`/`entry` as above; `Process32NextW`
            // reuses the same out-parameter contract as `Process32FirstW`.
            ok = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
        }
        None
    }

    /// `szExeFile` is a fixed-size, NUL-terminated UTF-16 buffer; this slices
    /// at the first NUL (or the whole buffer, if somehow unterminated)
    /// before decoding and comparing case-insensitively against
    /// [`GAME_PROCESS_NAMES`].
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

    /// Walks `GetExtendedTcpTable(AF_INET, TCP_TABLE_OWNER_PID_ALL)` for the
    /// row matching `(local_ip, local_port, remote_ip, remote_port)` and
    /// returns its owning pid.
    ///
    /// `MIB_TCPTABLE_OWNER_PID` is a C flexible-array-member struct — the
    /// `table` field windows-rs models as `[MIB_TCPROW_OWNER_PID; 1]` is
    /// only the *first* row; the real row count is `dwNumEntries`, and rows
    /// beyond the first live past the end of the fixed-size struct in the
    /// buffer this function allocates. Every row is read through a raw
    /// pointer offset from that buffer rather than through the `table`
    /// field, precisely so `dwNumEntries > 1` doesn't read out of bounds of
    /// the (size-1) array type.
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
            // SAFETY: `buffer` was just filled by a successful
            // `GetExtendedTcpTable` call using `TCP_TABLE_OWNER_PID_ALL`,
            // which documents its output as a `MIB_TCPTABLE_OWNER_PID`:
            // a `u32` row count followed by that many `MIB_TCPROW_OWNER_PID`
            // rows, back to back, with no padding between the count and the
            // first row on this (LLP64, 4-byte-aligned `u32` fields)
            // target. `buffer` is large enough for that whole layout because
            // the call just reported success writing into it.
            return unsafe {
                find_owner_pid(&buffer, local_ip, local_port, remote_ip, remote_port)
            };
        }
        None
    }

    /// # Safety
    /// `buffer` must hold a successfully-populated `MIB_TCPTABLE_OWNER_PID`:
    /// a `u32` entry count at offset 0, immediately followed by that many
    /// `MIB_TCPROW_OWNER_PID` rows.
    unsafe fn find_owner_pid(
        buffer: &[u8],
        local_ip: [u8; 4],
        local_port: u16,
        remote_ip: [u8; 4],
        remote_port: u16,
    ) -> Option<u32> {
        if buffer.len() < size_of::<u32>() {
            return None;
        }
        let num_entries = u32::from_ne_bytes(buffer[0..4].try_into().unwrap()) as usize;
        let rows_ptr =
            buffer.as_ptr().wrapping_add(size_of::<u32>()) as *const MIB_TCPROW_OWNER_PID;
        let row_size = size_of::<MIB_TCPROW_OWNER_PID>();
        let available = (buffer.len().saturating_sub(size_of::<u32>())) / row_size;
        let num_entries = num_entries.min(available);

        for i in 0..num_entries {
            // SAFETY: `i < num_entries <= available`, so `rows_ptr.add(i)`
            // stays within `buffer` per the row layout documented on
            // `find_owner_pid`'s safety contract, and reading a `Copy`,
            // `repr(C)` POD struct through a suitably-aligned pointer built
            // from a byte buffer is well-defined (no padding bytes are ever
            // treated as anything but plain integers here).
            let row = unsafe { rows_ptr.add(i).read_unaligned() };
            // `dwLocalAddr`/`dwRemoteAddr`/`*Port` are stored in network byte
            // order inside a native-endian `u32` field; `to_ne_bytes` reads
            // back exactly the bytes the API wrote, which is already the
            // address's byte order, and `from_be` undoes the port's 16-bit
            // network-order packing in the low bits.
            let row_local_ip = row.dwLocalAddr.to_ne_bytes();
            let row_local_port = u16::from_be(row.dwLocalPort as u16);
            let row_remote_ip = row.dwRemoteAddr.to_ne_bytes();
            let row_remote_port = u16::from_be(row.dwRemotePort as u16);
            if row_local_ip == local_ip
                && row_local_port == local_port
                && row_remote_ip == remote_ip
                && row_remote_port == remote_port
            {
                return Some(row.dwOwningPid);
            }
        }
        None
    }
}
