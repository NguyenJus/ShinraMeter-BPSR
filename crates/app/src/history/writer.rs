//! The history thread (issue #39): the single owner of the `HistoryStore`
//! connection, reached by the pipeline and the UI through [`HistoryHandle`]'s
//! request/reply channel.
//!
//! Unlike `pipeline::CacheWriter` — a bounded, capacity-1, drop-oldest
//! channel that deliberately coalesces bursts down to "persist only the
//! newest snapshot" — this channel is **unbounded** and does **no
//! coalescing**: each finished encounter is a one-shot write that must not
//! be dropped in favour of a newer one. Losing a stale name-cache save just
//! means slightly stale identity data on the next launch; losing an
//! encounter record means a fight the user can never get back.

use std::path::PathBuf;
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, unbounded};

use super::sqlite::SqliteHistory;
use super::{EncounterRecord, EncounterSummary, HistoryStore, RetentionPolicy};

/// What the history thread is asked to do (issue #39). Read requests carry
/// their own reply channel, so the thread never has to know the shape of
/// whatever channel the UI happens to be using.
pub enum HistoryRequest {
    Record(Box<EncounterRecord>),
    List {
        limit: u32,
        reply: Sender<HistoryEvent>,
    },
    Load {
        id: i64,
        reply: Sender<HistoryEvent>,
    },
    Delete {
        id: i64,
        reply: Sender<HistoryEvent>,
    },
    Clear {
        reply: Sender<HistoryEvent>,
    },
}

/// What the history thread sends back to whoever asked (issue #39).
#[derive(Debug, Clone)]
pub enum HistoryEvent {
    Listed(Vec<EncounterSummary>),
    /// One encounter's full detail, tagged with the id it was requested
    /// with: the record itself carries no row id, and a caller with more
    /// than one `Load` in flight has to be able to tell the replies apart.
    Loaded {
        id: i64,
        record: Box<EncounterRecord>,
    },
    /// The requested encounter is gone (deleted, or pruned since the list was
    /// taken) — the UI drops back to the list rather than showing nothing.
    Missing(i64),
    /// A delete/clear landed; the UI re-requests the list.
    Changed,
    /// The operation failed. Already logged by the history thread; carried
    /// here only so the view can say so instead of silently doing nothing.
    Failed(String),
}

/// The pipeline's and the UI's shared handle on the history thread (issue
/// #39). Cheap to clone — every clone shares one `Sender`, so any number of
/// callers can hold one without the thread caring how many.
#[derive(Clone)]
pub struct HistoryHandle {
    tx: Sender<HistoryRequest>,
}

impl HistoryHandle {
    /// Opens the store at `path` and spawns the thread that owns it. Returns
    /// `None` — with the failure logged — if the store cannot be opened, so
    /// a broken or unwritable history file costs the user history and
    /// nothing else (issue #39; never panic, never stall the overlay).
    ///
    /// This is also the integration-test seam: `tests/replay_history.rs`
    /// cannot see `#[cfg(test)]` items of this library (`SqliteHistory::
    /// in_memory` stays test-only for WP1's unit tests), so rather than
    /// growing a Cargo feature for one test binary, that test simply calls
    /// this with a temp-file path.
    pub fn spawn(path: PathBuf, policy: RetentionPolicy) -> Option<(Self, JoinHandle<()>)> {
        let store = match SqliteHistory::open(&path, policy) {
            Ok(store) => store,
            Err(err) => {
                log::warn!("history: failed to open {}: {err}", path.display());
                return None;
            }
        };
        let (tx, rx) = unbounded();
        let handle = std::thread::Builder::new()
            .name("history".to_string())
            .spawn(move || run(store, rx))
            .expect("failed to spawn the history thread");
        Some((Self { tx }, handle))
    }

    /// Enqueues a finished encounter. Never blocks: the channel is
    /// unbounded, and a dead receiver (the thread has already exited) is
    /// silently ignored — there is no reply channel to carry that failure
    /// to, and nothing the caller could do about it anyway.
    pub fn record(&self, record: EncounterRecord) {
        let _ = self.tx.send(HistoryRequest::Record(Box::new(record)));
    }

    /// Requests the newest `limit` encounters; the reply lands on `reply`.
    pub fn list(&self, limit: u32, reply: &Sender<HistoryEvent>) {
        let _ = self.tx.send(HistoryRequest::List {
            limit,
            reply: reply.clone(),
        });
    }

    /// Requests one encounter's full detail (players included); the reply
    /// lands on `reply`.
    pub fn load(&self, id: i64, reply: &Sender<HistoryEvent>) {
        let _ = self.tx.send(HistoryRequest::Load {
            id,
            reply: reply.clone(),
        });
    }

    /// Requests that one encounter be deleted; the reply lands on `reply`.
    pub fn delete(&self, id: i64, reply: &Sender<HistoryEvent>) {
        let _ = self.tx.send(HistoryRequest::Delete {
            id,
            reply: reply.clone(),
        });
    }

    /// Requests that every encounter be deleted; the reply lands on `reply`.
    pub fn clear(&self, reply: &Sender<HistoryEvent>) {
        let _ = self.tx.send(HistoryRequest::Clear {
            reply: reply.clone(),
        });
    }
}

/// The thread body: blocks on `rx`, dispatching each request against
/// `store`, until every `HistoryHandle` clone (and so every `Sender`) is
/// dropped and `recv` finally errs. Every `HistoryError` is logged here —
/// `Record` has no reply channel to carry one to, so it is logged and
/// dropped; every read/write request replies `HistoryEvent::Failed` on top
/// of the log line, so the view can say so instead of silently doing
/// nothing.
fn run(mut store: SqliteHistory, rx: Receiver<HistoryRequest>) {
    while let Ok(req) = rx.recv() {
        match req {
            HistoryRequest::Record(record) => {
                if let Err(err) = store.insert(&record) {
                    log::warn!("history: failed to record an encounter: {err}");
                }
            }
            HistoryRequest::List { limit, reply } => match store.list(limit) {
                Ok(rows) => {
                    let _ = reply.send(HistoryEvent::Listed(rows));
                }
                Err(err) => {
                    log::warn!("history: failed to list encounters: {err}");
                    let _ = reply.send(HistoryEvent::Failed(err.to_string()));
                }
            },
            HistoryRequest::Load { id, reply } => match store.load(id) {
                Ok(Some(record)) => {
                    let _ = reply.send(HistoryEvent::Loaded {
                        id,
                        record: Box::new(record),
                    });
                }
                Ok(None) => {
                    let _ = reply.send(HistoryEvent::Missing(id));
                }
                Err(err) => {
                    log::warn!("history: failed to load encounter {id}: {err}");
                    let _ = reply.send(HistoryEvent::Failed(err.to_string()));
                }
            },
            HistoryRequest::Delete { id, reply } => match store.delete(id) {
                Ok(()) => {
                    let _ = reply.send(HistoryEvent::Changed);
                }
                Err(err) => {
                    log::warn!("history: failed to delete encounter {id}: {err}");
                    let _ = reply.send(HistoryEvent::Failed(err.to_string()));
                }
            },
            HistoryRequest::Clear { reply } => match store.clear() {
                Ok(()) => {
                    let _ = reply.send(HistoryEvent::Changed);
                }
                Err(err) => {
                    log::warn!("history: failed to clear history: {err}");
                    let _ = reply.send(HistoryEvent::Failed(err.to_string()));
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crossbeam_channel::unbounded;

    use super::*;
    use crate::history::{record_from_snapshot, temp_history_path};
    use bpsr_meter::{EncounterInfo, PlayerRow, Snapshot};

    fn sample_record(title: &str) -> EncounterRecord {
        let snapshot = Snapshot {
            duration_ms: 10_000,
            total_damage: 5_000,
            total_dps: 500.0,
            rows: vec![PlayerRow {
                uid: 1,
                name: "Alice".to_string(),
                class: None,
                ability_score: None,
                season_strength: None,
                imagines: [None, None],
                imagine_tiers: [None, None],
                damage: 5_000,
                dps: 500.0,
                share_pct: 100.0,
                crit_pct: 0.0,
                lucky_pct: 0.0,
                hits: 10,
                absorbed_total: 0,
                immune_total: 0,
                shield: None,
                deaths: 0,
                dead_ms: Some(0),
                skills: Vec::new(),
                heals: Vec::new(),
                dealt: Vec::new(),
                received: Vec::new(),
                casts: Vec::new(),
                buffs: Vec::new(),
            }],
            encounter: EncounterInfo::default(),
            capture_alive: true,
            total_absorbed: 0,
            total_immune: 0,
        };
        record_from_snapshot(&snapshot, 1_000, title.to_string(), None).unwrap()
    }

    #[test]
    fn a_recorded_encounter_can_be_listed_back() {
        let path = temp_history_path("record-list");
        let (handle, thread) =
            HistoryHandle::spawn(path.clone(), RetentionPolicy::default()).unwrap();
        handle.record(sample_record("Recorded Fight"));

        let (reply_tx, reply_rx) = unbounded();
        // The record above and this list both go through the same
        // single-threaded, FIFO channel, so by the time `List` is processed
        // the `Record` ahead of it has already landed.
        handle.list(10, &reply_tx);
        let event = reply_rx.recv().unwrap();

        drop(handle);
        let _ = thread.join();
        let _ = std::fs::remove_file(&path);

        match event {
            HistoryEvent::Listed(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Listed, got {other:?}"),
        }
    }

    #[test]
    fn a_load_reply_carries_the_player_rows() {
        let path = temp_history_path("load");
        let (handle, thread) =
            HistoryHandle::spawn(path.clone(), RetentionPolicy::default()).unwrap();
        handle.record(sample_record("Loadable Fight"));

        let (list_tx, list_rx) = unbounded();
        handle.list(10, &list_tx);
        let id = match list_rx.recv().unwrap() {
            HistoryEvent::Listed(rows) => rows[0].id,
            other => panic!("expected Listed, got {other:?}"),
        };

        let (load_tx, load_rx) = unbounded();
        handle.load(id, &load_tx);
        let event = load_rx.recv().unwrap();

        drop(handle);
        let _ = thread.join();
        let _ = std::fs::remove_file(&path);

        match event {
            HistoryEvent::Loaded {
                id: replied_id,
                record,
            } => {
                assert_eq!(replied_id, id);
                assert_eq!(record.players[0].name, "Alice");
            }
            other => panic!("expected Loaded, got {other:?}"),
        }
    }

    #[test]
    fn loading_a_missing_id_replies_missing() {
        let path = temp_history_path("load-missing");
        let (handle, thread) =
            HistoryHandle::spawn(path.clone(), RetentionPolicy::default()).unwrap();

        let (reply_tx, reply_rx) = unbounded();
        handle.load(999, &reply_tx);
        let event = reply_rx.recv().unwrap();

        drop(handle);
        let _ = thread.join();
        let _ = std::fs::remove_file(&path);

        assert!(matches!(event, HistoryEvent::Missing(999)));
    }

    #[test]
    fn delete_replies_changed_and_drops_the_row() {
        let path = temp_history_path("delete");
        let (handle, thread) =
            HistoryHandle::spawn(path.clone(), RetentionPolicy::default()).unwrap();
        handle.record(sample_record("Deletable Fight"));

        let (list_tx, list_rx) = unbounded();
        handle.list(10, &list_tx);
        let id = match list_rx.recv().unwrap() {
            HistoryEvent::Listed(rows) => rows[0].id,
            other => panic!("expected Listed, got {other:?}"),
        };

        let (delete_tx, delete_rx) = unbounded();
        handle.delete(id, &delete_tx);
        let delete_event = delete_rx.recv().unwrap();

        let (list_tx2, list_rx2) = unbounded();
        handle.list(10, &list_tx2);
        let after = list_rx2.recv().unwrap();

        drop(handle);
        let _ = thread.join();
        let _ = std::fs::remove_file(&path);

        assert!(matches!(delete_event, HistoryEvent::Changed));
        assert!(matches!(after, HistoryEvent::Listed(rows) if rows.is_empty()));
    }

    #[test]
    fn clear_replies_changed_and_empties_the_list() {
        let path = temp_history_path("clear");
        let (handle, thread) =
            HistoryHandle::spawn(path.clone(), RetentionPolicy::default()).unwrap();
        handle.record(sample_record("Fight One"));
        handle.record(sample_record("Fight Two"));

        let (clear_tx, clear_rx) = unbounded();
        handle.clear(&clear_tx);
        let clear_event = clear_rx.recv().unwrap();

        let (list_tx, list_rx) = unbounded();
        handle.list(10, &list_tx);
        let after = list_rx.recv().unwrap();

        drop(handle);
        let _ = thread.join();
        let _ = std::fs::remove_file(&path);

        assert!(matches!(clear_event, HistoryEvent::Changed));
        assert!(matches!(after, HistoryEvent::Listed(rows) if rows.is_empty()));
    }

    #[test]
    fn spawn_returns_none_when_the_store_cannot_be_opened() {
        let file = temp_history_path("not-a-directory");
        std::fs::write(&file, b"not a directory").expect("write the blocking file");
        let bogus_path = file.join("history.sqlite");

        let result = HistoryHandle::spawn(bogus_path, RetentionPolicy::default());

        let _ = std::fs::remove_file(&file);
        assert!(result.is_none());
    }
}
