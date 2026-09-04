//! Windows packet capture: WinDivert sniff mode → etherparse → TCP
//! reassembly → protocol decode → `ProtocolEvent` channel.
//!
//! Kept deliberately thin: every branch worth unit-testing (reassembly,
//! server-signature detection, connection-direction classification) lives in
//! [`crate::tcp`] / [`crate::detect`], which are host-tested. This module
//! only wires WinDivert + etherparse to that pure logic and cannot itself be
//! tested off Windows.
//!
//! The driver is driven through the raw FFI declared in [`crate::driver`]
//! rather than a safe wrapper crate: the wrapper keeps its OS `HANDLE`
//! private and takes `&mut self` in `shutdown`, which cannot be called
//! soundly while the capture thread is parked in `recv` on the same object.
//! Owning the `Copy` `HANDLE` ourselves lets [`CaptureHandle::stop`] unblock
//! `recv` with a plain syscall and no Rust references at all. Hand-declaring
//! the four entry points is also what allows the library to be loaded at
//! runtime from the unpacked copy — see [`crate::driver`].

use std::ffi::{CString, c_void};
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bpsr_protocol::{Decoder, InspectSink, ProtocolEvent};
use crossbeam_channel::Sender;
use etherparse::{NetSlice, SlicedPacket, TransportSlice};
use windows::Win32::Foundation::HANDLE;

use crate::backoff::recv_error_backoff;
use crate::detect::{Conn, ServerDetector, decide_packet};
use crate::driver::{Api, WinDivertAddress};
use crate::error::CaptureError;
use crate::owner::{self, SystemOwnerLookup};
use crate::restart::CaptureRestart;
use crate::tcp::TcpReassembler;
use crate::throughput::{
    self, Heartbeat, HeartbeatKind, PacketRecord, SharedMonitor, Tick, WATCHDOG_TICK, run_watchdog,
};

/// WinDivert filter: every non-loopback TCP/IP packet, in either direction.
const FILTER: &str = "!loopback && ip && tcp";

/// Recv buffer, reused across calls to `WinDivertRecv`.
const RECV_BUFFER_SIZE: usize = 10 * 1024 * 1024;

/// Backoff applied after the first failed `WinDivertRecv`, multiplied by the
/// consecutive-failure count and clamped to [`MAX_RECV_ERROR_BACKOFF`]. A
/// transient failure costs one short nap; a handle stuck in a permanently
/// failing state (adapter removed, driver unloaded or upgraded mid-session)
/// no longer spins the thread at 100% CPU.
const RECV_ERROR_BACKOFF: Duration = Duration::from_millis(20);
const MAX_RECV_ERROR_BACKOFF: Duration = Duration::from_millis(500);

/// Issue #337: how often the capture loop retries `owner::find_game_pid`
/// while it has not yet found the game's pid (e.g. capture started before
/// the game process did). Once found, the pid is cached for the rest of the
/// loop's life — the game is not expected to restart mid-session — so this
/// only bounds the cost of the "not running yet" window, not the steady
/// state.
const GAME_PID_LOOKUP_INTERVAL: Duration = Duration::from_secs(2);

/// Consecutive `WinDivertRecv` failures tolerated before the capture thread
/// concludes the handle is dead and exits. Exiting drops `tx`, which closes
/// the event channel and is how the failure reaches the rest of the app —
/// far better than a live thread that silently never emits again.
const MAX_CONSECUTIVE_RECV_ERRORS: u32 = 64;

/// Handle to the running capture thread.
pub struct CaptureHandle {
    stop: Arc<AtomicBool>,
    /// The restart request the capture thread reads once per packet. Held
    /// as the shared [`CaptureRestart`] rather than a bare flag so the app
    /// can hand a clone to its command loop — the handle itself cannot
    /// travel between threads (see [`Self::restart_requester`]).
    restart: CaptureRestart,
    /// The driver handle, owned as the `Copy` OS value the C API takes. Both
    /// the capture thread (`WinDivertRecv`) and [`Self::stop`]
    /// (`WinDivertShutdown`) pass it to the driver by value; no Rust
    /// reference to a shared object is ever formed, so there is no aliasing
    /// question to answer.
    handle: HANDLE,
    /// The loaded WinDivert runtime. Borrowed from the process-lifetime
    /// static, so shutdown never has to re-enter the load path.
    api: &'static Api,
    /// `None` once `stop()` (or `Drop`) has taken it to join the thread.
    /// `Option` rather than a bare `JoinHandle` because [`Self::stop`] needs
    /// to move it out through `&mut self` — a value of a type that
    /// implements `Drop` cannot be partially moved out of by value.
    join: Option<JoinHandle<()>>,
    /// The heartbeat watchdog (issue #271). Held separately because it is
    /// deliberately *not* the packet thread: it wakes on a wall clock, so
    /// the diagnostics keep running when no packet the capture loop cares
    /// about — or no packet at all — is arriving.
    heartbeat_join: Option<JoinHandle<()>>,
    /// Set once [`Self::shutdown_and_close`] has run, so a second call
    /// (`stop()` followed by the `Drop` that runs when it returns) is a
    /// no-op instead of re-running `WinDivertShutdown`/`WinDivertClose` on an
    /// already-closed handle.
    closed: bool,
}

impl CaptureHandle {
    /// Forces the capture loop to forget the currently-known server
    /// connection and re-run detection from scratch on the next packet.
    pub fn request_restart(&self) {
        self.restart.request();
    }

    /// A `Send`-able clone of the restart request, for callers that cannot
    /// hold the handle itself (issue #214).
    ///
    /// `CaptureHandle` owns a raw Windows `HANDLE`, so it is neither `Send`
    /// nor `Sync` and has to stay on the thread that opened it — which is
    /// precisely why [`Self::request_restart`] shipped with no caller
    /// anywhere in the app. The pipeline thread, which is where the UI's
    /// command channel is drained, can hold one of these instead.
    pub fn restart_requester(&self) -> CaptureRestart {
        self.restart.clone()
    }

    /// Signals the capture thread to stop and waits for it to exit.
    ///
    /// `WinDivertRecv` blocks waiting for the next packet, which on a quiet
    /// link (typically: the game already exited) may never return — so
    /// setting the stop flag alone is not enough. `WinDivertShutdown` is the
    /// driver-documented way to unblock a thread parked in a recv on the
    /// same handle from another thread.
    ///
    /// Does the same work `Drop` would; the `closed` guard in
    /// `shutdown_and_close` makes the `Drop` that runs when `self` falls out
    /// of scope here a no-op, so the driver handle is still closed exactly
    /// once and nothing in `self` (the `stop`/`restart` `Arc`s included) is
    /// leaked.
    pub fn stop(mut self) {
        self.shutdown_and_close();
    }

    /// Shared teardown: signal, unblock `recv`, join the thread, close the
    /// handle. Idempotent — guarded by `closed` — so it is safe to call from
    /// both `stop` and the `Drop` that follows it.
    fn shutdown_and_close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.stop.store(true, Ordering::SeqCst);
        // SAFETY: `self.handle` is a live WinDivert handle (checked at open,
        // closed only below after the capture thread has joined).
        // `WinDivertShutdown` is documented as callable concurrently with a
        // blocked recv on the same handle, and takes the handle by value.
        unsafe {
            self.api.shutdown_recv(self.handle);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        // The watchdog never touches the driver handle, but it is joined
        // here anyway so the thread is gone before the process moves on —
        // it notices the stop flag within one `WATCHDOG_TICK`.
        if let Some(join) = self.heartbeat_join.take() {
            let _ = join.join();
        }
        // SAFETY: the capture thread has exited (or was never spawned with
        // this handle outstanding), so nothing can use the handle after
        // this point.
        unsafe {
            self.api.close(self.handle);
        }
    }
}

impl Drop for CaptureHandle {
    /// Runs `WinDivertShutdown`/join/`WinDivertClose` if the handle is
    /// dropped without an explicit `stop()` — e.g. an unwind out of
    /// `eframe::run_native` — so the driver handle and capture thread are
    /// never leaked until process exit. `stop()` performs this same teardown
    /// itself; the `closed` guard in `shutdown_and_close` makes this a no-op
    /// when it then runs on the same handle.
    fn drop(&mut self) {
        self.shutdown_and_close();
    }
}

/// Opens a WinDivert sniff-mode handle on all non-loopback TCP/IP traffic
/// and spawns a thread that reassembles the detected game-server's TCP
/// stream and emits decoded [`ProtocolEvent`]s on `tx`. `inspect_sink`
/// (issue #25 slice A, opt-in since issue #122) is diagnostic
/// observation, wired straight into the thread's `Decoder`; `None`
/// reproduces the pre-#25 decoder exactly.
pub fn start_capture(
    tx: Sender<ProtocolEvent>,
    inspect_sink: Option<Arc<dyn InspectSink>>,
) -> Result<CaptureHandle, CaptureError> {
    let filter = CString::new(FILTER).expect("FILTER is a literal without interior NULs");
    let api = crate::driver::api()?;
    let handle = api.open_sniff(&filter)?;

    let stop = Arc::new(AtomicBool::new(false));
    let restart = CaptureRestart::new();
    let thread_stop = Arc::clone(&stop);
    let thread_restart = restart.clone();
    // `HANDLE` is a `Copy` newtype over a raw `*mut c_void`, which makes it
    // neither `Send` nor `Sync` — so the closure below cannot capture one
    // directly, even though a WinDivert handle has no thread affinity and the
    // C API takes it by value from whichever thread calls in. Cross the
    // boundary as a plain integer and rebuild the handle on the far side: the
    // capture thread still gets its own copy of the value rather than a
    // reference into shared state, and no crate-wide `unsafe impl Send for
    // HANDLE`-style escape hatch — which would silently cover every other
    // handle type too — has to exist for it.
    let thread_handle = handle.0 as usize;
    // Issue #213: what capture is actually delivering, so "the game sent
    // nothing" and "reassembly ate the stream" stop looking identical in a
    // log. Shared rather than owned by the packet loop because of #271 —
    // see `heartbeat_loop`.
    let monitor = SharedMonitor::new(Instant::now());
    let loop_monitor = monitor.clone();
    let join = thread::spawn(move || {
        recv_loop(
            api,
            HANDLE(thread_handle as *mut c_void),
            tx,
            thread_stop,
            thread_restart,
            inspect_sink,
            loop_monitor,
        )
    });

    let heartbeat_stop = Arc::clone(&stop);
    let heartbeat_restart = restart.clone();
    let heartbeat_join = thread::spawn(move || {
        heartbeat_loop(&monitor, &heartbeat_stop, &heartbeat_restart);
    });

    Ok(CaptureHandle {
        stop,
        restart,
        handle,
        api,
        join: Some(join),
        heartbeat_join: Some(heartbeat_join),
        closed: false,
    })
}

/// Issue #271: the heartbeat's own thread.
///
/// `ThroughputMonitor::poll` used to be called from the bottom of
/// [`recv_loop`], below six `continue`s — two of which skip every packet
/// that is not a server→client segment of the adopted flow. The result was
/// that the heartbeat, and #214's self-restart with it, fell silent in
/// precisely the two states they exist to name: the game closed (nothing is
/// ever adopted again, so nothing reaches the bottom of the loop) and the
/// capture handle wedged (nothing arrives at all, so `recv` never returns).
/// Both produced total log silence, indistinguishable from a healthy idle
/// session.
///
/// Deciding on a wall clock in a thread of its own removes the question
/// entirely: no `continue` can skip it and no packet has to arrive for it
/// to run. The packet loop only records; the recovery action is routed back
/// through the same [`CaptureRestart`] the UI uses, so there is exactly one
/// re-anchoring code path.
fn heartbeat_loop(monitor: &SharedMonitor, stop: &AtomicBool, restart: &CaptureRestart) {
    run_watchdog(monitor, stop, WATCHDOG_TICK, |tick: Tick| {
        if let Some(beat) = tick.beat {
            log_heartbeat(&beat);
        }
        // Issue #214: the recovery #211 had no path to. Packets are still
        // arriving on the adopted connection but nothing has reached the
        // decoder for minutes, which no amount of further sniffing fixes —
        // only re-anchoring does.
        if tick.restart {
            // The connection is named from the tick rather than from a local:
            // this thread has no `known_server`, and a session with several
            // zone changes needs the log to say *which* flow stalled.
            log::error!(
                "capture: nothing has reached the decoder in {:?} while packets kept arriving on \
                 the tracked connection {}; re-running server detection and reassembly from \
                 scratch (issue #214)",
                throughput::STALL_RESTART_AFTER,
                describe(tick.tracked.as_ref()),
            );
            restart.request();
        }
    });
}

/// Blocking single-packet receive. On success returns how many bytes of
/// `buffer` the driver filled with the captured packet (IP header onwards);
/// on failure returns the OS error, which the caller must inspect — a
/// shutdown-initiated wakeup and a dead adapter both surface here and are
/// only distinguishable by the error (and by the stop flag).
///
/// Returns a length rather than a borrow of `buffer` so the caller can react
/// to an error without holding a borrow across the retry.
fn recv_packet(api: &Api, handle: HANDLE, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
    let mut addr = MaybeUninit::<WinDivertAddress>::uninit();
    // SAFETY: `handle` is live for the whole capture thread; `buffer` is a
    // valid writable slice of `buffer.len()` bytes and `addr` a valid
    // out-parameter of exactly the size the driver writes. `addr` is never
    // read back, so it does not matter that the driver leaves it untouched
    // on failure.
    unsafe { api.recv(handle, buffer, addr.as_mut_ptr()) }
}

/// Sets the shared stop flag when the packet loop leaves `recv_loop` by any
/// route at all.
///
/// Only [`CaptureHandle::shutdown_and_close`] used to set it, so a
/// `recv_loop` that gave up on its own — a handle dead for
/// [`MAX_CONSECUTIVE_RECV_ERRORS`] receives in a row, or an event channel
/// whose pipeline thread is gone — left [`heartbeat_loop`]'s watchdog awake
/// for the rest of the session, waking every [`WATCHDOG_TICK`] to warn about
/// a capture subsystem that is permanently dead. A `Drop` guard rather than a
/// store at each `break`/`return` so a future exit path cannot forget to do
/// it.
struct StopOnExit(Arc<AtomicBool>);

impl StopOnExit {
    fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl Drop for StopOnExit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn recv_loop(
    api: &Api,
    handle: HANDLE,
    tx: Sender<ProtocolEvent>,
    stop: Arc<AtomicBool>,
    restart: CaptureRestart,
    inspect_sink: Option<Arc<dyn InspectSink>>,
    monitor: SharedMonitor,
) {
    let stop = StopOnExit(stop);
    let mut buffer = vec![0u8; RECV_BUFFER_SIZE];
    let mut known_server: Option<Conn> = None;
    let mut detector = ServerDetector::new();
    let mut reassembler = TcpReassembler::new();
    let mut decoder = match inspect_sink {
        Some(sink) => Decoder::with_inspect_sink(sink),
        None => Decoder::new(),
    };
    let mut consecutive_errors: u32 = 0;
    // Finding 2 (pipeline-robustness audit): a stalled pipeline must not
    // block this thread behind a full channel and back the kernel WinDivert
    // queue up with it. See `crate::backpressure`.
    let mut drop_counter = crate::backpressure::DropCounter::new();
    // Issue #337: process-ownership filter for candidate streams. `game_pid`
    // starts unknown and is (re)resolved at most every
    // `GAME_PID_LOOKUP_INTERVAL` until found; `owner_allows_adoption` (in
    // `crate::detect`) fails open on `None`, so capture behaves exactly as
    // it did before #337 until the game's pid is located.
    let owner_lookup = SystemOwnerLookup::new();
    let mut game_pid: Option<u32> = None;
    let mut last_game_pid_lookup: Option<Instant> = None;

    log::info!("capture: WinDivert sniff loop started on filter {FILTER:?}");

    while !stop.is_set() {
        if restart.take_requested() {
            log::info!(
                "capture: restart requested; dropping the tracked connection {} and re-running server detection",
                describe(known_server.as_ref()),
            );
            known_server = None;
            detector.reset();
            decoder.reset();
            // The reassembler has to go too, or the restart re-adopts a
            // connection while `next_seq` still points into the wedged
            // stream's sequence space — which is the state the restart
            // exists to escape (#211, #214).
            reassembler = TcpReassembler::new();
            // The stall evidence goes with it: the packets that funded the
            // verdict belonged to a connection that is no longer tracked,
            // so they must not fund a second restart a tick later (#271).
            monitor.note_detached();
            monitor.record_gap_cache(0, 0);
        }

        let packet_len = match recv_packet(api, handle, &mut buffer) {
            Ok(len) => {
                consecutive_errors = 0;
                len
            }
            Err(err) => {
                // `CaptureHandle::stop` sets the flag *before* calling
                // `WinDivertShutdown`, so an expected shutdown wakeup always
                // finds it set: this is the normal exit, not a failure.
                if stop.is_set() {
                    break;
                }
                consecutive_errors += 1;
                log::warn!(
                    "WinDivertRecv failed ({consecutive_errors}/{MAX_CONSECUTIVE_RECV_ERRORS} consecutive): {err}"
                );
                if consecutive_errors >= MAX_CONSECUTIVE_RECV_ERRORS {
                    log::error!(
                        "WinDivertRecv failed {consecutive_errors} times in a row; \
                         giving up on the capture handle (last error: {err})"
                    );
                    break;
                }
                thread::sleep(recv_error_backoff(
                    consecutive_errors,
                    RECV_ERROR_BACKOFF,
                    MAX_RECV_ERROR_BACKOFF,
                ));
                continue;
            }
        };
        let packet = &buffer[..packet_len];
        // Counted here, above every `continue` below, so a heartbeat can
        // say whether the driver was delivering anything at all — which is
        // what separates "the game is closed" from "the capture handle has
        // stopped working" (#271).
        monitor.record_observed();
        let now = Instant::now();

        // Issue #337: resolve the game's own pid, at most once every
        // `GAME_PID_LOOKUP_INTERVAL`, until found — see the field doc above.
        if game_pid.is_none()
            && last_game_pid_lookup
                .is_none_or(|at| now.duration_since(at) >= GAME_PID_LOOKUP_INTERVAL)
        {
            last_game_pid_lookup = Some(now);
            game_pid = owner::find_game_pid();
        }

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

        // Role classification, teardown detection, and the adopt/emit
        // decision all live in `decide_packet` (crate::detect) so that
        // sequence — the exact class of bug 6363e90 fixed — is
        // host-tested; this loop only acts on what it returns.
        let decision = decide_packet(
            &mut detector,
            &mut known_server,
            &conn,
            payload,
            tcp.fin(),
            tcp.rst(),
            &crate::detect::OwnershipFilter {
                lookup: &owner_lookup,
                game_pid,
            },
        );
        // A FIN or RST on either direction of the tracked flow means it is
        // tearing down naturally. `decide_packet` already cleared
        // `known_server` (rather than only on an explicit `request_restart`),
        // which lets the subnet-reconnect fallback re-arm on its own for the
        // connection that replaces it — otherwise only a user-initiated
        // restart ever re-enables it.
        if decision.torn_down {
            log::info!(
                "capture: tracked connection {conn} torn down (fin={} rst={}); re-arming server detection",
                tcp.fin(),
                tcp.rst(),
            );
            monitor.note_detached();
        }
        if decision.skip {
            // Either the client→server half of the adopted connection
            // (recognized, so detection/adoption does not ping-pong on it,
            // but its bytes belong to that direction's own sequence space
            // and must never reach the server-stream reassembler), or an
            // unrelated connection that detection declined to adopt.
            continue;
        }
        if decision.newly_adopted {
            // issue #293: `decision.frame_offset` is the byte offset of the
            // actual frame boundary `decide_packet` located within this
            // payload (0 when the adopting evidence carried no such
            // boundary — the login-return and subnet-reconnect paths).
            // Resyncing to the packet's bare `seq` instead would start the
            // decoder at whatever byte the game server happened to send
            // first after adoption, which is only a frame boundary when
            // capture observed the connection from its very start — not
            // true for a mid-connection attach (issue #282).
            let resync_seq = seq.wrapping_add(decision.frame_offset as u32);
            log::info!(
                "capture: adopted game-server connection {conn} at seq={seq} \
                 frame_offset={} ({} payload bytes)",
                decision.frame_offset,
                payload.len(),
            );
            reassembler.resync(resync_seq);
            decoder.reset();
            monitor.note_adopted(conn);
            // Issue #337: only a genuine server-endpoint change gets a
            // `ServerChanged` — a reconnect (or a secondary stream) to the
            // same server still resyncs/resets above, but must not wipe the
            // in-progress meter.
            if decision.emit_server_changed {
                if tx.send(ProtocolEvent::ServerChanged).is_err() {
                    break;
                }
            } else {
                log::info!(
                    "capture: reconnected to the same server endpoint on {conn}; suppressing \
                     ServerChanged (issue #337)"
                );
            }
        }

        let payload_packet = !payload.is_empty();
        reassembler.push(seq, payload);
        if reassembler.take_loss() {
            log::info!(
                "capture: reassembly reported a break in the byte stream; resetting the decoder"
            );
            decoder.reset();
        }
        let stream = reassembler.take_stream();

        // One lock for the whole packet's accounting rather than one per
        // counter. The gap cache is published here rather than read back at
        // heartbeat time: the watchdog thread decides when to log and cannot
        // reach the reassembler.
        monitor.record(
            PacketRecord {
                payload_packet,
                delivered: stream.len(),
                gap_segments: reassembler.gap_segments(),
                gap_bytes: reassembler.gap_bytes(),
            },
            now,
        );

        if stream.is_empty() {
            continue;
        }

        for event in decoder.push_stream(&stream, now_ms()) {
            use crate::backpressure::SendOutcome;
            match drop_counter.try_send(&tx, event, Instant::now()) {
                SendOutcome::Sent => {}
                SendOutcome::Dropped(Some(total)) => log::warn!(
                    "capture: the protocol-event channel is full; dropped {total} event(s) in \
                     the last ~{:?} (the pipeline is not keeping up)",
                    crate::backpressure::LOG_INTERVAL,
                ),
                SendOutcome::Dropped(None) => {}
                SendOutcome::Disconnected => {
                    log::error!(
                        "capture: the protocol-event channel is closed (the pipeline thread is \
                         gone); the capture loop is exiting"
                    );
                    return;
                }
            }
        }
    }

    log::info!("capture: WinDivert sniff loop exited");
}

/// The tracked connection for a log line, or a placeholder when detection
/// has not adopted one.
fn describe(known_server: Option<&Conn>) -> String {
    match known_server {
        Some(conn) => conn.to_string(),
        None => "<none adopted>".to_string(),
    }
}

/// Issue #213's throughput heartbeat, in the four flavours #271 asked it to
/// tell apart. One line a minute is the entire budget for a log a user may
/// have to hand over, so the level carries the diagnosis: `warn` for the two
/// states that need looking at (a wedged stream, a handle that has stopped
/// delivering) and `info` for the two that do not (bytes are flowing; the
/// game simply is not running).
fn log_heartbeat(beat: &Heartbeat) {
    match beat.kind() {
        HeartbeatKind::Delivering => log::info!(
            "capture: {} byte(s) delivered to the decoder in {:.1?} from {} payload packet(s) \
             ({} packet(s) seen on the link); gap cache {} segment(s) / {} byte(s)",
            beat.bytes,
            beat.window,
            beat.packets,
            beat.observed,
            beat.gap_segments,
            beat.gap_bytes,
        ),
        HeartbeatKind::Wedged => log::warn!(
            "capture: 0 bytes delivered to the decoder in {:.1?} while {} payload packet(s) arrived \
             on the tracked connection (silent for {:.1?}); reassembly is holding {} segment(s) / \
             {} byte(s) behind a gap",
            beat.window,
            beat.packets,
            beat.silent_for,
            beat.gap_segments,
            beat.gap_bytes,
        ),
        // Not a fault: the capture handle is demonstrably alive, the game
        // is not sending. Said out loud anyway, because before #271 this
        // was the silence that looked identical to a dead capture.
        HeartbeatKind::NoGameTraffic => log::info!(
            "capture: no game traffic in {:.1?} — {} packet(s) crossed the link, none of them on \
             {} (silent for {:.1?}); capture is alive and waiting",
            beat.window,
            beat.observed,
            if beat.adopted {
                "the tracked connection"
            } else {
                "any adopted connection (none is adopted)"
            },
            beat.silent_for,
        ),
        // A filter this wide (`!loopback && ip && tcp`) going a full minute
        // without a single packet is either a machine with no network
        // activity whatsoever or a handle that has stopped delivering. The
        // second is invisible from the app's UI, so it is worth the `warn`.
        HeartbeatKind::LinkSilent => log::warn!(
            "capture: WinDivert delivered no packets at all in {:.1?} on filter {FILTER:?} \
             (silent for {:.1?}); either this machine has no network traffic or the capture \
             handle has stopped delivering",
            beat.window,
            beat.silent_for,
        ),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
