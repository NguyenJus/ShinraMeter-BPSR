//! Stable entity identity for the protocol boundary (issue #335).
//!
//! The wire's `uuid` packs an entity's id *and* its type/flags into one
//! integer (see [`crate::event::kind_of`] for the layout). Every consumer
//! above this crate wants the short, human-sized `uuid >> 16` for display —
//! it is what the game's own UI shows, what the name cache is keyed on, and
//! what every golden file in this repo prints. But that truncation throws
//! away the bits that tell two entities apart:
//!
//! * **uid recycling** — a server session hands the same `uuid >> 16` to a
//!   different entity later on, and every stat filed under it blends;
//! * **shadow/mirror entities** — a client-side or summoned copy shares the
//!   original's uid and differs only in the flag bits, so its damage lands
//!   on the original's row.
//!
//! [`EntityId`] therefore keeps the *whole* uuid as the identity, and
//! [`EntityId::display_uid`] derives the display number from it. Events
//! carry both: consumers key on the former and print the latter.
//!
//! [`EntityTable`] is the registry that makes that identity usable for the
//! two wire sources that carry a bare uid and no uuid at all
//! (`CharSerialize.char_id` and `TeamMemData.char_id` — see
//! `decode::on_sync_container_data` / `decode::on_notify_join_team`). Its
//! *shadow map* remembers which live `EntityId` currently wears each display
//! uid, so those sources resolve to the same identity the AOI channel is
//! already using rather than to a second, parallel one.
//!
//! Modelled on bpsr-logs' `Encounter::uid_to_monster_info` /
//! `entity_uid_to_entity` pair (`src-tauri/src/live/opcodes_process.rs`),
//! with the identity widened from their truncated uid to the full uuid.

use std::collections::HashMap;
use std::collections::VecDeque;

use crate::event::EntityKind;
pub use bpsr_meter::EntityId;

/// Upper bound on tracked entities. A long session streams through far more
/// entities than are ever live at once (every mob in every room the client
/// walks past), and this table is keyed on an identity that is deliberately
/// *never* reused — so without a cap it would grow for as long as the client
/// stays connected. Well above any plausible live-AOI population, so the
/// eviction below only ever reaches entities that have long since despawned.
const MAX_ENTITIES: usize = 4096;

/// What the table knows about one entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityRecord {
    pub kind: EntityKind,
    /// `uuid >> 16` — cached so callers rendering a row never have to
    /// re-derive it, and so a record answers "what number does this print
    /// as" without the caller knowing the bit layout.
    pub display_uid: i64,
    /// The `now_ms` of the packet this entity was first observed in. Not
    /// wall-clock: the same caller-supplied clock every event carries.
    pub first_seen_ms: u64,
}

/// Every entity this server session has named, keyed on the full uuid, plus
/// the shadow map from display uid to whichever entity currently wears it.
///
/// See the module doc for why both maps exist. Cleared on the same
/// boundaries the meter clears its own entity state on — a server change or
/// a scene entry (see `decode::Decoder::reset` and
/// `decode::on_enter_scene`) — since neither the uuids nor the uid
/// assignments survive one.
#[derive(Clone, Debug, Default)]
pub struct EntityTable {
    entries: HashMap<EntityId, EntityRecord>,
    /// Display uid -> the entity that most recently claimed it. A recycled
    /// uid overwrites this; the *previous* holder keeps its own `entries`
    /// row, so its stats never merge into the newcomer's.
    shadow: HashMap<i64, EntityId>,
    /// Insertion order, for the `MAX_ENTITIES` eviction.
    order: VecDeque<EntityId>,
}

impl EntityTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `uuid` exists (or is still around) and returns its
    /// identity.
    ///
    /// A uuid already in the table is returned untouched — `first_seen_ms`
    /// is the *first* sighting, and a re-observation must not restamp it.
    /// A uuid whose display uid is already claimed by a **different** entity
    /// takes the shadow slot over while leaving that entity's record intact:
    /// that is the recycling/mirror case this table exists for.
    pub fn observe(&mut self, uuid: i64, now_ms: u64) -> EntityId {
        let id = EntityId::from_uuid(uuid);
        let display_uid = id.display_uid();
        if let std::collections::hash_map::Entry::Vacant(slot) = self.entries.entry(id) {
            slot.insert(EntityRecord {
                kind: id.kind(),
                display_uid,
                first_seen_ms: now_ms,
            });
            self.order.push_back(id);
            self.evict_if_full();
        }
        self.shadow.insert(display_uid, id);
        id
    }

    pub fn get(&self, id: EntityId) -> Option<&EntityRecord> {
        self.entries.get(&id)
    }

    /// The entity currently wearing `display_uid`, if any. This is the
    /// shadow map: after a recycle it answers with the *new* holder, which
    /// is what a bare-uid packet arriving now is talking about.
    pub fn live_for_display_uid(&self, display_uid: i64) -> Option<EntityId> {
        self.shadow.get(&display_uid).copied()
    }

    /// The identity to file a bare-uid packet under (`CharSerialize.char_id`,
    /// `TeamMemData.char_id`): the live holder of that uid when the AOI
    /// channel has already named one, else the canonical reconstruction
    /// [`EntityId::from_display_uid`] produces.
    ///
    /// The shadow hit is only accepted when its kind matches — a party
    /// roster's `char_id` is a player, and resolving it onto a monster that
    /// happens to share the number would be worse than the fallback.
    pub fn resolve_uid(&self, display_uid: i64, kind: EntityKind) -> EntityId {
        match self.live_for_display_uid(display_uid) {
            Some(id) if id.kind() == kind => id,
            _ => EntityId::from_display_uid(display_uid, kind),
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.shadow.clear();
        self.order.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drops the oldest entities once the table is over `MAX_ENTITIES`,
    /// FIFO by first sighting — `order` is insertion order and is never
    /// reshuffled, so a re-observed entity does not get a fresher spot in
    /// the queue and is exactly as eviction-prone as the moment it first
    /// appeared. This is not LRU: repeatedly re-observing an entity does
    /// not keep it alive.
    /// Only the evicted entity's *own* shadow entry goes with it — if the
    /// uid has since been recycled onto a newer entity, that mapping is the
    /// live one and must survive its predecessor.
    fn evict_if_full(&mut self) {
        while self.entries.len() > MAX_ENTITIES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(rec) = self.entries.remove(&oldest)
                && self.shadow.get(&rec.display_uid) == Some(&oldest)
            {
                self.shadow.remove(&rec.display_uid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `EEntityType::EntChar`/`EntMonster` shifted into a uuid's type field —
    // see `bpsr_meter::event::kind_of` for the bit layout this mirrors.
    const PLAYER_KIND_BITS: i64 = 10 << 6;
    const MONSTER_KIND_BITS: i64 = 1 << 6;

    fn player_uuid(uid: i64) -> i64 {
        (uid << 16) | PLAYER_KIND_BITS
    }

    fn monster_uuid(uid: i64) -> i64 {
        (uid << 16) | MONSTER_KIND_BITS
    }

    #[test]
    fn entity_id_keeps_the_whole_uuid_and_derives_the_display_uid() {
        let id = EntityId::from_uuid(player_uuid(12345));
        assert_eq!(id.uuid(), player_uuid(12345));
        assert_eq!(id.display_uid(), 12345);
        assert_eq!(id.kind(), EntityKind::Player);
    }

    /// The whole point of the newtype: two entities the old `uuid >> 16`
    /// boundary could not tell apart are distinct identities here.
    #[test]
    fn a_summon_flag_makes_a_distinct_identity_with_the_same_display_uid() {
        let plain = EntityId::from_uuid(monster_uuid(999));
        let mirrored = EntityId::from_uuid(monster_uuid(999) | (1 << 15));
        assert_ne!(plain, mirrored);
        assert_eq!(plain.display_uid(), mirrored.display_uid());
        assert_eq!(mirrored.kind(), EntityKind::Monster);
    }

    #[test]
    fn from_display_uid_reconstructs_the_aoi_channels_own_id() {
        assert_eq!(
            EntityId::from_display_uid(77, EntityKind::Player),
            EntityId::from_uuid(player_uuid(77))
        );
        assert_eq!(
            EntityId::from_display_uid(77, EntityKind::Monster),
            EntityId::from_uuid(monster_uuid(77))
        );
    }

    #[test]
    fn observe_records_kind_display_uid_and_first_seen() {
        let mut table = EntityTable::new();
        let id = table.observe(monster_uuid(42), 1_000);
        let rec = table.get(id).expect("observed entity is in the table");
        assert_eq!(rec.kind, EntityKind::Monster);
        assert_eq!(rec.display_uid, 42);
        assert_eq!(rec.first_seen_ms, 1_000);
    }

    #[test]
    fn re_observing_an_entity_keeps_its_first_sighting() {
        let mut table = EntityTable::new();
        let id = table.observe(monster_uuid(42), 1_000);
        assert_eq!(table.observe(monster_uuid(42), 9_000), id);
        assert_eq!(table.get(id).unwrap().first_seen_ms, 1_000);
        assert_eq!(table.len(), 1);
    }

    /// Issue #335's recycling case: the server hands `uuid >> 16` == 42 to a
    /// second, different entity. Both keep their own record, so nothing
    /// filed under either can blend into the other.
    #[test]
    fn a_recycled_display_uid_gets_its_own_entry() {
        let mut table = EntityTable::new();
        let first = table.observe(monster_uuid(42), 1_000);
        let second = table.observe(monster_uuid(42) | (1 << 15), 2_000);
        assert_ne!(first, second);
        assert_eq!(table.len(), 2);
        assert_eq!(table.get(first).unwrap().first_seen_ms, 1_000);
        assert_eq!(table.get(second).unwrap().first_seen_ms, 2_000);
    }

    /// ...and the shadow map follows the newcomer, since a bare-uid packet
    /// arriving now is talking about whatever holds the uid now.
    #[test]
    fn the_shadow_map_follows_the_newest_holder_of_a_display_uid() {
        let mut table = EntityTable::new();
        table.observe(player_uuid(7), 1_000);
        let second = table.observe(player_uuid(7) | (1 << 14), 2_000);
        assert_eq!(table.live_for_display_uid(7), Some(second));
    }

    #[test]
    fn resolve_uid_prefers_the_live_holder_over_the_reconstruction() {
        let mut table = EntityTable::new();
        let live = table.observe(player_uuid(7) | (1 << 14), 1_000);
        assert_eq!(table.resolve_uid(7, EntityKind::Player), live);
    }

    #[test]
    fn resolve_uid_falls_back_when_the_uid_was_never_seen() {
        let table = EntityTable::new();
        assert_eq!(
            table.resolve_uid(7, EntityKind::Player),
            EntityId::from_uuid(player_uuid(7))
        );
    }

    /// A shadow hit of the wrong kind is refused: a roster `char_id` is a
    /// player, and a monster that happens to share the number is not it.
    #[test]
    fn resolve_uid_ignores_a_shadow_hit_of_a_different_kind() {
        let mut table = EntityTable::new();
        table.observe(monster_uuid(7), 1_000);
        assert_eq!(
            table.resolve_uid(7, EntityKind::Player),
            EntityId::from_uuid(player_uuid(7))
        );
    }

    #[test]
    fn clear_drops_both_maps() {
        let mut table = EntityTable::new();
        table.observe(monster_uuid(42), 0);
        table.clear();
        assert!(table.is_empty());
        assert_eq!(table.live_for_display_uid(42), None);
    }

    /// The table is keyed on an identity that is never reused, so it has to
    /// be bounded or a long session grows it without limit.
    #[test]
    fn the_table_is_bounded_and_evicts_the_oldest_entities() {
        let mut table = EntityTable::new();
        for uid in 0..(MAX_ENTITIES as i64 + 10) {
            table.observe(monster_uuid(uid), uid as u64);
        }
        assert_eq!(table.len(), MAX_ENTITIES);
        // The first ten are gone; the newest are still there.
        assert_eq!(table.live_for_display_uid(0), None);
        assert!(
            table
                .live_for_display_uid(MAX_ENTITIES as i64 + 9)
                .is_some()
        );
    }

    /// Eviction must not tear down a shadow mapping that has since moved on
    /// to a newer entity — that mapping belongs to the newcomer.
    #[test]
    fn eviction_leaves_a_recycled_uids_current_holder_reachable() {
        let mut table = EntityTable::new();
        let old = table.observe(monster_uuid(1), 0);
        let new = table.observe(monster_uuid(1) | (1 << 15), 1);
        // One over the cap in total, so exactly `old` — the oldest — goes.
        for uid in 2..(MAX_ENTITIES as i64 + 1) {
            table.observe(monster_uuid(uid), uid as u64);
        }
        assert_eq!(table.get(old), None);
        assert_eq!(table.live_for_display_uid(1), Some(new));
    }
}
