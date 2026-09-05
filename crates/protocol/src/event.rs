//! Typed protocol events — the crate's cross-crate contract (plan §T1.3).
//!
//! `ProtocolEvent` is consumed as-is by `bpsr-meter` and the app; freeze the
//! shape here before Phase 2 starts.

use crate::entity::EntityId;
use crate::pb::Class;

/// `EntityKind`/`uid_of`/`kind_of` are defined once, in `bpsr-meter` (issue
/// #371) — this crate re-exports them rather than keeping a hand-written
/// copy of the wire's uuid bit layout. See `bpsr_meter::event::kind_of` for
/// the layout and its sourcing, and `bpsr_meter::event::EntityKind` for the
/// type itself.
pub use bpsr_meter::{EntityKind, kind_of, uid_of};

/// Which of the (evidenced) `pb::EDamageType` buckets a hit's `value`
/// belongs in (issue #338). Deliberately narrower than the wire enum: `Miss`
/// and `Heal` already have their own booleans on `DamageEvent`
/// (`is_miss`/`is_heal`), decoded independently for backward compatibility,
/// so this only distinguishes the two the meter previously had no channel
/// for — a shield-absorbed hit and an immunity-blocked one — from
/// everything else. `Fall` (`pb::EDamageType::Fall`) has no dedicated
/// variant here for the same reason it has none on the wire enum's doc
/// comment: no live-capture evidence separates it from `Normal`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum DamageKind {
    #[default]
    Normal,
    /// `pb::EDamageType::Absorbed` — a target's shield (`AttrShieldList`)
    /// soaked this hit. `value` is the amount the shield absorbed, not
    /// damage the target's HP took, so the meter must not fold it into
    /// `damage`/DPS (issue #338).
    Absorbed,
    /// `pb::EDamageType::Immune` — the target was immune to this hit.
    /// `value` is usually `0`, but MessageManager.cs notes rare packets
    /// where it isn't even though HP didn't change, so this still gets its
    /// own channel rather than being assumed to always be a no-op.
    Immune,
}

/// Reads a raw `pb::SyncDamageInfo.r#type` value into [`DamageKind`].
/// Anything other than `Absorbed`/`Immune` — including `Normal`, `Miss`,
/// `Heal`, `Fall`, and any unrecognized future value — maps to `Normal`,
/// matching `DamageKind`'s `Default`: those cases already have their own
/// signal (`is_miss`/`is_heal`) or no signal at all, so there's nothing this
/// function should do differently for them.
pub fn damage_kind_of(raw_type: i32) -> DamageKind {
    match raw_type {
        v if v == crate::pb::EDamageType::Absorbed as i32 => DamageKind::Absorbed,
        v if v == crate::pb::EDamageType::Immune as i32 => DamageKind::Immune,
        _ => DamageKind::Normal,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DamageEvent {
    /// Who dealt this hit, as a whole-uuid identity (issue #335). This is
    /// the key a consumer must file per-entity state under; `attacker_uid`
    /// below is the same entity's *display* number and is not unique.
    ///
    /// Pet/summon damage is already attributed to the top summoner here,
    /// exactly as `attacker_uid` is.
    pub attacker: EntityId,
    /// `attacker.display_uid()` — the short `uuid >> 16` every display
    /// surface, name cache and golden file uses. Two different entities can
    /// share one (issue #335), so never key stats on it.
    pub attacker_uid: i64,
    pub attacker_kind: EntityKind,
    pub skill_id: i32,
    pub value: i64,
    pub crit: bool,
    pub lucky: bool,
    pub hp_lessen: i64,
    pub is_miss: bool,
    pub is_heal: bool,
    /// Which absorbed/immune channel (if any) this hit's `value` belongs to
    /// (issue #338), sourced from `pb::SyncDamageInfo.r#type` via
    /// [`damage_kind_of`].
    pub kind: DamageKind,
    /// Who was hit, as a whole-uuid identity (issue #335) — the counterpart
    /// of `attacker` above.
    pub target: EntityId,
    /// `target.display_uid()`. Display only; see `attacker_uid`.
    pub target_uid: i64,
    pub target_kind: EntityKind,
    pub timestamp_ms: u64,
    /// Whether the target died from this hit, sourced from
    /// `pb::SyncDamageInfo` tag 17 (`is_dead`). This is a per-hit flag on the
    /// *victim*, not the attacker — a player entry here means that player
    /// died, not that they scored a kill (issue #49).
    pub is_dead: bool,
}

/// One skill activation (issue #245). Carries no amount of any kind: a
/// cast is a use of a skill, whether or not anything came of it, which is
/// exactly what makes the Skill casts tab different from the Dps tab's hit
/// counts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CastEvent {
    /// Who cast, as a whole-uuid identity (issue #335).
    pub caster: EntityId,
    /// `caster.display_uid()`. Display only; see [`DamageEvent::attacker_uid`].
    pub caster_uid: i64,
    pub skill_id: i32,
    pub timestamp_ms: u64,
    /// Issue #287's skill-cast metadata cluster, decoded alongside
    /// `skill_id` off the same `AoiSyncDelta` (see
    /// `attrs::skill_cast_metadata_from_attrs`). Every field below is
    /// independently `None` when its id was absent from this particular
    /// delta. [`attrs::attr_id::SKILL_STAGE`] — medium confidence.
    pub skill_stage: Option<i32>,
    /// [`attrs::attr_id::SKILL_LEVEL`] — medium confidence.
    pub skill_level: Option<i32>,
    /// [`attrs::attr_id::SKILL_BEGIN_TIME`] — high confidence: a real cast
    /// begin time (Unix epoch ms), not packet-arrival time.
    pub skill_begin_time_ms: Option<i64>,
    /// [`attrs::attr_id::SKILL_STAGE_NUM`] — medium confidence.
    pub skill_stage_num: Option<i32>,
    /// [`attrs::attr_id::SKILL_UUID`] — medium confidence; not a `uid_of`-
    /// shaped entity uuid.
    pub skill_uuid: Option<i32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlayerInfo {
    /// This player's whole-uuid identity (issue #335). The two wire sources
    /// that carry a bare `char_id` and no uuid — `SyncContainerData` and
    /// `NotifyJoinTeam` — resolve theirs through
    /// [`crate::entity::EntityTable::resolve_uid`], so every `PlayerInfo`
    /// carries a usable one however it was sourced.
    pub entity: EntityId,
    /// `entity.display_uid()`. Display only; see [`DamageEvent::attacker_uid`].
    pub uid: i64,
    pub name: Option<String>,
    pub class: Option<Class>,
    /// Ability score (a.k.a. combat power), sourced from `attr_id::FIGHT_POINT`
    /// or `CharBaseInfo.fight_point` — not every packet carries it, so this is
    /// `None` rather than `Some(0)` when absent (issue #15).
    pub ability_score: Option<u32>,
    /// Season level, sourced from `attr_id::SEASON_LEVEL`. `None` rather than
    /// `Some(0)` when absent (issue #15).
    ///
    /// Deliberately decoded but unconsumed above this crate: the UI column
    /// that displayed it was removed in #51, and issue #55 stripped the rest
    /// of the chain (`bpsr_meter::PlayerStats`/`PlayerRow`,
    /// `pipeline::map_event`). `SEASON_LEVEL` is a documented
    /// reverse-engineering finding (`docs/packet-inspection.md`), so it stays
    /// decoded here on purpose — do not "clean up" this field by removing it
    /// from the protocol layer too.
    pub season_level: Option<u32>,
    /// Season strength, sourced from `attr_id::SEASON_STRENGTH`. `None`
    /// rather than `Some(0)` when absent (issue #15).
    pub season_strength: Option<u32>,
    /// Equipped Imagine skills (issue #33) as `(skill_id, remodel_level)`
    /// pairs, sourced from `attr_id::SKILL_LEVEL_ID_LIST` (`0x74`), in wire
    /// order. **Empty == absent** — matching `FIGHT_POINT`'s zero-is-absent
    /// rule — whether because the attr wasn't present in this packet or
    /// because it decoded to no ids. `remodel_level` is the tier field
    /// (issues #169/#170; BPSR-ZDPS calls it `Tier`) — see
    /// `attrs::decode_skill_ids`'s doc comment for the wire correspondence.
    /// This crate stops at raw ids and tiers: it never learns what an
    /// Imagine *is* (name/icon classification happens above this crate, in
    /// `crates/app`).
    pub skill_ids: Vec<(i32, i32)>,
    /// World position, sourced from `attr_id::POSITION` (`0x34`, issue
    /// #286). `None` when this packet's attrs carried no position update —
    /// see `attrs::attr_id::POSITION`'s doc comment for the wire evidence
    /// and sourcing.
    pub position: Option<[f32; 3]>,
    /// Target/destination position, sourced from `attr_id::TARGET_POSITION`
    /// (`0x35`, issue #286) — rides the same attr channel as `position`
    /// above but is a distinct id; see `attrs::attr_id::TARGET_POSITION`'s
    /// doc comment for why these are believed to be current-vs-target
    /// rather than duplicates, and the confidence caveat on that belief.
    pub target_position: Option<[f32; 3]>,
    /// Current total shield value, sourced from `attr_id::SHIELD_LIST`
    /// (`AttrShieldList`, issue #338) — summed across every active shield
    /// instance this delta's attrs carried. `None` when this packet's
    /// attrs carried no shield-list update at all (not the same as "no
    /// shield right now", which decodes as `Some(0)` — see
    /// `attrs::decode_shield_total`'s doc comment).
    pub shield: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnemyHp {
    /// This enemy's whole-uuid identity (issue #335).
    pub entity: EntityId,
    /// `entity.display_uid()`. Display only; see [`DamageEvent::attacker_uid`].
    pub uid: i64,
    pub curr_hp: Option<u64>,
    pub max_hp: Option<u64>,
    pub monster_id: Option<u32>,
    pub timestamp_ms: u64,
    /// World position, sourced from `attr_id::POSITION` (`0x34`, issue
    /// #286) — same field and sourcing as `PlayerInfo::position`, applied
    /// to monster/boss entities.
    pub position: Option<[f32; 3]>,
    /// Target/destination position, sourced from `attr_id::TARGET_POSITION`
    /// (`0x35`, issue #286) — same field and sourcing as
    /// `PlayerInfo::target_position`.
    pub target_position: Option<[f32; 3]>,
}

/// Dungeon flow state (`decode::opcode::SYNC_DUNGEON_DATA` /
/// `SYNC_DUNGEON_DIRTY_DATA`'s `FlowInfo.state`, issue #139).
///
/// Mirrors BPSR-ZDPS's `EnumEDungeonState.cs` (`Null=0, Active=1, Ready=2,
/// Playing=3, End=4, Settlement=5, Vote=6`). `Playing` was never observed in
/// this build's real captures — the capture began mid-dungeon — so it is
/// modeled from the reference source only, same as every other named
/// variant here.
///
/// `Unknown` is a deliberate, explicit variant: folding a future/unrecognized
/// state value into `Null` would silently read a still-active dungeon as
/// "open world", which is exactly the wrong failure mode for this signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EDungeonState {
    Null,
    Active,
    Ready,
    Playing,
    End,
    Settlement,
    Vote,
    Unknown(i32),
}

impl From<i32> for EDungeonState {
    fn from(v: i32) -> Self {
        match v {
            0 => EDungeonState::Null,
            1 => EDungeonState::Active,
            2 => EDungeonState::Ready,
            3 => EDungeonState::Playing,
            4 => EDungeonState::End,
            5 => EDungeonState::Settlement,
            6 => EDungeonState::Vote,
            other => EDungeonState::Unknown(other),
        }
    }
}

/// Why an entity left the client's area of interest, mirroring
/// [`crate::pb::EDisappearType`] (issue #276) — see that type's doc comment
/// for the reference sourcing and the live-capture evidence behind each
/// variant.
///
/// `Unknown` is an explicit variant for the same reason [`EDungeonState`]'s
/// is: a future value must not be silently folded into a named one. Every
/// consumer treats it as "no usable reason", which is the conservative
/// reading here — see `bpsr_meter::Meter::apply_enemy_gone`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DisappearReason {
    Normal,
    Dead,
    Destroy,
    TransferLeave,
    TransferPassLineLeave,
    Unknown(i32),
}

impl From<i32> for DisappearReason {
    fn from(v: i32) -> Self {
        match v {
            0 => DisappearReason::Normal,
            1 => DisappearReason::Dead,
            2 => DisappearReason::Destroy,
            3 => DisappearReason::TransferLeave,
            4 => DisappearReason::TransferPassLineLeave,
            other => DisappearReason::Unknown(other),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProtocolEvent {
    Damage(DamageEvent),
    /// One entity began casting one skill (issue #245), decoded from
    /// `AttrSkillId` on `AoiSyncDelta`'s attr channel — see
    /// `attrs::attr_id::SKILL_ID` for the wire evidence, and
    /// `decode::on_aoi_sync_delta` for the emit site.
    ///
    /// Emitted for players only. The attr rides every entity's deltas, but
    /// a monster's cast count has nowhere to go: `bpsr-meter` keeps no
    /// per-monster skill breakdown, and the breakdown window is opened from
    /// a player row.
    Cast(CastEvent),
    Player(PlayerInfo),
    EnemyHp(EnemyHp),
    /// The dungeon/instance id (issue #9 slice 2), decoded from either of
    /// two sources: `AttrSceneBasicId` on `WorldNtf.EnterScene`'s attr
    /// channel (see `decode::on_enter_scene` and
    /// `attrs::attr_id::SCENE_BASIC_ID`), or `SyncContainerData`'s
    /// `CharSerialize.scene_data.level_map_id` (see
    /// `decode::on_sync_container_data`). `EnterScene` fires once, on zone
    /// entry; `SyncContainerData` is a full-state push that can land any
    /// time a client's session with the world is (re)established,
    /// including well after zone entry — which is what makes it the source
    /// a meter attached mid-instance actually sees (issue #293). Consumers
    /// must treat a repeat of the same id as a no-op, not a fresh
    /// transition: see `bpsr-meter`'s `Scene` handling.
    ///
    /// `CharSerialize.scene_data` was originally ported, then pulled as
    /// "disproven" (issue #35) on a single capture that turned out to only
    /// ever catch `SyncContainerData` after `EnterScene` had already fired
    /// — see `pb::CharSerialize::scene_data`'s doc comment for the
    /// byte-level re-verification that brought it back.
    Scene {
        level_map_id: u32,
    },
    ServerChanged,
    /// A dungeon flow-state transition (issue #139), decoded from either
    /// `SYNC_DUNGEON_DATA`'s plain-protobuf `FlowInfo` or
    /// `SYNC_DUNGEON_DIRTY_DATA`'s blob-encoded one — see
    /// `decode::on_sync_dungeon_data` / `decode::on_sync_dungeon_dirty_data`.
    /// `scene_uuid` is only ever `Some` alongside a freshly observed
    /// `state` in this build's real captures; it is not independently
    /// tracked.
    DungeonState {
        state: EDungeonState,
        scene_uuid: Option<u32>,
    },
    /// One dungeon objective's progress (issue #139), decoded from
    /// `SYNC_DUNGEON_DIRTY_DATA`'s blob-encoded `Target` hashmap —
    /// `target_id` is always the hashmap key, never
    /// `blob::TargetData.target_id` (an *update* entry commonly omits it;
    /// see `blob::TargetData`'s doc comment). Emitted once per changed
    /// hashmap entry (both the `add` and `update` sides), in ascending
    /// `target_id` order so the event stream is deterministic — the wire
    /// hashmap's own iteration order is not.
    DungeonObjective {
        target_id: i32,
        nums: Option<i32>,
        complete: Option<bool>,
    },
    /// One dungeon objective disappearing from the wire's `Target`
    /// hashmap (issue #139), decoded from `SYNC_DUNGEON_DIRTY_DATA`'s
    /// `remove` side. The remove entries carry bare keys and no
    /// `TargetData` at all (see `blob::HashmapDelta`), so this event says
    /// only that the objective is gone — emphatically *not* that it
    /// completed; the meter has to be able to tell those apart, which is
    /// the whole reason this is its own variant rather than a synthesized
    /// `DungeonObjective { complete: Some(true) }`. Emitted before the
    /// same message's `add`/`update` events, and in ascending id order —
    /// see `decode::on_sync_dungeon_dirty_data` for both reasons.
    DungeonObjectiveRemoved {
        target_id: i32,
    },
    /// A named dungeon variable (issue #139), decoded from
    /// `SYNC_DUNGEON_DIRTY_DATA`'s blob-encoded `DungeonVar` list. Emitted
    /// for every var this build carries (`IsFinishTarget` was never
    /// observed in this build's real captures, but the channel itself —
    /// 201 messages, real names — is verified) — this crate emits every
    /// one and leaves interpreting them to the meter layer.
    DungeonVar {
        name: String,
        value: i32,
    },
    /// An entity left the client's area of interest (issue #215), decoded
    /// from `pb::SyncNearEntities.disappear` — the counterpart of the
    /// `appear` list that produces `Player`/`EnemyHp`. Emitted for monsters
    /// only: a player walking out of range says nothing this meter tracks.
    ///
    /// `reason` is the server's own statement of *why* (issue #276), decoded
    /// from `pb::DisappearEntity`'s optional tag 2 — see
    /// [`crate::pb::EDisappearType`] for its sourcing and capture evidence.
    /// It is genuinely optional: 382 of 851 observed disappear entries carry
    /// no tag 2 at all, so `None` means "the server did not say", never
    /// "nothing happened".
    ///
    /// **A despawn is still not, by itself, a death.** Only
    /// [`DisappearReason::Dead`] says the entity died; `Destroy`,
    /// `TransferLeave` and `Normal` are evictions, zone-outs and ordinary
    /// streaming churn, and a `None` says nothing either way. This crate
    /// therefore reports the fact and the server's reason and nothing more;
    /// deciding whether a particular despawn ends a fight is the meter's
    /// job, under the rule documented on
    /// `bpsr_meter::Meter::apply_enemy_gone`.
    EnemyGone {
        /// The departing enemy's whole-uuid identity (issue #335).
        entity: EntityId,
        /// `entity.display_uid()`. Display only.
        uid: i64,
        reason: Option<DisappearReason>,
    },
    /// A buff was applied, refreshed, or gained a stack layer on `host_uid`
    /// (issue #267), decoded from `AoiSyncDelta.buff_effect` — see
    /// `crate::pb::AoiSyncDelta::buff_effect`'s doc comment for the field-tag
    /// evidence and `crate::decode::on_aoi_sync_delta` for which
    /// `EBuffEventType` values map to this variant.
    ///
    /// `base_id` is the buff's definition/template id, present only when
    /// this particular wire event carried the double-encoded `BuffInfo` (see
    /// `crate::pb::BUFF_EFFECT_ADD_BUFF`) — roughly half of apply events in
    /// this project's own captures do not, so `None` here does not mean "not
    /// a real buff", only "this event didn't say which one". The consumer is
    /// expected to remember a buff's `base_id` from whichever event first
    /// supplies it, keyed by `buff_uuid`.
    ///
    /// `adds_layer` is true only for `StackLayer`, the one apply-shaped
    /// event that grows a stacking buff's layer count; `AddTo`/`Replace` on
    /// an instance that is already up are refreshes, not extra layers. It
    /// pairs with [`ProtocolEvent::BuffRemove::removes_layer`] so a consumer
    /// can tell a partial teardown from a full one.
    BuffApply {
        /// The buffed entity's whole-uuid identity (issue #335).
        host: EntityId,
        /// `host.display_uid()`. Display only.
        host_uid: i64,
        buff_uuid: i32,
        base_id: Option<i32>,
        adds_layer: bool,
        timestamp_ms: u64,
    },
    /// A buff was removed, or lost a stack layer, on `host_uid` (issue
    /// #267). Never carries a `base_id`: the wire's `Remove`/`RemoveLayer`
    /// events observed in this project's own captures carry only
    /// `Type`/`BuffUuid` — never a `LogicEffect` list — so identifying which
    /// buff this is is the consumer's job, via `buff_uuid`.
    ///
    /// `removes_layer` distinguishes `RemoveLayer` (drops one layer; the
    /// instance is only gone once its last layer is) from `Remove` (the
    /// whole instance, however many layers it held). A single-layer buff
    /// ends the same way either way — which is why the near-parity of real
    /// `AddTo`/`RemoveLayer` counts documented on `pb::EBuffEventType` is
    /// not a contradiction.
    BuffRemove {
        /// The buffed entity's whole-uuid identity (issue #335).
        host: EntityId,
        /// `host.display_uid()`. Display only.
        host_uid: i64,
        buff_uuid: i32,
        removes_layer: bool,
        timestamp_ms: u64,
    },
    /// An entity's decoded `AttrState` (issue #339/#272), sourced from
    /// `attrs::attr_id::STATE` on an `AoiSyncDelta`'s attr channel — see
    /// `attrs::entity_state_from_attrs` for the wire evidence and
    /// `decode::on_aoi_sync_delta` for the emit site. Emitted for both
    /// players and monsters (unlike `Cast`, which is player-only): a boss's
    /// own `AttrState` transitioning to dead is one of this issue's two
    /// explicit death signals.
    ///
    /// `is_dead` is the only bit this crate extracts from the wire's full
    /// `EActorState` enum — every non-dead state (skill, stiff, born, the
    /// resurrection animation, ...) collapses to `false`. Consumers must
    /// not read `false` as "just revived": it means only "not currently in
    /// the dead state", which is true on every ordinary delta a live
    /// entity ever sends.
    EntityState {
        /// The entity's whole-uuid identity (issue #335).
        entity: EntityId,
        /// `entity.display_uid()`. Display only; never key state on it.
        uid: i64,
        kind: EntityKind,
        is_dead: bool,
        timestamp_ms: u64,
    },
    /// `WorldNtf.NotifyReviveUser` (opcode `0x27`, issue #272), the
    /// dedicated revive notify — see `decode::opcode::NOTIFY_REVIVE_USER`
    /// for the wire evidence. Carries only the revived actor's uid and the
    /// packet-arrival timestamp; unlike the inferred "next action implies
    /// alive" fallback it replaces, this is the actual revive moment.
    Revive {
        /// The revived actor's whole-uuid identity (issue #335).
        entity: EntityId,
        /// `entity.display_uid()`. Display only.
        uid: i64,
        timestamp_ms: u64,
    },
    /// One party member left or was kicked from the team (issue #343),
    /// decoded from `GrpcTeamNtf.NotifyLeaveTeam` — see
    /// `crate::decode::on_notify_leave_team`. The wire's `leaveType` field
    /// tells voluntary leaves and kicks apart, but nothing downstream
    /// treats them differently (both mean "this uid is no longer in the
    /// roster"), so it is not carried onto this event.
    TeamMemberLeft {
        uid: i64,
    },
    /// The authoritative party/raid roster as of this `NotifyJoinTeam`
    /// push (issue #343), decoded alongside that message's per-member
    /// `Player` events — see `crate::decode::on_notify_join_team`.
    /// `NotifyJoinTeam` is not purely additive: BPSR-ZDPS's own client
    /// resends the *whole* roster (up to a 20-player raid) on every team
    /// change, not just the delta, so this variant lets a consumer prune
    /// any roster row it holds that is no longer named here — the
    /// counterpart to `TeamMemberLeft` for changes this crate never saw an
    /// explicit leave/kick notify for (e.g. a member who left while this
    /// meter was attached to a different scene).
    ///
    /// `members` holds every roster uid this push resolved (the same
    /// `TeamMemData.char_id` / `TeamBasicData.char_id` fallback
    /// `on_notify_join_team` uses), including members whose `Player` event
    /// was suppressed for carrying no name/class/ability_score — a
    /// bot-like entry with nothing to display is still a roster member and
    /// must not be pruned as if it had left.
    TeamRoster {
        members: Vec<i64>,
    },
    /// The local player's own uid (issue #344), decoded from
    /// `SyncContainerData.v_data.char_id` — see
    /// `decode::on_sync_container_data`. `char_id` is a **bare** uid, not a
    /// packed `uuid`; unlike `Entity`/`AoiSyncDelta`/`SyncDamageInfo` ids it
    /// must never be shifted through `uid_of` (that trap is what sank the
    /// field's first pass in `crate::sanitize`, whose module doc (see
    /// crates/protocol/src/sanitize.rs:22-23) documents the same
    /// distinction for `CharSerialize.char_id`/`CharBaseInfo.char_id`).
    /// Emitted independently of `Player`: a
    /// `CharSerialize` with only `char_id` (no `char_base`) still says who
    /// the local player is, even though `on_sync_container_data` has
    /// nothing to build a `Player` event from in that case.
    LocalPlayer {
        uid: i64,
    },
}
