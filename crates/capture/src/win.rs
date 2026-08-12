//! Windows packet capture: WinDivert sniff mode → etherparse → TCP
//! reassembly → protocol decode → `ProtocolEvent` channel.
//!
//! Kept deliberately thin: every branch worth unit-testing (reassembly,
//! server-signature detection) lives in [`crate::tcp`] / [`crate::detect`],
//! which are host-tested. This module only wires WinDivert + etherparse to
//! that pure logic and cannot itself be tested off Windows.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use bpsr_protocol::{Decoder, ProtocolEvent};
use crossbeam_channel::Sender;
use etherparse::{NetSlice, SlicedPacket, TransportSlice};
use windivert::error::{WinDivertError, WinDivertOpenError};
use windivert::layer::NetworkLayer;
use windivert::prelude::{WinDivert, WinDivertFlags};

use crate::detect::{Conn, is_login_return, looks_like_game_server, subnet_prefix};
use crate::error::CaptureError;
use crate::tcp::TcpReassembler;

/// WinDivert filter: every non-loopback TCP/IP packet, in either direction.
const FILTER: &str = "!loopback && ip && tcp";

/// Recv buffer, reused across calls to `WinDivert::recv`.
const RECV_BUFFER_SIZE: usize = 10 * 1024 * 1024;

/// Bound on how many distinct connections within the known server's /16
/// subnet get auto-adopted as the new game-server connection (the
/// "reconnect to the same datacenter" path) before that path gives up and
/// only the payload-signature scan is tried.
const MAX_SUBNET_CONNECTIONS: usize = 16;

/// Handle to the running capture thread.
pub struct CaptureHandle {
    stop: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
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
    /// Note: `WinDivert::recv` blocks waiting for the next packet, so this
    /// only returns once the driver delivers (or fails to deliver) one more
    /// packet after the stop flag is set.
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.join.join();
    }
}

/// Opens a WinDivert sniff-mode handle on all non-loopback TCP/IP traffic
/// and spawns a thread that reassembles the detected game-server's TCP
/// stream and emits decoded [`ProtocolEvent`]s on `tx`.
pub fn start_capture(tx: Sender<ProtocolEvent>) -> Result<CaptureHandle, CaptureError> {
    let flags = WinDivertFlags::new().set_sniff();
    let handle: WinDivert<NetworkLayer> = WinDivert::network(FILTER, 0, flags).map_err(map_open_error)?;

    let stop = Arc::new(AtomicBool::new(false));
    let restart = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread_restart = Arc::clone(&restart);
    let join = thread::spawn(move || recv_loop(handle, tx, thread_stop, thread_restart));

    Ok(CaptureHandle { stop, restart, join })
}

/// Maps a WinDivert open failure to the user-facing [`CaptureError`]
/// variants (§0.7 of the plan): missing driver files / blocked driver vs.
/// missing elevation vs. everything else.
fn map_open_error(err: WinDivertError) -> CaptureError {
    match &err {
        WinDivertError::Open(open_err) => match open_err {
            WinDivertOpenError::AccessDenied => CaptureError::NotElevated,
            WinDivertOpenError::MissingSYS
            | WinDivertOpenError::MissingInstall
            | WinDivertOpenError::DriverBlocked
            | WinDivertOpenError::InvalidImageHash
            | WinDivertOpenError::IncompatibleVersion
            | WinDivertOpenError::BaseFilteringEngineDisabled => {
                CaptureError::DriverMissing(format!("{open_err:?}"))
            }
            _ => CaptureError::Open(err.to_string()),
        },
        _ => CaptureError::Open(err.to_string()),
    }
}

fn recv_loop(handle: WinDivert<NetworkLayer>, tx: Sender<ProtocolEvent>, stop: Arc<AtomicBool>, restart: Arc<AtomicBool>) {
    let mut buffer = vec![0u8; RECV_BUFFER_SIZE];
    let mut known_server: Option<Conn> = None;
    let mut known_subnet: Option<[u8; 2]> = None;
    let mut subnet_candidates: HashSet<Conn> = HashSet::new();
    let mut reassembler = TcpReassembler::new();
    let mut decoder = Decoder::new();

    while !stop.load(Ordering::Relaxed) {
        if restart.swap(false, Ordering::SeqCst) {
            known_server = None;
            known_subnet = None;
            subnet_candidates.clear();
            decoder.reset();
        }

        let packet = match handle.recv(Some(&mut buffer)) {
            Ok(packet) => packet,
            Err(_) => continue,
        };

        let Ok(sliced) = SlicedPacket::from_ip(&packet.data) else {
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

        if known_server != Some(conn) {
            if !detects_game_server(&conn, payload, known_subnet, &mut subnet_candidates) {
                continue;
            }
            known_server = Some(conn);
            known_subnet = Some(subnet_prefix(&conn));
            subnet_candidates.clear();
            reassembler.resync(seq);
            decoder.reset();
            if tx.send(ProtocolEvent::ServerChanged).is_err() {
                break;
            }
        }

        reassembler.push(seq, payload);
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

/// Game-server detection per §0.7: payload signature scan, then the
/// login-return fallback, then (once a server has ever been seen) the
/// subnet-tracking path for reconnects to the same datacenter.
fn detects_game_server(conn: &Conn, payload: &[u8], known_subnet: Option<[u8; 2]>, subnet_candidates: &mut HashSet<Conn>) -> bool {
    if looks_like_game_server(payload) || is_login_return(payload) {
        return true;
    }

    let Some(prefix) = known_subnet else {
        return false;
    };
    if subnet_prefix(conn) != prefix || subnet_candidates.contains(conn) {
        return false;
    }
    if subnet_candidates.len() >= MAX_SUBNET_CONNECTIONS {
        return false;
    }
    subnet_candidates.insert(*conn);
    true
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
