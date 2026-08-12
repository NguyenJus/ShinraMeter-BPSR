//! Windows packet capture: WinDivert sniff mode → etherparse → TCP
//! reassembly → protocol decode → `ProtocolEvent` channel.
//!
//! Kept deliberately thin: every branch worth unit-testing (reassembly,
//! server-signature detection, connection-direction classification) lives in
//! [`crate::tcp`] / [`crate::detect`], which are host-tested. This module
//! only wires WinDivert + etherparse to that pure logic and cannot itself be
//! tested off Windows.
//!
//! The driver is driven through the raw [`windivert_sys`] FFI rather than the
//! `windivert` safe wrapper: that wrapper keeps its OS `HANDLE` private and
//! takes `&mut self` in `shutdown`, which cannot be called soundly while the
//! capture thread is parked in `recv` on the same object. Owning the `Copy`
//! `HANDLE` ourselves lets [`CaptureHandle::stop`] unblock `recv` with a
//! plain syscall and no Rust references at all. The `windivert` crate is
//! still used for its open-error classification.

use std::ffi::{CString, c_void};
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use bpsr_protocol::{Decoder, ProtocolEvent};
use crossbeam_channel::Sender;
use etherparse::{NetSlice, SlicedPacket, TransportSlice};
use windivert::error::WinDivertOpenError;
use windivert_sys as sys;
use windivert_sys::address::WINDIVERT_ADDRESS;
use windivert_sys::{WinDivertFlags, WinDivertLayer, WinDivertShutdownMode};
use windows::Win32::Foundation::HANDLE;

use crate::detect::{Conn, ConnStreamRole, ServerDetector, classify_connection};
use crate::error::CaptureError;
use crate::tcp::TcpReassembler;

/// WinDivert filter: every non-loopback TCP/IP packet, in either direction.
const FILTER: &str = "!loopback && ip && tcp";

/// Recv buffer, reused across calls to `WinDivertRecv`.
const RECV_BUFFER_SIZE: usize = 10 * 1024 * 1024;

/// Handle to the running capture thread.
pub struct CaptureHandle {
    stop: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    /// The driver handle, owned as the `Copy` OS value the C API takes. Both
    /// the capture thread (`WinDivertRecv`) and [`Self::stop`]
    /// (`WinDivertShutdown`) pass it to the driver by value; no Rust
    /// reference to a shared object is ever formed, so there is no aliasing
    /// question to answer.
    handle: HANDLE,
    join: JoinHandle<()>,
}

impl CaptureHandle {
    /// Forces the capture loop to forget the currently-known server
    /// connection and re-run detection from scratch on the next packet.
    pub fn request_restart(&self) {
        self.restart.store(true, Ordering::SeqCst);
    }

    /// Signals the capture thread to stop and waits for it to exit.
    ///
    /// `WinDivertRecv` blocks waiting for the next packet, which on a quiet
    /// link (typically: the game already exited) may never return — so
    /// setting the stop flag alone is not enough. `WinDivertShutdown` is the
    /// driver-documented way to unblock a thread parked in a recv on the
    /// same handle from another thread.
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        // SAFETY: `self.handle` is a live WinDivert handle (checked at open,
        // closed only below after the capture thread has joined).
        // `WinDivertShutdown` is documented as callable concurrently with a
        // blocked recv on the same handle, and takes the handle by value.
        unsafe {
            sys::WinDivertShutdown(self.handle, WinDivertShutdownMode::Recv);
        }
        let _ = self.join.join();
        // SAFETY: the capture thread has exited, so nothing can use the
        // handle after this point.
        unsafe {
            sys::WinDivertClose(self.handle);
        }
    }
}

/// Opens a WinDivert sniff-mode handle on all non-loopback TCP/IP traffic
/// and spawns a thread that reassembles the detected game-server's TCP
/// stream and emits decoded [`ProtocolEvent`]s on `tx`.
pub fn start_capture(tx: Sender<ProtocolEvent>) -> Result<CaptureHandle, CaptureError> {
    let filter = CString::new(FILTER).expect("FILTER is a literal without interior NULs");
    let flags = WinDivertFlags::new().set_sniff();
    // SAFETY: `filter` is a valid NUL-terminated C string that outlives the
    // call; the remaining arguments are plain values.
    let handle = unsafe { sys::WinDivertOpen(filter.as_ptr(), WinDivertLayer::Network, 0, flags) };
    if handle.is_invalid() {
        return Err(map_open_error(std::io::Error::last_os_error()));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let restart = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread_restart = Arc::clone(&restart);
    // `HANDLE` is a `Copy` newtype over `isize`; the capture thread gets its
    // own copy of the value, not a reference into shared state.
    let thread_handle = handle;
    let join = thread::spawn(move || recv_loop(thread_handle, tx, thread_stop, thread_restart));

    Ok(CaptureHandle { stop, restart, handle, join })
}

/// Maps a WinDivert open failure to the user-facing [`CaptureError`]
/// variants (§0.7 of the plan): missing driver files / blocked driver vs.
/// missing elevation vs. everything else.
fn map_open_error(err: std::io::Error) -> CaptureError {
    let text = err.to_string();
    match WinDivertOpenError::try_from(err) {
        Ok(WinDivertOpenError::AccessDenied) => CaptureError::NotElevated,
        Ok(
            open_err @ (WinDivertOpenError::MissingSYS
            | WinDivertOpenError::MissingInstall
            | WinDivertOpenError::DriverBlocked
            | WinDivertOpenError::InvalidImageHash
            | WinDivertOpenError::IncompatibleVersion
            | WinDivertOpenError::BaseFilteringEngineDisabled),
        ) => CaptureError::DriverMissing(format!("{open_err:?}")),
        _ => CaptureError::Open(text),
    }
}

/// Blocking single-packet receive. Returns the captured packet bytes (IP
/// header onwards) borrowed from `buffer`, or `None` if the driver call
/// failed (including the shutdown-initiated wakeup).
fn recv_packet<'a>(handle: HANDLE, buffer: &'a mut [u8]) -> Option<&'a [u8]> {
    let mut packet_len: u32 = 0;
    let mut addr = MaybeUninit::<WINDIVERT_ADDRESS>::uninit();
    // SAFETY: `handle` is live for the whole capture thread; `buffer` is a
    // valid writable slice of `buffer.len()` bytes; `packet_len` and `addr`
    // are valid out-parameters. `addr` is only read back on success, when
    // the driver has initialized it.
    let ok = unsafe {
        sys::WinDivertRecv(
            handle,
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len() as u32,
            &mut packet_len,
            addr.as_mut_ptr(),
        )
    };
    if !ok.as_bool() {
        return None;
    }
    let len = (packet_len as usize).min(buffer.len());
    Some(&buffer[..len])
}

fn recv_loop(handle: HANDLE, tx: Sender<ProtocolEvent>, stop: Arc<AtomicBool>, restart: Arc<AtomicBool>) {
    let mut buffer = vec![0u8; RECV_BUFFER_SIZE];
    let mut known_server: Option<Conn> = None;
    let mut detector = ServerDetector::new();
    let mut reassembler = TcpReassembler::new();
    let mut decoder = Decoder::new();

    while !stop.load(Ordering::Relaxed) {
        if restart.swap(false, Ordering::SeqCst) {
            known_server = None;
            detector.reset();
            decoder.reset();
        }

        let Some(packet) = recv_packet(handle, &mut buffer) else {
            continue;
        };

        let Ok(sliced) = SlicedPacket::from_ip(packet) else {
            continue;
        };
        let Some(NetSlice::Ipv4(ipv4)) = sliced.net else {
            continue;
        };
        let Some(TransportSlice::Tcp(tcp)) = sliced.transport else {
            continue;
        };

        let conn = Conn {
            src: ipv4.header().source(),
            src_port: tcp.source_port(),
            dst: ipv4.header().destination(),
            dst_port: tcp.destination_port(),
        };
        let payload = tcp.payload();
        let seq = tcp.sequence_number();

        match classify_connection(&conn, known_server.as_ref()) {
            // The client→server half of the adopted connection: recognized,
            // so detection/adoption does not ping-pong on it, but its bytes
            // belong to that direction's own sequence space and must never
            // reach the server-stream reassembler.
            ConnStreamRole::Reverse => continue,
            ConnStreamRole::Adopted => {}
            ConnStreamRole::Unrelated => {
                if !detector.detects(&conn, payload, known_server.is_some()) {
                    continue;
                }
                known_server = Some(conn);
                detector.adopt(&conn);
                reassembler.resync(seq);
                decoder.reset();
                if tx.send(ProtocolEvent::ServerChanged).is_err() {
                    break;
                }
            }
        }

        reassembler.push(seq, payload);
        if reassembler.take_loss() {
            decoder.reset();
        }
        let stream = reassembler.take_stream();
        if stream.is_empty() {
            continue;
        }

        for event in decoder.push_stream(&stream, now_ms()) {
            if tx.send(event).is_err() {
                return;
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
