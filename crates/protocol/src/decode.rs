//! Opcode dispatch: `Notify.method_id` → `ProtocolEvent` (plan §T1.4).
//!
//! `Decoder` is the crate's only public façade — `push_stream` feeds raw
//! reassembled TCP bytes in and gets fully-typed events out. A decode error
//! anywhere in this module is dropped with a debug log; nothing here panics
//! or propagates an error to the caller.

use prost::Message;

use std::sync::Arc;

use crate::attrs::{
    enemy_hp_from_attrs, entity_state_from_attrs, player_info_from_attrs, scene_id_from_attrs,
    skill_cast_metadata_from_attrs,
};
use crate::blob;
use crate::event::{
    CastEvent, DamageEvent, DisappearReason, EDungeonState, EntityKind, PlayerInfo, ProtocolEvent,
    kind_of, uid_of,
};
use crate::frame::{
    Desync, MAX_TAIL_LEN, Notify, SERVICE_UUID, TEAM_NTF_SERVICE_UUID, parse_frame, split_frames,
};
use crate::inspect::InspectSink;
use crate::pb::{self, AoiSyncDelta, EDamageType};

pub mod opcode {
    pub const SYNC_NEAR_ENTITIES: u32 = 0x0000_0006;
    pub const SYNC_CONTAINER_DATA: u32 = 0x0000_0015;
    pub const SYNC_NEAR_DELTA_INFO: u32 = 0x0000_002d;
    pub const SYNC_TO_ME_DELTA_INFO: u32 = 0x0000_002e;
    /// `WorldNtf.EnterScene` (issue #35). Recovered, along with the four
    /// constants above, by parsing `BPSR_ZDPSLib.ServiceMethods.WorldNtf`
    /// out of the BPSR-ZDPS reference tool's .NET metadata — that parse
    /// independently reproduced all four existing opcodes byte-for-byte,
    /// which is what makes this value (and the enum's `SyncSceneAttrs` = 7
    /// / `SyncContainerDirtyData` = 22, not otherwise used by this crate)
    /// trustworthy rather than another unverified port. Carries the scene
    /// attrs (`attrs::attr_id::SCENE_BASIC_ID` et al.) that
    /// `on_enter_scene` reads `ProtocolEvent::Scene` from. Only fires once,
    /// on zone entry — `SYNC_CONTAINER_DATA`'s `on_sync_container_data` is
    /// the other, complementary source (issue #293): it also emits
    /// `ProtocolEvent::Scene`, from `CharSerialize.scene_data`, and is what
    /// a meter attached mid-instance actually sees (see
    /// `pb::CharSerialize::scene_data`'s doc comment).
    pub const ENTER_SCENE: u32 = 0x0000_0003;
    /// `WorldNtf.SyncDungeonData` (issue #139): the plain-protobuf full
    /// dungeon sync (`pb::DungeonSyncData`). Validated against this
    /// build's real captures — 6/6 `Notify` records on this opcode, every
    /// one empty (open world). See
    /// `docs/specs/2026-08-23-issue-139-dungeon-state-spec.md` for the
    /// capture evidence and `decode::on_sync_dungeon_data`.
    pub const SYNC_DUNGEON_DATA: u32 = 0x0000_0017;
    /// `WorldNtf.SyncDungeonDirtyData` (issue #139): the blob-wrapped
    /// dungeon dirty-data channel (`pb::SyncDungeonDirtyData`, see
    /// `crate::blob`'s module doc for the inner wire format). Validated
    /// against this build's real captures — 392/392 `Notify` records on
    /// this opcode parsed with zero failures by the Python prototype this
    /// port reproduces; see
    /// `docs/specs/2026-08-23-issue-139-dungeon-state-spec.md` and
    /// `decode::on_sync_dungeon_dirty_data`.
    pub const SYNC_DUNGEON_DIRTY_DATA: u32 = 0x0000_0018;
    /// `WorldNtf.NotifyReviveUser` (issue #272/#339) — see
    /// `pb::NotifyReviveUser`'s doc comment for the full sourcing and the
    /// live-capture caveat.
    pub const NOTIFY_REVIVE_USER: u32 = 0x0000_0027;
}

/// Method ids on `frame::TEAM_NTF_SERVICE_UUID` (`EServiceId.GrpcTeamNtf`,
/// issue #146) — kept in their own module rather than mixed into `opcode`
/// because the two services' method-id spaces overlap: `0x3` is
/// `ENTER_SCENE` on the main service (`opcode::ENTER_SCENE`) and
/// `NotifyJoinTeam` here. `decode_notify` dispatches on
/// `(Notify.service_uuid, Notify.method_id)` together so these never
/// collide with `opcode`'s constants despite sharing raw values.
pub mod team_opcode {
    /// `NotifyJoinTeam` — the bulk party-roster push, and the only
    /// `GrpcTeamNtf` method this crate decodes (issue #146).
    pub const NOTIFY_JOIN_TEAM: u32 = 0x3;
    /// `NoticeUpdateTeamInfo`. No documented field tags (issue #146's
    /// table covers only `NotifyJoinTeam`) — left unhandled on purpose so
    /// traffic on this method falls through to the inspect sink instead of
    /// being guessed at.
    #[allow(dead_code)]
    pub const NOTICE_UPDATE_TEAM_INFO: u32 = 0x1;
    /// `NotifyLeaveTeam`. Same as `NOTICE_UPDATE_TEAM_INFO` above: no
    /// documented field tags, deliberately unhandled.
    #[allow(dead_code)]
    pub const NOTIFY_LEAVE_TEAM: u32 = 0x4;
}

/// Decodes one Notify's payload and appends any resulting events to `out`.
/// Unknown opcodes are skipped silently; a prost decode failure is dropped
/// with a debug log. `sink` is the issue #25 slice A diagnostic hook: `None`
/// on every normal (non-diagnostic) call site.
///
/// Dispatches on `(n.service_uuid, n.method_id)` together, never on
/// `method_id` alone (issue #146): the main service and
/// `frame::TEAM_NTF_SERVICE_UUID` have overlapping method-id spaces — `0x3`
/// is `opcode::ENTER_SCENE` on the former and `team_opcode::NOTIFY_JOIN_TEAM`
/// on the latter — so a method-id-only match would route team traffic into
/// the scene decoder (or vice versa) whenever the two happened to share a
/// value.
pub fn decode_notify(
    n: &Notify,
    now_ms: u64,
    out: &mut Vec<ProtocolEvent>,
    sink: Option<&dyn InspectSink>,
) {
    match (n.service_uuid, n.method_id) {
        (SERVICE_UUID, opcode::SYNC_NEAR_ENTITIES) => {
            match pb::SyncNearEntities::decode(n.payload.as_slice()) {
                Ok(msg) => on_sync_near_entities(&msg, now_ms, out, sink),
                Err(_) => log::debug!("bpsr-protocol: SyncNearEntities decode failed"),
            }
        }
        (SERVICE_UUID, opcode::SYNC_CONTAINER_DATA) => {
            match pb::SyncContainerData::decode(n.payload.as_slice()) {
                Ok(msg) => on_sync_container_data(&msg, out),
                Err(_) => log::debug!("bpsr-protocol: SyncContainerData decode failed"),
            }
        }
        (SERVICE_UUID, opcode::ENTER_SCENE) => match pb::EnterScene::decode(n.payload.as_slice()) {
            Ok(msg) => on_enter_scene(&msg, out, sink),
            Err(_) => log::debug!("bpsr-protocol: EnterScene decode failed"),
        },
        (SERVICE_UUID, opcode::SYNC_DUNGEON_DATA) => {
            match pb::DungeonSyncData::decode(n.payload.as_slice()) {
                Ok(msg) => on_sync_dungeon_data(&msg, out),
                Err(_) => log::debug!("bpsr-protocol: DungeonSyncData decode failed"),
            }
        }
        (SERVICE_UUID, opcode::SYNC_DUNGEON_DIRTY_DATA) => {
            match pb::SyncDungeonDirtyData::decode(n.payload.as_slice()) {
                Ok(msg) => on_sync_dungeon_dirty_data(&msg, out),
                Err(_) => log::debug!("bpsr-protocol: SyncDungeonDirtyData decode failed"),
            }
        }
        (SERVICE_UUID, opcode::NOTIFY_REVIVE_USER) => {
            match pb::NotifyReviveUser::decode(n.payload.as_slice()) {
                Ok(msg) => on_notify_revive_user(&msg, now_ms, out),
                Err(_) => log::debug!("bpsr-protocol: NotifyReviveUser decode failed"),
            }
        }
        (SERVICE_UUID, opcode::SYNC_NEAR_DELTA_INFO) => {
            match pb::SyncNearDeltaInfo::decode(n.payload.as_slice()) {
                Ok(msg) => {
                    for delta in &msg.delta_infos {
                        on_aoi_sync_delta(delta, delta.uuid, now_ms, out, sink);
                    }
                }
                Err(_) => log::debug!("bpsr-protocol: SyncNearDeltaInfo decode failed"),
            }
        }
        (SERVICE_UUID, opcode::SYNC_TO_ME_DELTA_INFO) => {
            match pb::SyncToMeDeltaInfo::decode(n.payload.as_slice()) {
                Ok(msg) => {
                    if let Some(to_me) = msg.delta_info {
                        // The wrapper's own `uuid` identifies the entity this
                        // "to-me" update is about; `base_delta.uuid` is usually 0
                        // and only serves as a fallback.
                        if let Some(delta) = &to_me.base_delta {
                            let uuid = if to_me.uuid != 0 {
                                to_me.uuid
                            } else {
                                delta.uuid
                            };
                            on_aoi_sync_delta(delta, uuid, now_ms, out, sink);
                        }
                    }
                }
                Err(_) => log::debug!("bpsr-protocol: SyncToMeDeltaInfo decode failed"),
            }
        }
        (TEAM_NTF_SERVICE_UUID, team_opcode::NOTIFY_JOIN_TEAM) => {
            match pb::NotifyJoinTeam::decode(n.payload.as_slice()) {
                Ok(msg) => on_notify_join_team(&msg, out),
                Err(_) => log::debug!("bpsr-protocol: NotifyJoinTeam decode failed"),
            }
        }
        // Every other (service, method) pair is skipped on purpose. The
        // largest single main-service opcode left unhandled,
        // `SyncContainerDirtyData` (0x16, a few hundred messages a session), was
        // investigated against a real capture and is *not* worth decoding: all
        // 264 payloads in that capture share the shape
        // `{ 1: { 1: bytes diff_blob, 2: varint == 1 } }`, where `diff_blob` is
        // not protobuf at all but a proprietary little-endian i32 tree keyed by
        // `0xFFFF_FFFE`/`0xFFFF_FFFD` sentinel words. It carries no entity uuid
        // and no `AttrCollection`: none of the distinctive attr ids (220
        // PROFESSION_ID, 10030 FIGHT_POINT, 11310 HP, 11320 MAX_HP) appears
        // anywhere in it. It is a container/inventory diff channel, so every
        // entity attribute we care about still arrives on the four opcodes
        // above. `team_opcode::NOTICE_UPDATE_TEAM_INFO` /
        // `team_opcode::NOTIFY_LEAVE_TEAM` also fall through here (issue
        // #146): no documented field tags for either.
        _ => {}
    }
}

fn on_sync_near_entities(
    msg: &pb::SyncNearEntities,
    now_ms: u64,
    out: &mut Vec<ProtocolEvent>,
    sink: Option<&dyn InspectSink>,
) {
    for entity in &msg.appear {
        let Some(attrs) = &entity.attrs else {
            continue;
        };
        let uid = uid_of(entity.uuid);
        match kind_of(entity.uuid) {
            EntityKind::Player => {
                out.push(ProtocolEvent::Player(player_info_from_attrs(
                    uid,
                    &attrs.attrs,
                    sink,
                )));
            }
            EntityKind::Monster => {
                out.push(ProtocolEvent::EnemyHp(enemy_hp_from_attrs(
                    uid,
                    &attrs.attrs,
                    now_ms,
                    sink,
                )));
            }
            EntityKind::Unknown => {}
        }
    }
    // issue #215: the `disappear` list, decoded after `appear` so a single
    // packet's events stay in wire-field order. Monsters only — a player
    // leaving AOI range is not something the meter models, and an entity
    // type it has no model for is dropped here exactly as it is above.
    // See `ProtocolEvent::EnemyGone` for why this is emitted as a "gone"
    // fact plus the server's own reason rather than as a death: tag 2
    // (issue #276) says *why* the entity vanished, but only
    // `DisappearReason::Dead` says it died, and it is absent entirely on
    // 382 of the 851 disappear entries in our captures.
    for entity in &msg.disappear {
        if kind_of(entity.uuid) == EntityKind::Monster {
            out.push(ProtocolEvent::EnemyGone {
                uid: uid_of(entity.uuid),
                reason: entity.disappear_type.map(DisappearReason::from),
            });
        }
    }
}

/// `uuid` is the delta's identity (the entity the attrs/damage apply to); it
/// is passed in because a `SyncToMeDeltaInfo` carries it on the wrapper rather
/// than on the `AoiSyncDelta` itself.
fn on_aoi_sync_delta(
    delta: &AoiSyncDelta,
    uuid: i64,
    now_ms: u64,
    out: &mut Vec<ProtocolEvent>,
    sink: Option<&dyn InspectSink>,
) {
    let target_uid = uid_of(uuid);
    let target_kind = kind_of(uuid);
    if let Some(attrs) = &delta.attrs {
        match target_kind {
            EntityKind::Player => {
                out.push(ProtocolEvent::Player(player_info_from_attrs(
                    target_uid,
                    &attrs.attrs,
                    sink,
                )));
                // Issue #245: the same attr channel reports a cast by
                // changing `AttrSkillId` on the caster. Emitted as its own
                // event rather than folded into `PlayerInfo`, because a
                // cast is a *thing that happened at a time*, while every
                // other field on `PlayerInfo` is a standing property the
                // meter merges rather than counts.
                //
                // Issue #287: the metadata cluster rides the same attr
                // delta as `skill_id`, so both are decoded together in one
                // walk of `attrs.attrs` — each field (including `skill_id`
                // itself) is independently `None` when its own id is
                // absent.
                let meta = skill_cast_metadata_from_attrs(&attrs.attrs);
                if let Some(skill_id) = meta.skill_id {
                    out.push(ProtocolEvent::Cast(CastEvent {
                        caster_uid: target_uid,
                        skill_id,
                        timestamp_ms: now_ms,
                        skill_stage: meta.skill_stage,
                        skill_level: meta.skill_level,
                        skill_begin_time_ms: meta.skill_begin_time_ms,
                        skill_stage_num: meta.skill_stage_num,
                        skill_uuid: meta.skill_uuid,
                    }));
                }
            }
            EntityKind::Monster => {
                out.push(ProtocolEvent::EnemyHp(enemy_hp_from_attrs(
                    target_uid,
                    &attrs.attrs,
                    now_ms,
                    sink,
                )));
            }
            EntityKind::Unknown => {}
        }
        // Issue #339/#272: `AttrState` rides the same attr channel as every
        // other field above and is decoded for both players and monsters —
        // a boss's own dead state is one of the two explicit death signals
        // this issue adds (the other is `Revive`/`SyncDamageInfo.is_dead`).
        // `Unknown`-kind entities are dropped everywhere else in this
        // function, so no event is emitted for them here either.
        if target_kind != EntityKind::Unknown
            && let Some(is_dead) = entity_state_from_attrs(&attrs.attrs)
        {
            out.push(ProtocolEvent::EntityState {
                uid: target_uid,
                kind: target_kind,
                is_dead,
                timestamp_ms: now_ms,
            });
        }
    }
    if let Some(effects) = &delta.skill_effects {
        for dmg in &effects.damages {
            // Pet/summon damage is attributed to the top summoner.
            let attacker_uuid = if dmg.top_summoner_id != 0 {
                dmg.top_summoner_id
            } else if dmg.attacker_uuid != 0 {
                dmg.attacker_uuid
            } else {
                continue;
            };
            // No skill id → skip.
            if dmg.owner_id == 0 {
                continue;
            }
            let value = if dmg.lucky_value != 0 {
                dmg.lucky_value
            } else {
                dmg.value
            };
            // A target's max HP can grow between the hit landing and this
            // packet being built, making the server's own "new HP minus old
            // HP" arithmetic underflow into a negative `value` (BPSR-ZDPS
            // `MessageManager.cs` ~1372-1384). `hp_lessen_value` is computed
            // independently and stays correct, so it takes over whenever it
            // is usable; otherwise the hit reports no damage rather than a
            // negative one. This guard applies to damage-typed hits only:
            // heal-typed `SyncDamageInfo` can legitimately carry a negative
            // `value` (lethal/self-damage heals — see encounter.rs's
            // negative/lethal heal handling), so those pass through
            // unchanged.
            let is_heal = dmg.r#type == EDamageType::Heal as i32;
            let value = if !is_heal && value < 0 {
                if dmg.hp_lessen_value > 0 {
                    dmg.hp_lessen_value
                } else {
                    0
                }
            } else {
                value
            };
            out.push(ProtocolEvent::Damage(DamageEvent {
                attacker_uid: uid_of(attacker_uuid),
                attacker_kind: kind_of(attacker_uuid),
                skill_id: dmg.owner_id,
                value,
                crit: dmg.type_flag & 1 != 0,
                lucky: dmg.lucky_value != 0,
                hp_lessen: dmg.hp_lessen_value,
                is_miss: dmg.is_miss || dmg.r#type == EDamageType::Miss as i32,
                is_heal,
                target_uid,
                target_kind,
                timestamp_ms: now_ms,
                // `dmg.is_dead` (SyncDamageInfo tag 17) flags that
                // `target_uid` died from this hit — a victim-side signal,
                // not an attacker-side kill count (issue #49).
                is_dead: dmg.is_dead,
            }));
        }
    }
    // Issue #267: buff apply/remove/stack events. Player-only, matching the
    // rest of this function — `bpsr_meter` keeps no per-monster buff
    // breakdown, and the skill window is opened from a player row. Uses
    // `target_uid` (the delta's own identity) rather than decoding
    // `be.host_uuid` a second time: every sample in this project's own
    // captures had them equal (see `pb::AoiSyncDelta::buff_effect`'s doc
    // comment), and `target_uid` is already computed above.
    if target_kind == EntityKind::Player
        && let Some(buff_effect) = &delta.buff_effect
    {
        for be in &buff_effect.buff_effects {
            if is_buff_apply_event(be.r#type) {
                out.push(ProtocolEvent::BuffApply {
                    host_uid: target_uid,
                    buff_uuid: be.buff_uuid,
                    base_id: buff_base_id_from_logic_effects(&be.logic_effect),
                    adds_layer: be.r#type == pb::EBuffEventType::StackLayer as i32,
                    timestamp_ms: now_ms,
                });
            } else if is_buff_remove_event(be.r#type) {
                out.push(ProtocolEvent::BuffRemove {
                    host_uid: target_uid,
                    buff_uuid: be.buff_uuid,
                    removes_layer: be.r#type == pb::EBuffEventType::RemoveLayer as i32,
                    timestamp_ms: now_ms,
                });
            }
        }
    }
}

/// Whether a `BuffEffect.type` (issue #267) marks a buff as up: a fresh
/// application, a re-application/refresh, or a new stack layer. See
/// `pb::AoiSyncDelta::buff_effect`'s doc comment for the confirmed-vs-ported
/// status of each `EBuffEventType` variant.
fn is_buff_apply_event(r#type: i32) -> bool {
    r#type == pb::EBuffEventType::AddTo as i32
        || r#type == pb::EBuffEventType::Replace as i32
        || r#type == pb::EBuffEventType::StackLayer as i32
}

/// Whether a `BuffEffect.type` (issue #267) tears a buff down, in full
/// (`Remove`) or by one stack layer (`RemoveLayer`). Which of the two it was
/// rides along on `ProtocolEvent::BuffRemove::removes_layer`, so the meter
/// can keep a multi-layer buff up through a partial teardown instead of
/// closing its uptime interval early.
fn is_buff_remove_event(r#type: i32) -> bool {
    r#type == pb::EBuffEventType::Remove as i32 || r#type == pb::EBuffEventType::RemoveLayer as i32
}

/// Double-decodes a buff's `base_id` out of `logic_effect` (issue #267),
/// when present: the entry whose `effect_type == pb::BUFF_EFFECT_ADD_BUFF`
/// has a `raw_data` that is itself a protobuf-encoded `pb::BuffInfo` — see
/// `pb::BUFF_EFFECT_ADD_BUFF`'s doc comment for the confirming evidence.
/// `None` when no such entry is present (common — see
/// `ProtocolEvent::BuffApply`'s doc comment) or its `raw_data` fails to
/// decode.
fn buff_base_id_from_logic_effects(logic_effect: &[pb::BuffEffectLogicInfo]) -> Option<i32> {
    logic_effect
        .iter()
        .find(|le| le.effect_type == pb::BUFF_EFFECT_ADD_BUFF)
        .and_then(|le| pb::BuffInfo::decode(le.raw_data.as_slice()).ok())
        .map(|info| info.base_id)
}

/// Emits `Scene` (issue #293) and `Player`.
///
/// `Scene` comes from `v_data.scene_data.level_map_id` — see
/// `pb::CharSerialize::scene_data`'s doc comment for why this field, once
/// pulled as "disproven" (issue #35), is back. It matters because this is
/// the *only* scene id source a meter attached mid-instance ever sees:
/// `on_enter_scene` fires once, on `ENTER_SCENE` (opcode 3), which the
/// server sends on zone entry and never again — a capture started after
/// that has already happened. `SyncContainerData` is different: it is the
/// full-state push the server sends when a client (re)establishes its
/// session with the world, independent of when the zone was actually
/// entered, so it still carries the current scene on a late attach. A
/// zero id is treated the same as an absent field — `scene_id_from_attrs`'s
/// zero-is-absent guard, mirrored here rather than shared, since this path
/// has no `AttrCollection` to route through.
fn on_sync_container_data(msg: &pb::SyncContainerData, out: &mut Vec<ProtocolEvent>) {
    let Some(v_data) = &msg.v_data else {
        return;
    };
    if let Some(level_map_id) = v_data
        .scene_data
        .as_ref()
        .map(|s| s.level_map_id)
        .filter(|&id| id != 0)
    {
        out.push(ProtocolEvent::Scene { level_map_id });
    }
    let Some(char_base) = &v_data.char_base else {
        return;
    };
    let name = if char_base.name.is_empty() {
        None
    } else {
        Some(char_base.name.clone())
    };
    // An Imagine transform id (issue #37) yields `None` rather than
    // `Some(Class::Unknown)` — see `pb::class_of_profession_id`'s doc
    // comment.
    let class = v_data
        .profession_list
        .as_ref()
        .and_then(|p| pb::class_of_profession_id(p.cur_profession_id));
    let ability_score = if char_base.fight_point > 0 {
        u32::try_from(char_base.fight_point).ok()
    } else {
        None
    };
    out.push(ProtocolEvent::Player(PlayerInfo {
        uid: v_data.char_id,
        name,
        class,
        ability_score,
        // Season data has no confirmed `CharBaseInfo` field (issue #15) —
        // attr-list path only, via `player_info_from_attrs`.
        season_level: None,
        season_strength: None,
        // Same: no confirmed `CharBaseInfo` field for equipped Imagines
        // (issue #33) — attr-list path only.
        skill_ids: Vec::new(),
        // `SyncContainerData` carries no position field — attr-list path
        // only (issue #286).
        position: None,
        target_position: None,
    }));
}

/// `WorldNtf.NotifyReviveUser` (issue #272/#339): emits `ProtocolEvent::
/// Revive` for the actor the notify names. See `pb::NotifyReviveUser`'s
/// doc comment for the wire sourcing. `v_actor_uuid` missing entirely
/// drops the packet — there is no uid to key a revive on — matching this
/// module's non-panicking, nothing-to-report-is-not-an-error convention.
fn on_notify_revive_user(msg: &pb::NotifyReviveUser, now_ms: u64, out: &mut Vec<ProtocolEvent>) {
    let Some(actor_uuid) = msg.v_actor_uuid else {
        return;
    };
    out.push(ProtocolEvent::Revive {
        uid: uid_of(actor_uuid),
        timestamp_ms: now_ms,
    });
}

/// `GrpcTeamNtf.NotifyJoinTeam` (`team_opcode::NOTIFY_JOIN_TEAM`, issue
/// #146): the bulk party-roster push, emitting one `ProtocolEvent::Player`
/// per roster member so party members' names/classes/ability scores arrive
/// without depending on AOI proximity (issue #145 rides AOI and misses
/// distant raid members).
///
/// Every sub-message on the path from `NotifyJoinTeamRequest` down to
/// `TeamBasicData`/`TeamProfessionData`/`TeamUserAttrData` is independently
/// optional and decoded defensively: a bot-like roster entry that carries
/// only `char_id` (no `social_data` at all) must not panic and must not
/// produce a name-less garbage row — it simply yields no event. An event is
/// emitted only when at least one of name / class / ability_score is
/// present.
///
/// `TeamBasicData.level` is decoded onto the pb struct (see its doc
/// comment) but deliberately never read here: `PlayerInfo` has no `level`
/// field and nothing downstream consumes one (issue #146 spec decision 3).
fn on_notify_join_team(msg: &pb::NotifyJoinTeam, out: &mut Vec<ProtocolEvent>) {
    let Some(request) = &msg.v_request else {
        return;
    };
    for member in &request.member_data {
        let social = member.social_data.as_ref();
        let name = social
            .and_then(|s| s.basic_data.as_ref())
            .map(|b| b.name.as_str())
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        // An Imagine transform id (issue #37) yields `None` rather than
        // `Some(Class::Unknown)` — see `pb::class_of_profession_id`'s doc
        // comment. Same conversion `on_sync_container_data` uses above.
        let class = social
            .and_then(|s| s.profession_data.as_ref())
            .and_then(|p| pb::class_of_profession_id(p.profession_id));
        // Same zero-is-absent narrowing `on_sync_container_data` uses for
        // `CharBaseInfo.fight_point` above, just from an `i64` source here.
        let ability_score = social
            .and_then(|s| s.user_attr_data.as_ref())
            .and_then(|u| (u.fight_point > 0).then(|| u32::try_from(u.fight_point).ok()))
            .flatten();
        let season_strength = social
            .and_then(|s| s.user_attr_data.as_ref())
            .and_then(|u| (u.season_strength > 0).then(|| u32::try_from(u.season_strength).ok()))
            .flatten();
        if name.is_none() && class.is_none() && ability_score.is_none() {
            continue;
        }
        // `TeamMemData.char_id` is the uid directly (issue #146: ZDPS's
        // `EntityIdToUuid(charId, EntChar)` is exactly undone by our
        // `uid_of = uuid >> 16`). A member missing it falls back to the copy
        // inside `basicData`; with neither there is no uid to key a player
        // row on, so the member is dropped rather than filed under uid 0.
        let uid = match member.char_id {
            0 => social
                .and_then(|s| s.basic_data.as_ref())
                .map(|b| b.char_id)
                .unwrap_or(0),
            id => id,
        };
        if uid == 0 {
            continue;
        }
        out.push(ProtocolEvent::Player(PlayerInfo {
            uid,
            name,
            class,
            ability_score,
            season_level: None,
            season_strength,
            skill_ids: Vec::new(),
            // `NotifyJoinTeam`'s roster push carries no position field —
            // attr-list path only (issue #286).
            position: None,
            target_position: None,
        }));
    }
}

/// `WorldNtf.EnterScene` (issue #35): the current scene id, decoded from
/// `AttrSceneBasicId` on the scene attr collection nested at field path
/// `1.1` (`EnterScene.info.attrs`, per `pb::EnterScene`'s doc comment).
/// Reuses `SyncNearEntities`' `AttrCollection`/`Attr` shape and
/// `attrs::scene_id_from_attrs`'s zero-is-absent guard — no event is
/// emitted when the collection, the attr, or a nonzero value is missing.
fn on_enter_scene(
    msg: &pb::EnterScene,
    out: &mut Vec<ProtocolEvent>,
    sink: Option<&dyn InspectSink>,
) {
    let Some(attrs) = msg.info.as_ref().and_then(|i| i.attrs.as_ref()) else {
        return;
    };
    if let Some(level_map_id) = scene_id_from_attrs(&attrs.attrs, sink) {
        out.push(ProtocolEvent::Scene { level_map_id });
    }
}

/// `WorldNtf.SyncDungeonData` (issue #139, `opcode::SYNC_DUNGEON_DATA`):
/// the plain-protobuf full dungeon sync. Every one of this build's real
/// capture messages on this opcode was empty, so this only ever emits
/// `ProtocolEvent::DungeonState` when a future capture actually carries a
/// `flow_info` — matching `on_sync_dungeon_dirty_data`'s condition for the
/// blob path below rather than guessing at `target`/`dungeon_var`, which
/// are unmodeled placeholders on this message (see `pb::DungeonSyncData`'s
/// doc comment).
fn on_sync_dungeon_data(msg: &pb::DungeonSyncData, out: &mut Vec<ProtocolEvent>) {
    if let Some(flow_info) = &msg.flow_info {
        out.push(ProtocolEvent::DungeonState {
            state: EDungeonState::from(flow_info.state),
            scene_uuid: (msg.scene_uuid != 0).then_some(msg.scene_uuid),
        });
    }
}

/// `WorldNtf.SyncDungeonDirtyData` (issue #139,
/// `opcode::SYNC_DUNGEON_DIRTY_DATA`): the blob-wrapped dungeon dirty-data
/// channel. `v_data.buffer` is parsed by `blob::parse_dungeon_dirty_data`
/// (not protobuf — see that module's doc comment); a parse failure there
/// is a truncated/malformed blob and is dropped the same way a prost
/// decode failure is everywhere else in this crate.
///
/// `DungeonObjective` events are emitted in ascending `target_id` order
/// (both the hashmap's `add` and `update` sides carry real objective data
/// on this build, so both are applied, `update` last so it wins over a
/// same-message `add` for the same key) — the wire hashmap's own iteration
/// order is not deterministic, so this crate makes the event stream
/// deterministic instead. `TargetData.target_id` is deliberately never
/// used for this: the hashmap key is the authoritative target id (see
/// `blob::TargetData`'s doc comment) — an *update* entry commonly omits
/// `target_id` on the wire entirely. The hashmap's `remove` side carries
/// bare keys and no value at all, so it becomes `DungeonObjectiveRemoved`
/// — sorted for the same determinism reason, and emitted *before* the
/// add/update events so a key this same message removes and re-adds ends
/// up tracked rather than dropped. Dropping removals instead (as this
/// function first did) left the meter's `current_objective_id` pointing
/// at an objective the game had thrown away, which nothing could ever
/// clear (PR #226 review, finding 2).
fn on_sync_dungeon_dirty_data(msg: &pb::SyncDungeonDirtyData, out: &mut Vec<ProtocolEvent>) {
    let Some(v_data) = &msg.v_data else {
        return;
    };
    let Some(data) = blob::parse_dungeon_dirty_data(&v_data.buffer) else {
        log::debug!("bpsr-protocol: SyncDungeonDirtyData blob decode failed");
        return;
    };
    if let Some(flow_info) = data.flow_info {
        out.push(ProtocolEvent::DungeonState {
            state: EDungeonState::from(flow_info.state),
            scene_uuid: data.scene_uuid,
        });
    }
    if let Some(target) = data.target {
        let mut removed = target.remove;
        removed.sort_unstable();
        for target_id in removed {
            out.push(ProtocolEvent::DungeonObjectiveRemoved { target_id });
        }
        let mut objectives = std::collections::BTreeMap::new();
        for (target_id, value) in target.add.into_iter().chain(target.update) {
            objectives.insert(target_id, value);
        }
        for (target_id, value) in objectives {
            out.push(ProtocolEvent::DungeonObjective {
                target_id,
                nums: value.nums,
                complete: value.complete.map(|c| c != 0),
            });
        }
    }
    if let Some(vars) = data.dungeon_var {
        for var in vars {
            out.push(ProtocolEvent::DungeonVar {
                name: var.name,
                value: var.value,
            });
        }
    }
}

/// The crate's public façade: buffers raw stream bytes across pushes,
/// re-runs `split_frames` → `parse_frame` → `decode_notify` on every push,
/// and returns the events decoded from whatever complete frames arrived.
pub struct Decoder {
    tail: Vec<u8>,
    /// Bytes still to be discarded from the stream to get past the body of a
    /// refused over-large frame. Non-zero only while `tail` is empty.
    skip: u64,
    /// Issue #25 slice A diagnostic hook. `None` (the default, via `new`) is
    /// zero-cost: every call site downstream only pays for a null check.
    /// `Arc`, not a borrow, because the decoder — and the sink it reports
    /// to — both outlive a single `push_stream` call across the capture
    /// thread's lifetime.
    sink: Option<Arc<dyn InspectSink>>,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            tail: Vec::new(),
            skip: 0,
            sink: None,
        }
    }

    /// Same as `new`, but with diagnostic observation turned on: every
    /// unrecognized service/method id, raw post-decompression frame, and
    /// unknown attr id reaches `sink` instead of being silently dropped.
    /// Opt-in only — see `crates/app/src/inspect.rs` for how a user turns
    /// this on (issue #25 slice A).
    pub fn with_inspect_sink(sink: Arc<dyn InspectSink>) -> Self {
        Self {
            tail: Vec::new(),
            skip: 0,
            sink: Some(sink),
        }
    }

    /// Bytes currently buffered waiting for a frame to complete. Never
    /// exceeds `MAX_TAIL_LEN`.
    pub fn pending_len(&self) -> usize {
        self.tail.len()
    }

    /// Feeds `bytes` (raw, reassembled TCP payload) in; returns every event
    /// decoded from complete frames. Incomplete trailing bytes are kept for
    /// the next call.
    ///
    /// Desync recovery depends on how badly the stream broke: an unusable
    /// length prefix drops the buffered tail and re-synchronises on whatever
    /// arrives next, while a refused *over-large* frame also skips the rest of
    /// that frame's body (possibly spanning several pushes) so the stream
    /// resumes on a real frame boundary instead of mid-body.
    pub fn push_stream(&mut self, bytes: &[u8], now_ms: u64) -> Vec<ProtocolEvent> {
        let mut bytes = bytes;
        if self.skip > 0 {
            let dropped = self.skip.min(bytes.len() as u64);
            self.skip -= dropped;
            bytes = &bytes[dropped as usize..];
            if bytes.is_empty() {
                return Vec::new();
            }
        }
        self.tail.extend_from_slice(bytes);
        let sink = self.sink.as_deref();
        let mut out = Vec::new();
        let (consumed, desync) = {
            let result = split_frames(&self.tail);
            let mut notifies = Vec::new();
            for f in &result.frames {
                parse_frame(f, 0, &mut notifies, sink, now_ms);
            }
            for n in &notifies {
                decode_notify(n, now_ms, &mut out, sink);
            }
            (result.consumed, result.desync)
        };
        if consumed > 0 {
            self.tail.drain(..consumed);
        }
        match desync {
            None => {}
            Some(Desync::Unrecoverable) => {
                log::debug!("bpsr-protocol: stream desync, dropping buffered tail");
                self.tail.clear();
            }
            Some(Desync::Oversized { total_len }) => {
                // `tail` now starts at the refused frame's length prefix, so
                // whatever is buffered already counts against the skip.
                let buffered = self.tail.len() as u64;
                self.tail.clear();
                self.skip = u64::from(total_len).saturating_sub(buffered);
                log::debug!(
                    "bpsr-protocol: refusing {total_len}-byte frame, skipping {} more bytes",
                    self.skip
                );
            }
        }
        // Backstop: a pending frame is capped at MAX_FRAME_LEN, so the tail
        // cannot legitimately exceed MAX_TAIL_LEN. If it ever does, the stream
        // is not frame-aligned — drop it rather than buffer without limit.
        if self.tail.len() > MAX_TAIL_LEN {
            log::debug!("bpsr-protocol: buffered tail exceeded MAX_TAIL_LEN, resynchronising");
            self.tail.clear();
        }
        out
    }

    /// Drops any buffered tail bytes and pending skip; call on a
    /// server/connection change.
    pub fn reset(&mut self) {
        self.tail.clear();
        self.skip = 0;
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb::{AttrCollection, SkillEffect, SyncNearDeltaInfo};

    const ATTACKER_UUID: i64 = (10i64 << 16) | 640; // player uid 10
    const SUMMONER_UUID: i64 = (20i64 << 16) | 640; // player uid 20
    const TARGET_UUID: i64 = (30i64 << 16) | 64; // monster uid 30

    fn base_damage() -> pb::SyncDamageInfo {
        pb::SyncDamageInfo {
            is_miss: false,
            r#type: EDamageType::Normal as i32,
            type_flag: 0,
            value: 100,
            lucky_value: 0,
            hp_lessen_value: 100,
            attacker_uuid: ATTACKER_UUID,
            owner_id: 1,
            is_dead: false,
            top_summoner_id: 0,
        }
    }

    fn notify_for_damage(dmg: pb::SyncDamageInfo) -> Notify {
        let delta = AoiSyncDelta {
            uuid: TARGET_UUID,
            attrs: None,
            skill_effects: Some(SkillEffect { damages: vec![dmg] }),
            buff_effect: None,
        };
        let msg = SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload,
        }
    }

    fn only_damage(out: Vec<ProtocolEvent>) -> DamageEvent {
        assert_eq!(out.len(), 1);
        match out.into_iter().next().unwrap() {
            ProtocolEvent::Damage(d) => d,
            other => panic!("expected Damage, got {other:?}"),
        }
    }

    #[test]
    fn pet_damage_attributed_to_top_summoner() {
        let dmg = pb::SyncDamageInfo {
            top_summoner_id: SUMMONER_UUID,
            ..base_damage()
        };
        let n = notify_for_damage(dmg);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        let ev = only_damage(out);
        assert_eq!(ev.attacker_uid, uid_of(SUMMONER_UUID));
        assert_eq!(ev.attacker_kind, EntityKind::Player);
    }

    #[test]
    fn owner_id_zero_is_skipped() {
        let dmg = pb::SyncDamageInfo {
            owner_id: 0,
            ..base_damage()
        };
        let n = notify_for_damage(dmg);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(out.is_empty());
    }

    #[test]
    fn crit_bit_set() {
        let dmg = pb::SyncDamageInfo {
            type_flag: 1,
            ..base_damage()
        };
        let n = notify_for_damage(dmg);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(only_damage(out).crit);
    }

    #[test]
    fn crit_bit_clear() {
        let dmg = pb::SyncDamageInfo {
            type_flag: 0,
            ..base_damage()
        };
        let n = notify_for_damage(dmg);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(!only_damage(out).crit);
    }

    #[test]
    fn lucky_value_overrides_value() {
        let dmg = pb::SyncDamageInfo {
            value: 100,
            lucky_value: 250,
            ..base_damage()
        };
        let n = notify_for_damage(dmg);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        let ev = only_damage(out);
        assert_eq!(ev.value, 250);
        assert!(ev.lucky);
    }

    /// A mid-frame max-HP increase can make the server's own `value` go
    /// negative (BPSR-ZDPS `MessageManager.cs` ~1372-1384): the target's HP
    /// pool grew between the hit landing and the packet being built, so the
    /// naive "new HP minus old HP" the server computes underflows. When
    /// that happens the *actual* HP the target lost, `hp_lessen_value`, is
    /// still correct and takes over.
    #[test]
    fn negative_value_falls_back_to_a_positive_hp_lessen() {
        let dmg = pb::SyncDamageInfo {
            value: -500,
            lucky_value: 0,
            hp_lessen_value: 300,
            ..base_damage()
        };
        let n = notify_for_damage(dmg);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(only_damage(out).value, 300);
    }

    /// If `hp_lessen_value` is *also* non-positive there is nothing
    /// trustworthy to fall back to, so the hit reports zero damage rather
    /// than propagating the negative (or another zero) value.
    #[test]
    fn negative_value_and_non_positive_hp_lessen_reports_zero() {
        let dmg = pb::SyncDamageInfo {
            value: -500,
            lucky_value: 0,
            hp_lessen_value: 0,
            ..base_damage()
        };
        let n = notify_for_damage(dmg);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(only_damage(out).value, 0);

        let dmg_negative_lessen = pb::SyncDamageInfo {
            value: -500,
            lucky_value: 0,
            hp_lessen_value: -10,
            ..base_damage()
        };
        let n = notify_for_damage(dmg_negative_lessen);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(only_damage(out).value, 0);
    }

    /// A negative `lucky_value` is picked over `value` by the existing
    /// preference rule (nonzero wins), so it must be guarded the same way.
    #[test]
    fn negative_lucky_value_falls_back_to_hp_lessen_too() {
        let dmg = pb::SyncDamageInfo {
            value: 100,
            lucky_value: -500,
            hp_lessen_value: 300,
            ..base_damage()
        };
        let n = notify_for_damage(dmg);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        let ev = only_damage(out);
        assert_eq!(ev.value, 300);
        assert!(ev.lucky, "the lucky flag is still set on the raw field");
    }

    #[test]
    fn is_dead_flag_survives_decode() {
        // Issue #49: `SyncDamageInfo.is_dead` (tag 17) must reach the
        // decoded `DamageEvent` unchanged — it is the wire signal a
        // per-player death count is built on.
        let dmg = pb::SyncDamageInfo {
            is_dead: true,
            ..base_damage()
        };
        let n = notify_for_damage(dmg);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(only_damage(out).is_dead);
    }

    #[test]
    fn is_dead_false_by_default() {
        let n = notify_for_damage(base_damage());
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(!only_damage(out).is_dead);
    }

    #[test]
    fn heal_type_sets_is_heal() {
        let dmg = pb::SyncDamageInfo {
            r#type: EDamageType::Heal as i32,
            ..base_damage()
        };
        let n = notify_for_damage(dmg);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(only_damage(out).is_heal);
    }

    /// The negative-value guard is for damage-typed hits only. Heal-typed
    /// `SyncDamageInfo` can legitimately carry a negative `value` (lethal /
    /// self-damage heals — see `bpsr_meter::encounter`'s negative/lethal
    /// heal handling), so it must survive decode unchanged even when a
    /// positive `hp_lessen_value` is also present.
    #[test]
    fn heal_type_keeps_negative_value() {
        let dmg = pb::SyncDamageInfo {
            r#type: EDamageType::Heal as i32,
            value: -500,
            hp_lessen_value: 300,
            ..base_damage()
        };
        let n = notify_for_damage(dmg);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        let ev = only_damage(out);
        assert_eq!(ev.value, -500);
        assert!(ev.is_heal);
    }

    #[test]
    fn miss_type_counts_as_hit_with_is_miss_true_even_if_flag_false() {
        // plan §0.6: Miss(1)/is_miss counts as a hit with 0 damage. The
        // server may set r#type == Miss without also setting is_miss.
        let dmg = pb::SyncDamageInfo {
            r#type: EDamageType::Miss as i32,
            is_miss: false,
            ..base_damage()
        };
        let n = notify_for_damage(dmg);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(only_damage(out).is_miss);
    }

    #[test]
    fn unknown_opcode_produces_zero_events() {
        let n = Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: 0x0000_0099,
            payload: Vec::new(),
        };
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(out.is_empty());
    }

    #[test]
    fn attr_carrying_entity_emits_player_event() {
        let attrs = AttrCollection {
            uuid: ATTACKER_UUID,
            attrs: vec![pb::Attr {
                id: crate::attrs::attr_id::NAME,
                raw_data: vec![0xFF, b'A', b'l', b'i'],
            }],
        };
        let delta = AoiSyncDelta {
            uuid: ATTACKER_UUID,
            attrs: Some(attrs),
            skill_effects: None,
            buff_effect: None,
        };
        let msg = SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let n = Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload,
        };
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ProtocolEvent::Player(p) => {
                assert_eq!(p.uid, uid_of(ATTACKER_UUID));
                assert_eq!(p.name.as_deref(), Some("Ali"));
            }
            other => panic!("expected Player, got {other:?}"),
        }
    }

    /// Issue #245: `AttrSkillId` on a player's delta is how BPSR reports a
    /// cast — there is no dedicated cast notify. See
    /// `attrs::attr_id::SKILL_ID` for the evidence.
    fn skill_attr_notify(uuid: i64, skill_id: u64) -> Notify {
        let mut raw_data = Vec::new();
        prost::encoding::encode_varint(skill_id, &mut raw_data);
        let delta = AoiSyncDelta {
            uuid,
            attrs: Some(AttrCollection {
                uuid,
                attrs: vec![pb::Attr {
                    id: crate::attrs::attr_id::SKILL_ID,
                    raw_data,
                }],
            }),
            skill_effects: None,
            buff_effect: None,
        };
        let msg = SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload,
        }
    }

    #[test]
    fn a_skill_id_attr_on_a_player_emits_a_cast() {
        let mut out = Vec::new();
        decode_notify(
            &skill_attr_notify(ATTACKER_UUID, 1550),
            4242,
            &mut out,
            None,
        );
        let cast = out
            .iter()
            .find_map(|ev| match ev {
                ProtocolEvent::Cast(c) => Some(c),
                _ => None,
            })
            .expect("expected a Cast event");
        assert_eq!(cast.caster_uid, uid_of(ATTACKER_UUID));
        assert_eq!(cast.skill_id, 1550);
        assert_eq!(cast.timestamp_ms, 4242);
    }

    /// Issue #287: the skill-cast metadata cluster (`SKILL_LEVEL` /
    /// `SKILL_BEGIN_TIME` here) rides the same delta as `SKILL_ID` and must
    /// land on the emitted `CastEvent`'s new fields.
    #[test]
    fn a_skill_id_attr_with_metadata_emits_a_cast_carrying_it() {
        let mut level_raw = Vec::new();
        prost::encoding::encode_varint(30u64, &mut level_raw);
        let mut begin_time_raw = Vec::new();
        prost::encoding::encode_varint(1_787_022_297_550u64, &mut begin_time_raw);
        let mut skill_id_raw = Vec::new();
        prost::encoding::encode_varint(1550u64, &mut skill_id_raw);

        let attrs = AttrCollection {
            uuid: ATTACKER_UUID,
            attrs: vec![
                pb::Attr {
                    id: crate::attrs::attr_id::SKILL_ID,
                    raw_data: skill_id_raw,
                },
                pb::Attr {
                    id: crate::attrs::attr_id::SKILL_LEVEL,
                    raw_data: level_raw,
                },
                pb::Attr {
                    id: crate::attrs::attr_id::SKILL_BEGIN_TIME,
                    raw_data: begin_time_raw,
                },
            ],
        };
        let delta = AoiSyncDelta {
            uuid: ATTACKER_UUID,
            attrs: Some(attrs),
            skill_effects: None,
            buff_effect: None,
        };
        let msg = SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let n = Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload,
        };

        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        let cast = out
            .iter()
            .find_map(|ev| match ev {
                ProtocolEvent::Cast(c) => Some(c),
                _ => None,
            })
            .expect("expected a Cast event");
        assert_eq!(cast.skill_level, Some(30));
        assert_eq!(cast.skill_begin_time_ms, Some(1_787_022_297_550));
        assert_eq!(cast.skill_stage, None);
        assert_eq!(cast.skill_stage_num, None);
        assert_eq!(cast.skill_uuid, None);
    }

    /// The attr rides every entity's deltas, monsters included, but a
    /// monster has no per-skill breakdown to count into — see
    /// `ProtocolEvent::Cast`.
    #[test]
    fn a_skill_id_attr_on_a_monster_emits_no_cast() {
        let mut out = Vec::new();
        decode_notify(&skill_attr_notify(TARGET_UUID, 1550), 0, &mut out, None);
        assert!(
            !out.iter().any(|ev| matches!(ev, ProtocolEvent::Cast(_))),
            "a monster's casts are not tracked: {out:?}"
        );
    }

    /// `0` is the attr channel's "no skill" value, not a skill whose id
    /// happens to be zero.
    #[test]
    fn a_zero_skill_id_attr_emits_no_cast() {
        let mut out = Vec::new();
        decode_notify(&skill_attr_notify(ATTACKER_UUID, 0), 0, &mut out, None);
        assert!(
            !out.iter().any(|ev| matches!(ev, ProtocolEvent::Cast(_))),
            "zero means no skill: {out:?}"
        );
    }

    // -- issue #267: buff apply/remove events ----------------------------

    fn buff_notify(uuid: i64, effects: Vec<pb::BuffEffect>) -> Notify {
        let delta = AoiSyncDelta {
            uuid,
            attrs: None,
            skill_effects: None,
            buff_effect: Some(pb::BuffEffectSync {
                uuid: 0,
                buff_effects: effects,
            }),
        };
        let msg = SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload,
        }
    }

    /// Encodes a `BuffInfo` carrying only `base_id`, for use as a
    /// `BuffEffectLogicInfo.raw_data` — the double-encoding issue #267
    /// documents.
    fn buff_info_raw_data(base_id: i32) -> Vec<u8> {
        let info = pb::BuffInfo {
            base_id,
            ..Default::default()
        };
        let mut raw = Vec::new();
        info.encode(&mut raw).unwrap();
        raw
    }

    fn only_buff_apply(out: &[ProtocolEvent]) -> (i64, i32, Option<i32>, u64) {
        match out {
            [
                ProtocolEvent::BuffApply {
                    host_uid,
                    buff_uuid,
                    base_id,
                    adds_layer: _,
                    timestamp_ms,
                },
            ] => (*host_uid, *buff_uuid, *base_id, *timestamp_ms),
            other => panic!("expected exactly one BuffApply, got {other:?}"),
        }
    }

    #[test]
    fn add_to_event_with_double_encoded_base_id_emits_buff_apply_with_base_id() {
        let effect = pb::BuffEffect {
            r#type: pb::EBuffEventType::AddTo as i32,
            buff_uuid: 417,
            host_uuid: ATTACKER_UUID,
            trigger_time: 999,
            logic_effect: vec![pb::BuffEffectLogicInfo {
                effect_type: pb::BUFF_EFFECT_ADD_BUFF,
                raw_data: buff_info_raw_data(3_210_031),
                is_loop: false,
            }],
        };
        let mut out = Vec::new();
        decode_notify(
            &buff_notify(ATTACKER_UUID, vec![effect]),
            4242,
            &mut out,
            None,
        );
        let (host_uid, buff_uuid, base_id, timestamp_ms) = only_buff_apply(&out);
        assert_eq!(host_uid, uid_of(ATTACKER_UUID));
        assert_eq!(buff_uuid, 417);
        assert_eq!(base_id, Some(3_210_031));
        assert_eq!(timestamp_ms, 4242);
    }

    /// Roughly half of this build's real `AddTo` events carry no
    /// double-encoded `BuffInfo` at all (see `pb::AoiSyncDelta::buff_effect`'s
    /// doc comment) — `base_id` must be `None`, not a decode error, when
    /// `logic_effect` is empty.
    #[test]
    fn add_to_event_with_no_logic_effect_emits_buff_apply_with_no_base_id() {
        let effect = pb::BuffEffect {
            r#type: pb::EBuffEventType::AddTo as i32,
            buff_uuid: 1,
            host_uuid: ATTACKER_UUID,
            trigger_time: 0,
            logic_effect: Vec::new(),
        };
        let mut out = Vec::new();
        decode_notify(&buff_notify(ATTACKER_UUID, vec![effect]), 0, &mut out, None);
        let (_, _, base_id, _) = only_buff_apply(&out);
        assert_eq!(base_id, None);
    }

    #[test]
    fn replace_and_stack_layer_events_are_treated_as_apply() {
        // Only `StackLayer` grows a stacking buff's layer count; `Replace`
        // is a refresh of the instance that is already up.
        for (event_type, adds_layer) in [
            (pb::EBuffEventType::Replace, false),
            (pb::EBuffEventType::StackLayer, true),
        ] {
            let effect = pb::BuffEffect {
                r#type: event_type as i32,
                buff_uuid: 7,
                host_uuid: ATTACKER_UUID,
                trigger_time: 0,
                logic_effect: Vec::new(),
            };
            let mut out = Vec::new();
            decode_notify(&buff_notify(ATTACKER_UUID, vec![effect]), 0, &mut out, None);
            assert!(
                matches!(
                    out.as_slice(),
                    [ProtocolEvent::BuffApply { adds_layer: a, .. }] if *a == adds_layer
                ),
                "{event_type:?} should emit BuffApply with adds_layer={adds_layer}, got {out:?}"
            );
        }
    }

    #[test]
    fn remove_and_remove_layer_events_emit_buff_remove() {
        // `removes_layer` separates the partial teardown from the full one.
        for (event_type, removes_layer) in [
            (pb::EBuffEventType::Remove, false),
            (pb::EBuffEventType::RemoveLayer, true),
        ] {
            let effect = pb::BuffEffect {
                r#type: event_type as i32,
                buff_uuid: 417,
                host_uuid: ATTACKER_UUID,
                trigger_time: 5000,
                logic_effect: Vec::new(),
            };
            let mut out = Vec::new();
            decode_notify(
                &buff_notify(ATTACKER_UUID, vec![effect]),
                5000,
                &mut out,
                None,
            );
            match out.as_slice() {
                [
                    ProtocolEvent::BuffRemove {
                        host_uid,
                        buff_uuid,
                        removes_layer: layer,
                        timestamp_ms,
                    },
                ] => {
                    assert_eq!(*host_uid, uid_of(ATTACKER_UUID));
                    assert_eq!(*buff_uuid, 417);
                    assert_eq!(*layer, removes_layer, "{event_type:?}");
                    assert_eq!(*timestamp_ms, 5000);
                }
                other => panic!("{event_type:?} should emit BuffRemove, got {other:?}"),
            }
        }
    }

    /// `Timer` (periodic tick) is neither an apply nor a remove signal —
    /// see `is_buff_apply_event`/`is_buff_remove_event`.
    #[test]
    fn timer_event_emits_no_buff_event() {
        let effect = pb::BuffEffect {
            r#type: pb::EBuffEventType::Timer as i32,
            buff_uuid: 1,
            host_uuid: ATTACKER_UUID,
            trigger_time: 0,
            logic_effect: Vec::new(),
        };
        let mut out = Vec::new();
        decode_notify(&buff_notify(ATTACKER_UUID, vec![effect]), 0, &mut out, None);
        assert!(out.is_empty(), "Timer should emit nothing: {out:?}");
    }

    /// Buffs ride every entity's deltas, monsters included, but
    /// `bpsr_meter` keeps no per-monster buff breakdown — mirrors
    /// `a_skill_id_attr_on_a_monster_emits_no_cast`.
    #[test]
    fn buff_event_on_a_monster_emits_nothing() {
        let effect = pb::BuffEffect {
            r#type: pb::EBuffEventType::AddTo as i32,
            buff_uuid: 1,
            host_uuid: TARGET_UUID,
            trigger_time: 0,
            logic_effect: Vec::new(),
        };
        let mut out = Vec::new();
        decode_notify(&buff_notify(TARGET_UUID, vec![effect]), 0, &mut out, None);
        assert!(out.is_empty(), "a monster's buffs are not tracked: {out:?}");
    }

    /// Regression fixture, values taken verbatim from a real capture
    /// (`inspect/dump-2976.jsonl`, per `docs/packet-inspection.md`'s
    /// "Recording a result"): one `AddTo` event whose double-encoded
    /// `BuffInfo` carries `base_id = 21_404`, `buff_uuid = 1247`, on player
    /// uuid `51_373_802_112` (`uuid & 0xFFFF == 640`, the player entity-type
    /// bits — confirmed against `wire::player_uuid`'s own layout, unlike
    /// the otherwise-similar sample this project's issue #267 investigation
    /// first tried, whose uuid turned out to be a monster's).
    #[test]
    fn real_capture_add_to_event_decodes_expected_base_id() {
        const HOST_UUID: i64 = 51_373_802_112;
        let effect = pb::BuffEffect {
            r#type: pb::EBuffEventType::AddTo as i32,
            buff_uuid: 1247,
            host_uuid: HOST_UUID,
            trigger_time: 1_787_022_226_743,
            logic_effect: vec![pb::BuffEffectLogicInfo {
                effect_type: pb::BUFF_EFFECT_ADD_BUFF,
                raw_data: buff_info_raw_data(21_404),
                is_loop: false,
            }],
        };
        let mut out = Vec::new();
        decode_notify(
            &buff_notify(HOST_UUID, vec![effect]),
            1_787_022_226_743,
            &mut out,
            None,
        );
        let (host_uid, buff_uuid, base_id, _) = only_buff_apply(&out);
        assert_eq!(host_uid, uid_of(HOST_UUID));
        assert_eq!(buff_uuid, 1247);
        assert_eq!(base_id, Some(21_404));
    }

    fn to_me_notify(outer_uuid: i64, base_uuid: i64) -> Notify {
        let attrs = AttrCollection {
            uuid: 0,
            attrs: vec![pb::Attr {
                id: crate::attrs::attr_id::NAME,
                raw_data: vec![0xFF, b'A', b'l', b'i'],
            }],
        };
        let msg = pb::SyncToMeDeltaInfo {
            delta_info: Some(pb::AoiSyncToMeDelta {
                base_delta: Some(AoiSyncDelta {
                    uuid: base_uuid,
                    attrs: Some(attrs),
                    skill_effects: None,
                    buff_effect: None,
                }),
                uuid: outer_uuid,
            }),
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_TO_ME_DELTA_INFO,
            payload,
        }
    }

    fn only_player(out: Vec<ProtocolEvent>) -> PlayerInfo {
        assert_eq!(out.len(), 1);
        match out.into_iter().next().unwrap() {
            ProtocolEvent::Player(p) => p,
            other => panic!("expected Player, got {other:?}"),
        }
    }

    #[test]
    fn to_me_delta_takes_identity_from_the_outer_uuid() {
        // The "to-me" wrapper carries the entity uuid; base_delta.uuid is 0.
        let n = to_me_notify(ATTACKER_UUID, 0);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        let p = only_player(out);
        assert_eq!(p.uid, uid_of(ATTACKER_UUID));
        assert_eq!(p.name.as_deref(), Some("Ali"));
    }

    #[test]
    fn to_me_delta_falls_back_to_base_delta_uuid() {
        let n = to_me_notify(0, ATTACKER_UUID);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(only_player(out).uid, uid_of(ATTACKER_UUID));
    }

    #[test]
    fn push_stream_round_trips_through_decoder() {
        let dmg = base_damage();
        let n = notify_for_damage(dmg);
        // Build a minimal outer frame around the Notify body by hand: this
        // exercises split_frames + parse_frame + decode_notify together.
        let mut body = Vec::new();
        body.extend_from_slice(&crate::frame::SERVICE_UUID.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&n.method_id.to_be_bytes());
        body.extend_from_slice(&n.payload);
        let mut frame = Vec::new();
        let total_len = 4 + 2 + body.len() as u32;
        frame.extend_from_slice(&total_len.to_be_bytes());
        frame.extend_from_slice(&2u16.to_be_bytes()); // Notify, uncompressed
        frame.extend_from_slice(&body);

        let mut decoder = Decoder::new();
        let out = decoder.push_stream(&frame, 0);
        assert_eq!(only_damage(out).skill_id, 1);
    }

    // -- InspectSink observation (issue #25 slice A) -----------------------

    /// `(service_uuid, method_id, payload, payload_decoded, now_ms)`.
    type RecordedNotify = (u64, u32, Vec<u8>, bool, u64);
    /// `(uid, attr_id, raw, known)`.
    type RecordedAttr = (i64, i32, Vec<u8>, bool);

    struct RecordingSink {
        notifies: std::sync::Mutex<Vec<RecordedNotify>>,
        attrs: std::sync::Mutex<Vec<RecordedAttr>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                notifies: std::sync::Mutex::new(Vec::new()),
                attrs: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl crate::inspect::InspectSink for RecordingSink {
        fn on_notify(
            &self,
            service_uuid: u64,
            method_id: u32,
            payload: &[u8],
            payload_decoded: bool,
            now_ms: u64,
        ) {
            self.notifies.lock().unwrap().push((
                service_uuid,
                method_id,
                payload.to_vec(),
                payload_decoded,
                now_ms,
            ));
        }

        fn on_attr(&self, uid: i64, attr_id: i32, raw: &[u8], known: bool) {
            self.attrs
                .lock()
                .unwrap()
                .push((uid, attr_id, raw.to_vec(), known));
        }
    }

    /// `decode_notify` threads its sink down into the entity attr walk, so
    /// an unknown attr id on a player entity reaches it end to end, tagged
    /// `known = false`.
    #[test]
    fn decode_notify_forwards_unknown_attr_ids_to_the_sink() {
        let attrs = AttrCollection {
            uuid: ATTACKER_UUID,
            attrs: vec![pb::Attr {
                id: 0x7777,
                raw_data: vec![0x01],
            }],
        };
        let delta = AoiSyncDelta {
            uuid: ATTACKER_UUID,
            attrs: Some(attrs),
            skill_effects: None,
            buff_effect: None,
        };
        let msg = SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let n = Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload,
        };
        let sink = RecordingSink::new();
        let mut out = Vec::new();

        decode_notify(&n, 0, &mut out, Some(&sink));

        assert_eq!(
            *sink.attrs.lock().unwrap(),
            vec![(uid_of(ATTACKER_UUID), 0x7777, vec![0x01], false)]
        );
    }

    /// The same path, for an id we do decode (`FIGHT_POINT`): the sink
    /// still sees it, tagged `known = true` — this is what lets an operator
    /// diff a known id like ability score across an in-game change (issue
    /// #25's control-run procedure), not just discover new ones.
    #[test]
    fn decode_notify_forwards_known_attr_ids_to_the_sink_as_known() {
        let mut raw_data = Vec::new();
        prost::encoding::encode_varint(1_000_000u64, &mut raw_data);
        let attrs = AttrCollection {
            uuid: ATTACKER_UUID,
            attrs: vec![pb::Attr {
                id: crate::attrs::attr_id::FIGHT_POINT,
                raw_data,
            }],
        };
        let delta = AoiSyncDelta {
            uuid: ATTACKER_UUID,
            attrs: Some(attrs),
            skill_effects: None,
            buff_effect: None,
        };
        let msg = SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let n = Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload,
        };
        let sink = RecordingSink::new();
        let mut out = Vec::new();

        decode_notify(&n, 0, &mut out, Some(&sink));

        let recorded = sink.attrs.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, uid_of(ATTACKER_UUID));
        assert_eq!(recorded[0].1, crate::attrs::attr_id::FIGHT_POINT);
        assert!(recorded[0].3, "FIGHT_POINT must be reported as known");
    }

    fn container_notify(v_data: pb::CharSerialize) -> Notify {
        let msg = pb::SyncContainerData {
            v_data: Some(v_data),
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_CONTAINER_DATA,
            payload,
        }
    }

    // -- Imagine profession ids (issue #37) ---------------------------------

    #[test]
    fn container_data_imagine_profession_id_yields_no_class() {
        let n = container_notify(pb::CharSerialize {
            char_id: 8,
            char_base: Some(pb::CharBaseInfo {
                char_id: 8,
                name: "Ari".to_string(),
                fight_point: 0,
            }),
            scene_data: None,
            profession_list: Some(pb::ProfessionList {
                cur_profession_id: 8, // Dorothy (Imagine)
            }),
        });
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ProtocolEvent::Player(p) => assert_eq!(p.class, None),
            other => panic!("expected Player, got {other:?}"),
        }
    }

    // -- SyncContainerData.scene_data (issue #293: mid-instance attach) -----

    #[test]
    fn container_data_scene_data_emits_a_scene_event() {
        // A meter attached mid-instance never sees `ENTER_SCENE` (it fires
        // once, on zone entry, before the meter existed) — this is the
        // full-state push it sees instead.
        let n = container_notify(pb::CharSerialize {
            char_id: 0,
            char_base: None,
            scene_data: Some(pb::SceneData { level_map_id: 8 }),
            profession_list: None,
        });
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out, vec![ProtocolEvent::Scene { level_map_id: 8 }]);
    }

    #[test]
    fn container_data_scene_data_zero_level_map_id_is_treated_as_absent() {
        let n = container_notify(pb::CharSerialize {
            char_id: 0,
            char_base: None,
            scene_data: Some(pb::SceneData { level_map_id: 0 }),
            profession_list: None,
        });
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(out.is_empty());
    }

    #[test]
    fn container_data_without_scene_data_emits_no_scene_event() {
        let n = container_notify(pb::CharSerialize {
            char_id: 8,
            char_base: Some(pb::CharBaseInfo {
                char_id: 8,
                name: "Ari".to_string(),
                fight_point: 0,
            }),
            scene_data: None,
            profession_list: None,
        });
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(
            out,
            vec![ProtocolEvent::Player(PlayerInfo {
                uid: 8,
                name: Some("Ari".to_string()),
                class: None,
                ability_score: None,
                season_level: None,
                season_strength: None,
                skill_ids: Vec::new(),
                position: None,
                target_position: None,
            })]
        );
    }

    #[test]
    fn container_data_scene_data_and_char_base_emits_scene_then_player() {
        let n = container_notify(pb::CharSerialize {
            char_id: 8,
            char_base: Some(pb::CharBaseInfo {
                char_id: 8,
                name: "Ari".to_string(),
                fight_point: 0,
            }),
            scene_data: Some(pb::SceneData {
                level_map_id: 40001,
            }),
            profession_list: None,
        });
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0],
            ProtocolEvent::Scene {
                level_map_id: 40001
            }
        );
        match &out[1] {
            ProtocolEvent::Player(p) => assert_eq!(p.uid, 8),
            other => panic!("expected Player, got {other:?}"),
        }
    }

    // -- NotifyJoinTeam party roster (issue #146) ----------------------

    fn team_notify_for(request: pb::NotifyJoinTeamRequest) -> Notify {
        let msg = pb::NotifyJoinTeam {
            v_request: Some(request),
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        Notify {
            service_uuid: crate::frame::TEAM_NTF_SERVICE_UUID,
            method_id: team_opcode::NOTIFY_JOIN_TEAM,
            payload,
        }
    }

    fn full_team_member(
        char_id: i64,
        name: &str,
        profession_id: i32,
        fight_point: i64,
    ) -> pb::TeamMemData {
        pb::TeamMemData {
            char_id,
            scene_id: 0,
            group_id: 0,
            social_data: Some(pb::TeamMemberSocialData {
                basic_data: Some(pb::TeamBasicData {
                    char_id,
                    name: name.to_string(),
                    level: 0,
                }),
                profession_data: Some(pb::TeamProfessionData { profession_id }),
                user_attr_data: Some(pb::TeamUserAttrData {
                    fight_point,
                    season_strength: 0,
                }),
            }),
        }
    }

    #[test]
    fn notify_join_team_full_roster_yields_one_player_per_member() {
        let request = pb::NotifyJoinTeamRequest {
            base_info: Some(pb::TeamBaseInfo {}),
            member_data: vec![
                full_team_member(101, "Ari", 1, 12_345), // Stormblade
                full_team_member(102, "Zed", 2, 22_222), // FrostMage
                full_team_member(103, "Yin", 3, 33_333), // TwinStriker
            ],
        };
        let n = team_notify_for(request);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 3);
        let expected = [
            (101, "Ari", pb::Class::Stormblade, 12_345u32),
            (102, "Zed", pb::Class::FrostMage, 22_222u32),
            (103, "Yin", pb::Class::TwinStriker, 33_333u32),
        ];
        for (event, (uid, name, class, ability_score)) in out.iter().zip(expected) {
            match event {
                ProtocolEvent::Player(p) => {
                    assert_eq!(p.uid, uid);
                    assert_eq!(p.name.as_deref(), Some(name));
                    assert_eq!(p.class, Some(class));
                    assert_eq!(p.ability_score, Some(ability_score));
                }
                other => panic!("expected Player, got {other:?}"),
            }
        }
    }

    /// A bot-like roster entry carrying only `char_id` (no `social_data` at
    /// all — "bots are missing a lot of fields") must not panic and must
    /// not produce a name-less garbage row.
    #[test]
    fn notify_join_team_member_with_only_char_id_yields_no_event_and_no_panic() {
        let request = pb::NotifyJoinTeamRequest {
            base_info: Some(pb::TeamBaseInfo {}),
            member_data: vec![pb::TeamMemData {
                char_id: 999,
                scene_id: 0,
                group_id: 0,
                social_data: None,
            }],
        };
        let n = team_notify_for(request);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(out.is_empty());
    }

    /// A member with a name but no profession/attr data still yields a
    /// `Player` — with `class: None` and `ability_score: None`, not a
    /// dropped event.
    #[test]
    fn notify_join_team_member_with_name_only_yields_player_with_no_class() {
        let request = pb::NotifyJoinTeamRequest {
            base_info: Some(pb::TeamBaseInfo {}),
            member_data: vec![pb::TeamMemData {
                char_id: 55,
                scene_id: 0,
                group_id: 0,
                social_data: Some(pb::TeamMemberSocialData {
                    basic_data: Some(pb::TeamBasicData {
                        char_id: 55,
                        name: "Nameonly".to_string(),
                        level: 0,
                    }),
                    profession_data: None,
                    user_attr_data: None,
                }),
            }],
        };
        let n = team_notify_for(request);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ProtocolEvent::Player(p) => {
                assert_eq!(p.uid, 55);
                assert_eq!(p.name.as_deref(), Some("Nameonly"));
                assert_eq!(p.class, None);
                assert_eq!(p.ability_score, None);
            }
            other => panic!("expected Player, got {other:?}"),
        }
    }

    /// A member with a usable name but no uid anywhere — `TeamMemData.char_id`
    /// zero and the `TeamBasicData.char_id` fallback zero too — is dropped
    /// rather than filed under uid 0, where it would collide with every other
    /// uid-less member. A valid member sharing the same roster still lands.
    #[test]
    fn notify_join_team_member_with_name_but_zero_char_id_yields_no_event() {
        let request = pb::NotifyJoinTeamRequest {
            base_info: Some(pb::TeamBaseInfo {}),
            member_data: vec![
                pb::TeamMemData {
                    char_id: 0,
                    scene_id: 0,
                    group_id: 0,
                    social_data: Some(pb::TeamMemberSocialData {
                        basic_data: Some(pb::TeamBasicData {
                            char_id: 0,
                            name: "Uidless".to_string(),
                            level: 0,
                        }),
                        profession_data: Some(pb::TeamProfessionData { profession_id: 1 }),
                        user_attr_data: Some(pb::TeamUserAttrData {
                            fight_point: 4_444,
                            season_strength: 0,
                        }),
                    }),
                },
                full_team_member(101, "Ari", 1, 12_345),
            ],
        };
        let n = team_notify_for(request);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 1, "the uid-less member must yield no event");
        match &out[0] {
            ProtocolEvent::Player(p) => {
                assert_eq!(p.uid, 101);
                assert_eq!(p.name.as_deref(), Some("Ari"));
            }
            other => panic!("expected Player, got {other:?}"),
        }
    }

    /// The regression this slice exists to prevent: `NotifyJoinTeam`'s
    /// method id (`0x3`) collides with `opcode::ENTER_SCENE` on the main
    /// service. A `NotifyJoinTeam` payload delivered on the MAIN service
    /// uuid must still be routed to the `EnterScene` decoder (and, since
    /// the bytes don't match that shape meaningfully, fail cleanly — no
    /// `Player` event materializes from team-roster bytes misread as a
    /// scene).
    #[test]
    fn notify_join_team_payload_on_main_service_is_not_decoded_as_team_roster() {
        let request = pb::NotifyJoinTeamRequest {
            base_info: Some(pb::TeamBaseInfo {}),
            member_data: vec![full_team_member(101, "Ari", 1, 12_345)],
        };
        let msg = pb::NotifyJoinTeam {
            v_request: Some(request),
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let n = Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: 0x3, // opcode::ENTER_SCENE on the main service
            payload,
        };
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(
            out.iter().all(|e| !matches!(e, ProtocolEvent::Player(_))),
            "a NotifyJoinTeam payload on the main service must never surface as a Player event: {out:?}"
        );
    }

    /// The mirror image: an `EnterScene` payload delivered on the TEAM
    /// service uuid at method `0x3` must NOT be treated as a scene change —
    /// dispatch must key off `service_uuid` too, not `method_id` alone.
    #[test]
    fn enter_scene_payload_on_team_service_is_not_treated_as_scene_change() {
        let msg = pb::EnterScene {
            info: Some(pb::EnterSceneInfo {
                attrs: Some(AttrCollection {
                    uuid: 0,
                    attrs: vec![pb::Attr {
                        id: 341, // AttrSceneBasicId
                        raw_data: vec![0x08],
                    }],
                }),
            }),
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let n = Notify {
            service_uuid: crate::frame::TEAM_NTF_SERVICE_UUID,
            method_id: 0x3, // team_opcode::NOTIFY_JOIN_TEAM on the team service
            payload,
        };
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(
            !out.iter().any(|e| matches!(e, ProtocolEvent::Scene { .. })),
            "an EnterScene payload on the team service must never surface as a Scene event: {out:?}"
        );
    }

    // -- Scene, via EnterScene (issue #35) -----------------------------

    /// Builds an `EnterScene` notify carrying exactly `attrs` on the scene
    /// attr collection at field path `1.1`.
    fn enter_scene_notify(attrs: Vec<pb::Attr>) -> Notify {
        let msg = pb::EnterScene {
            info: Some(pb::EnterSceneInfo {
                attrs: Some(AttrCollection { uuid: 0, attrs }),
            }),
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::ENTER_SCENE,
            payload,
        }
    }

    /// The regression fixture: a real capture's `EnterScene` attr set
    /// (`AttrSceneBasicId`/`AttrSceneChannel`/`AttrSceneUuid`/
    /// `AttrSceneName`, ids 341/343/342/340), byte-for-byte as dumped
    /// (issue #35). Only `AttrSceneBasicId`'s value (`8`, "Asterleeds" per
    /// `tables::scene_name`) surfaces as a `Scene` event; the others —
    /// including the string-valued `AttrSceneName` — must not corrupt the
    /// result or panic.
    #[test]
    fn enter_scene_attr_set_emits_a_single_scene_event() {
        let attrs = vec![
            pb::Attr {
                id: 341, // AttrSceneBasicId
                raw_data: vec![0x08],
            },
            pb::Attr {
                id: 343, // AttrSceneChannel
                raw_data: vec![0x01],
            },
            pb::Attr {
                id: 342, // AttrSceneUuid
                raw_data: vec![0x80, 0x80, 0x80, 0x80, 0x80, 0x81, 0x40],
            },
            pb::Attr {
                id: 340, // AttrSceneName, length-prefixed UTF-8, not a varint
                raw_data: vec![
                    0x0f, 0xe9, 0x98, 0xbf, 0xe6, 0x96, 0xaf, 0xe7, 0x89, 0xb9, 0xe9, 0x87, 0x8c,
                    0xe6, 0x96, 0xaf,
                ],
            },
        ];
        let n = enter_scene_notify(attrs);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out, vec![ProtocolEvent::Scene { level_map_id: 8 }]);
    }

    #[test]
    fn enter_scene_without_scene_basic_id_emits_no_scene_event() {
        let attrs = vec![pb::Attr {
            id: 343, // AttrSceneChannel only — no AttrSceneBasicId
            raw_data: vec![0x01],
        }];
        let n = enter_scene_notify(attrs);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(out.is_empty());
    }

    #[test]
    fn enter_scene_zero_level_map_id_is_treated_as_absent() {
        // level_map_id == 0 is proto3's unset-scalar wire value, indistinguishable
        // from "not populated" — must not surface as a real scene id.
        let attrs = vec![pb::Attr {
            id: 341, // AttrSceneBasicId
            raw_data: vec![0x00],
        }];
        let n = enter_scene_notify(attrs);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(out.is_empty());
    }

    /// Two `AttrSceneBasicId` attrs in one `EnterScene` attr collection: the
    /// implementation doesn't dedupe by id, it just keeps overwriting as it
    /// walks the list, so whichever valid value comes last in `attrs` wins.
    #[test]
    fn enter_scene_duplicate_scene_basic_id_attrs_keeps_the_last_valid_value() {
        let attrs = vec![
            pb::Attr {
                id: 341, // AttrSceneBasicId
                raw_data: vec![0x05],
            },
            pb::Attr {
                id: 341, // AttrSceneBasicId, again — this one should win
                raw_data: vec![0x08],
            },
        ];
        let n = enter_scene_notify(attrs);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out, vec![ProtocolEvent::Scene { level_map_id: 8 }]);
    }

    #[test]
    fn enter_scene_malformed_scene_basic_id_varint_emits_no_scene_event() {
        // A truncated varint: the continuation bit is set but there's no
        // following byte, so `decode_varint_u32` returns `None`.
        let attrs = vec![pb::Attr {
            id: 341, // AttrSceneBasicId
            raw_data: vec![0x80],
        }];
        let n = enter_scene_notify(attrs);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(out.is_empty());
    }

    /// End-to-end through `Decoder::with_inspect_sink` + `push_stream`: a
    /// Notify on an unrecognized service uuid is observed via the sink and
    /// still produces no `ProtocolEvent`s.
    #[test]
    fn decoder_with_inspect_sink_observes_an_unrecognized_service_end_to_end() {
        let other_service = crate::frame::SERVICE_UUID.wrapping_add(1);
        let mut body = Vec::new();
        body.extend_from_slice(&other_service.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&0x42u32.to_be_bytes());
        body.extend_from_slice(b"hello");
        let mut frame = Vec::new();
        let total_len = 4 + 2 + body.len() as u32;
        frame.extend_from_slice(&total_len.to_be_bytes());
        frame.extend_from_slice(&2u16.to_be_bytes()); // Notify, uncompressed
        frame.extend_from_slice(&body);

        let sink = Arc::new(RecordingSink::new());
        let mut decoder = Decoder::with_inspect_sink(sink.clone());
        let out = decoder.push_stream(&frame, 999);

        assert!(out.is_empty());
        assert_eq!(
            *sink.notifies.lock().unwrap(),
            vec![(other_service, 0x42, b"hello".to_vec(), true, 999)]
        );
    }

    // -- Dungeon state / objectives (issue #139) ----------------------------
    //
    // Real `Notify.payload` hex for `opcode::SYNC_DUNGEON_DIRTY_DATA`
    // (`0x18`), captured on this build — see
    // docs/specs/2026-08-23-issue-139-dungeon-state-spec.md. Each is the
    // full protobuf-wrapped payload `decode_notify` actually receives, not
    // just the inner blob (that's covered in isolation by `blob::tests`).

    /// `FlowInfo.State = 4 (End)`.
    const DUNGEON_FLOW_INFO_END_HEX: &str = "0a9b010a9801feffffffefbeadde80000000efbeadde02000000efbeaddefeffffffefbeadde30000000efbeadde01000000efbeadde04000000efbeadde05000000efbeaddedecb836aefbeadde08000000efbeadde01000000efbeaddefdffffffefbeadde07000000efbeaddefeffffffefbeadde10000000efbeadde01000000efbeadde8f010000efbeaddefdffffffefbeaddefdffffffefbeadde";
    /// New objective, `add` path: target 1083, nums 0, complete 0.
    const DUNGEON_NEW_OBJECTIVE_HEX: &str = "0aab010aa801feffffffefbeadde90000000efbeadde04000000efbeaddefeffffffefbeadde70000000efbeadde01000000efbeadde01000000efbeadde00000000efbeadde00000000efbeadde3b040000efbeaddefeffffffefbeadde30000000efbeadde01000000efbeadde3b040000efbeadde02000000efbeadde00000000efbeadde03000000efbeadde00000000efbeaddefdffffffefbeaddefdffffffefbeaddefdffffffefbeadde";
    /// Objective completed, `update` path: key 111123, nums 900, complete
    /// 1, no `target_id` inside the value itself.
    const DUNGEON_OBJECTIVE_COMPLETED_HEX: &str = "0a9b010a9801feffffffefbeadde80000000efbeadde04000000efbeaddefeffffffefbeadde60000000efbeadde01000000efbeadde00000000efbeadde00000000efbeadde01000000efbeadde13b20100efbeaddefeffffffefbeadde20000000efbeadde02000000efbeadde84030000efbeadde03000000efbeadde01000000efbeaddefdffffffefbeaddefdffffffefbeaddefdffffffefbeadde";
    /// Dungeon vars, 9 entries: first `InteractTimes=2`, last
    /// `cur_qinshi=90`.
    const DUNGEON_VARS_HEX: &str = "0ade050adb05feffffffefbeaddec3020000efbeadde0a000000efbeaddefeffffffefbeaddea3020000efbeadde01000000efbeadde09000000efbeaddefeffffffefbeadde31000000efbeadde01000000efbeadde0d000000efbeadde496e74657261637454696d6573efbeadde02000000efbeadde02000000efbeaddefdffffffefbeaddefeffffffefbeadde37000000efbeadde01000000efbeadde13000000efbeadde436f756e74446f776e54696d65725374617465efbeadde02000000efbeadde01000000efbeaddefdffffffefbeaddefeffffffefbeadde31000000efbeadde01000000efbeadde0d000000efbeadde50726f67726573735374617465efbeadde02000000efbeadde01000000efbeaddefdffffffefbeaddefeffffffefbeadde30000000efbeadde01000000efbeadde0c000000efbeadde626c756562616c6c5f6e756defbeadde02000000efbeadde00000000efbeaddefdffffffefbeaddefeffffffefbeadde34000000efbeadde01000000efbeadde10000000efbeadde626c756562616c6c5f6e756d5f6d6178efbeadde02000000efbeadde04000000efbeaddefdffffffefbeaddefeffffffefbeadde33000000efbeadde01000000efbeadde0f000000efbeadde6d757369635f76616c75655f6d6178efbeadde02000000efbeadde84030000efbeaddefdffffffefbeaddefeffffffefbeadde2f000000efbeadde01000000efbeadde0b000000efbeadde6d757369635f76616c7565efbeadde02000000efbeadde7a020000efbeaddefdffffffefbeaddefeffffffefbeadde2e000000efbeadde01000000efbeadde0a000000efbeadde6d61785f71696e736869efbeadde02000000efbeadde64000000efbeaddefdffffffefbeaddefeffffffefbeadde2e000000efbeadde01000000efbeadde0a000000efbeadde6375725f71696e736869efbeadde02000000efbeadde5a000000efbeaddefdffffffefbeaddefdffffffefbeaddefdffffffefbeadde";

    /// Hand-built blob (unpadded -- the `0xDEADBEEF` guard words every
    /// real capture carries are irrelevant to the hashmap sections under
    /// test, and `blob::tests` covers both padding modes already). Shape:
    /// `DungeonDirtyData { 4: Target { 1: hashmap{ add: [], remove:
    /// [target_id], update: [] } } }`. No real capture on this build was
    /// ever seen carrying a `remove` entry -- this fixture exists to pin
    /// the decode side of the removal path (PR #226 review, finding 2),
    /// not to claim the shape was observed.
    fn synthetic_objective_removed_blob(target_id: i32) -> Vec<u8> {
        fn i32le(v: i32) -> [u8; 4] {
            v.to_le_bytes()
        }
        // `-2` struct begin, `-3` struct end (see `blob`'s module doc).
        const BEGIN: i32 = -2;
        const END: i32 = -3;
        fn wrap(body: &[u8]) -> Vec<u8> {
            [
                i32le(BEGIN).as_slice(),
                i32le(body.len() as i32).as_slice(),
                body,
                i32le(END).as_slice(),
            ]
            .concat()
        }
        // add = 0, remove = 1, update = 0, then the bare removed key.
        let hashmap = [i32le(0), i32le(1), i32le(0), i32le(target_id)].concat();
        // `Target` field 1 is the hashmap; `DungeonDirtyData` field 4 is
        // the target struct.
        let target = wrap(&[i32le(1).as_slice(), hashmap.as_slice()].concat());
        wrap(&[i32le(4).as_slice(), target.as_slice()].concat())
    }

    fn dungeon_notify(method_id: u32, payload: Vec<u8>) -> Notify {
        Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id,
            payload,
        }
    }

    /// `SYNC_DUNGEON_DATA` (`0x17`) is plain protobuf, so its fixtures are
    /// built with prost rather than capture hex: all six real messages on
    /// this opcode were empty (see `pb::DungeonSyncData`), which leaves
    /// the populated-`flow_info` branch with no capture to replay.
    fn dungeon_data_notify(scene_uuid: u32, state: Option<i32>) -> Notify {
        let msg = pb::DungeonSyncData {
            scene_uuid,
            flow_info: state.map(|state| pb::DungeonFlowInfo { state }),
            target: None,
            dungeon_var: None,
        };
        dungeon_notify(opcode::SYNC_DUNGEON_DATA, msg.encode_to_vec())
    }

    #[test]
    fn dungeon_data_flow_info_emits_state_and_scene_uuid() {
        // 3 = `Playing`, per `EDungeonState`'s `From<i32>`.
        let n = dungeon_data_notify(4_242, Some(3));
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ProtocolEvent::DungeonState { state, scene_uuid } => {
                assert_eq!(*state, EDungeonState::Playing);
                assert_eq!(*scene_uuid, Some(4_242));
            }
            other => panic!("expected DungeonState, got {other:?}"),
        }
    }

    /// Protobuf cannot tell an unset `uint32` from a real 0, so
    /// `on_sync_dungeon_data` reads 0 as absent -- the same call
    /// `attrs::scene_id_from_attrs` already makes for scene ids.
    #[test]
    fn dungeon_data_zero_scene_uuid_decodes_as_absent() {
        let n = dungeon_data_notify(0, Some(4));
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ProtocolEvent::DungeonState { state, scene_uuid } => {
                assert_eq!(*state, EDungeonState::End);
                assert_eq!(*scene_uuid, None);
            }
            other => panic!("expected DungeonState, got {other:?}"),
        }
    }

    /// The shape every real `0x17` message on this build actually had:
    /// nothing to report, and nothing emitted -- `target`/`dungeon_var`
    /// are unmodeled placeholders and are never guessed at.
    #[test]
    fn dungeon_data_without_flow_info_emits_nothing() {
        let n = dungeon_data_notify(0, None);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(out.is_empty());
    }

    #[test]
    fn dungeon_dirty_data_removed_objective_emits_a_removal() {
        let msg = pb::SyncDungeonDirtyData {
            v_data: Some(pb::BufferStream {
                buffer: synthetic_objective_removed_blob(1083),
                stream_type: 0,
            }),
        };
        let n = dungeon_notify(opcode::SYNC_DUNGEON_DIRTY_DATA, msg.encode_to_vec());
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(
            out,
            vec![ProtocolEvent::DungeonObjectiveRemoved { target_id: 1083 }]
        );
    }

    fn dungeon_dirty_notify(hex: &str) -> Notify {
        Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_DUNGEON_DIRTY_DATA,
            payload: crate::dump_format::hex_decode(hex).expect("valid fixture hex"),
        }
    }

    #[test]
    fn dungeon_dirty_data_flow_info_end() {
        let n = dungeon_dirty_notify(DUNGEON_FLOW_INFO_END_HEX);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ProtocolEvent::DungeonState { state, scene_uuid } => {
                assert_eq!(*state, EDungeonState::End);
                assert_eq!(*scene_uuid, None);
            }
            other => panic!("expected DungeonState, got {other:?}"),
        }
    }

    #[test]
    fn dungeon_dirty_data_new_objective_add_path() {
        let n = dungeon_dirty_notify(DUNGEON_NEW_OBJECTIVE_HEX);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ProtocolEvent::DungeonObjective {
                target_id,
                nums,
                complete,
            } => {
                assert_eq!(*target_id, 1083);
                assert_eq!(*nums, Some(0));
                assert_eq!(*complete, Some(false));
            }
            other => panic!("expected DungeonObjective, got {other:?}"),
        }
    }

    #[test]
    fn dungeon_dirty_data_objective_completed_update_path_uses_hashmap_key() {
        let n = dungeon_dirty_notify(DUNGEON_OBJECTIVE_COMPLETED_HEX);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ProtocolEvent::DungeonObjective {
                target_id,
                nums,
                complete,
            } => {
                // The value itself carries no `target_id` on this fixture
                // (an update entry) — the hashmap key (111123) must still
                // be the id that lands on the event.
                assert_eq!(*target_id, 111123);
                assert_eq!(*nums, Some(900));
                assert_eq!(*complete, Some(true));
            }
            other => panic!("expected DungeonObjective, got {other:?}"),
        }
    }

    #[test]
    fn dungeon_dirty_data_vars_all_nine_in_wire_order() {
        let n = dungeon_dirty_notify(DUNGEON_VARS_HEX);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 9);
        match &out[0] {
            ProtocolEvent::DungeonVar { name, value } => {
                assert_eq!(name, "InteractTimes");
                assert_eq!(*value, 2);
            }
            other => panic!("expected DungeonVar, got {other:?}"),
        }
        match &out[8] {
            ProtocolEvent::DungeonVar { name, value } => {
                assert_eq!(name, "cur_qinshi");
                assert_eq!(*value, 90);
            }
            other => panic!("expected DungeonVar, got {other:?}"),
        }
    }

    #[test]
    fn dungeon_dirty_data_truncated_payload_drops_silently_without_panic() {
        let full = crate::dump_format::hex_decode(DUNGEON_FLOW_INFO_END_HEX).unwrap();
        let n = Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_DUNGEON_DIRTY_DATA,
            payload: full[..full.len() / 2].to_vec(),
        };
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(out.is_empty());
    }

    // -- SyncNearEntities.disappear (issue #215) ---------------------------

    fn near_entities_notify(appear: Vec<pb::Entity>, disappear: Vec<i64>) -> Notify {
        let msg = pb::SyncNearEntities {
            appear,
            disappear: disappear
                .into_iter()
                .map(|uuid| pb::DisappearEntity {
                    uuid,
                    disappear_type: None,
                })
                .collect(),
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_ENTITIES,
            payload,
        }
    }

    #[test]
    fn disappearing_monster_emits_enemy_gone() {
        let n = near_entities_notify(vec![], vec![TARGET_UUID]);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ProtocolEvent::EnemyGone { uid, reason } => {
                assert_eq!(*uid, uid_of(TARGET_UUID));
                assert_eq!(
                    *reason, None,
                    "no tag 2 on the wire must decode as no reason at all"
                );
            }
            other => panic!("expected EnemyGone, got {other:?}"),
        }
    }

    /// A player walking out of AOI range is not an enemy, and must not
    /// produce an entity-gone signal the meter would have to filter itself.
    #[test]
    fn disappearing_player_emits_nothing() {
        let n = near_entities_notify(vec![], vec![ATTACKER_UUID]);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(out.is_empty());
    }

    /// An entity type the meter has no model for (NPC, pet, ...) is dropped
    /// exactly as it is on the `appear` side.
    #[test]
    fn disappearing_unknown_entity_kind_emits_nothing() {
        let n = near_entities_notify(vec![], vec![(77i64 << 16) | (2 << 6)]);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(out.is_empty());
    }

    /// One packet can both introduce entities and retire others; the
    /// `appear` side keeps its existing behaviour and the events stay in
    /// wire-field order (appear first, then disappear).
    #[test]
    fn appear_and_disappear_in_one_message_both_decode() {
        let appear = pb::Entity {
            uuid: (31i64 << 16) | 64,
            ent_type: pb::EEntityType::EntMonster as i32,
            attrs: Some(AttrCollection {
                uuid: (31i64 << 16) | 64,
                attrs: vec![],
            }),
        };
        let n = near_entities_notify(vec![appear], vec![TARGET_UUID]);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], ProtocolEvent::EnemyHp(e) if e.uid == 31));
        assert!(
            matches!(&out[1], ProtocolEvent::EnemyGone { uid, .. } if *uid == uid_of(TARGET_UUID)),
            "expected EnemyGone second, got {:?}",
            out[1]
        );
    }

    /// Every disappearing monster in a batch is reported, in wire order.
    #[test]
    fn every_disappearing_monster_in_a_batch_is_reported() {
        let n = near_entities_notify(vec![], vec![TARGET_UUID, (31i64 << 16) | 64]);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], ProtocolEvent::EnemyGone { uid, .. } if *uid == 30));
        assert!(matches!(&out[1], ProtocolEvent::EnemyGone { uid, .. } if *uid == 31));
    }

    // -- DisappearEntity tag 2 / EDisappearType (issue #276) ---------------

    /// Same as [`near_entities_notify`], but each disappearing uuid carries
    /// an explicit tag-2 value — the wire shape the two reference sources
    /// and 469 of our 851 captured disappear entries actually use.
    fn disappear_notify(entries: Vec<(i64, i32)>) -> Notify {
        let msg = pb::SyncNearEntities {
            appear: vec![],
            disappear: entries
                .into_iter()
                .map(|(uuid, disappear_type)| pb::DisappearEntity {
                    uuid,
                    disappear_type: Some(disappear_type),
                })
                .collect(),
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_ENTITIES,
            payload,
        }
    }

    /// Every named `EDisappearType` value round-trips through the wire into
    /// its `DisappearReason`. Asserted variant by variant rather than by
    /// `from(i as i32)` so a renumbering can't pass by matching its own bug.
    #[test]
    fn each_disappear_type_decodes_to_its_reason() {
        let cases = [
            (pb::EDisappearType::Normal, DisappearReason::Normal),
            (pb::EDisappearType::Dead, DisappearReason::Dead),
            (pb::EDisappearType::Destroy, DisappearReason::Destroy),
            (
                pb::EDisappearType::TransferLeave,
                DisappearReason::TransferLeave,
            ),
            (
                pb::EDisappearType::TransferPassLineLeave,
                DisappearReason::TransferPassLineLeave,
            ),
        ];
        for (wire, expected) in cases {
            let n = disappear_notify(vec![(TARGET_UUID, wire as i32)]);
            let mut out = Vec::new();
            decode_notify(&n, 0, &mut out, None);
            assert_eq!(out.len(), 1, "{wire:?}");
            match &out[0] {
                ProtocolEvent::EnemyGone { uid, reason } => {
                    assert_eq!(*uid, uid_of(TARGET_UUID), "{wire:?}");
                    assert_eq!(*reason, Some(expected), "{wire:?}");
                }
                other => panic!("expected EnemyGone for {wire:?}, got {other:?}"),
            }
        }
    }

    /// A value this build has never seen must stay distinguishable rather
    /// than collapsing into `Normal` — the same reason `EDungeonState` keeps
    /// an explicit `Unknown`.
    #[test]
    fn unrecognized_disappear_type_decodes_to_unknown() {
        let n = disappear_notify(vec![(TARGET_UUID, 99)]);
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(matches!(
            &out[0],
            ProtocolEvent::EnemyGone {
                reason: Some(DisappearReason::Unknown(99)),
                ..
            }
        ));
    }

    /// Tag 2 rides along per entry, not per packet: one batch can mix a
    /// killed monster with an ordinary eviction and an untyped disappear.
    #[test]
    fn one_batch_can_mix_disappear_reasons() {
        let msg = pb::SyncNearEntities {
            appear: vec![],
            disappear: vec![
                pb::DisappearEntity {
                    uuid: TARGET_UUID,
                    disappear_type: Some(pb::EDisappearType::Dead as i32),
                },
                pb::DisappearEntity {
                    uuid: (31i64 << 16) | 64,
                    disappear_type: Some(pb::EDisappearType::Destroy as i32),
                },
                pb::DisappearEntity {
                    uuid: (32i64 << 16) | 64,
                    disappear_type: None,
                },
            ],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let n = Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_ENTITIES,
            payload,
        };
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert_eq!(out.len(), 3);
        assert!(matches!(
            &out[0],
            ProtocolEvent::EnemyGone {
                reason: Some(DisappearReason::Dead),
                ..
            }
        ));
        assert!(matches!(
            &out[1],
            ProtocolEvent::EnemyGone {
                reason: Some(DisappearReason::Destroy),
                ..
            }
        ));
        assert!(matches!(
            &out[2],
            ProtocolEvent::EnemyGone { reason: None, .. }
        ));
    }

    // -- NotifyReviveUser / AttrState (issue #272/#339) -----------------

    #[test]
    fn notify_revive_user_emits_revive_for_actor() {
        let msg = pb::NotifyReviveUser {
            v_actor_uuid: Some(ATTACKER_UUID),
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let n = Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::NOTIFY_REVIVE_USER,
            payload,
        };
        let mut out = Vec::new();
        decode_notify(&n, 4242, &mut out, None);
        assert_eq!(out.len(), 1);
        match &out[0] {
            ProtocolEvent::Revive { uid, timestamp_ms } => {
                assert_eq!(*uid, uid_of(ATTACKER_UUID));
                assert_eq!(*timestamp_ms, 4242);
            }
            other => panic!("expected Revive, got {other:?}"),
        }
    }

    #[test]
    fn notify_revive_user_missing_actor_uuid_emits_nothing() {
        let msg = pb::NotifyReviveUser { v_actor_uuid: None };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let n = Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::NOTIFY_REVIVE_USER,
            payload,
        };
        let mut out = Vec::new();
        decode_notify(&n, 0, &mut out, None);
        assert!(out.is_empty());
    }

    fn attr_state_notify(uuid: i64, state: u64) -> Notify {
        let mut raw_data = Vec::new();
        prost::encoding::encode_varint(state, &mut raw_data);
        let delta = AoiSyncDelta {
            uuid,
            attrs: Some(AttrCollection {
                uuid,
                attrs: vec![pb::Attr {
                    id: crate::attrs::attr_id::STATE,
                    raw_data,
                }],
            }),
            skill_effects: None,
            buff_effect: None,
        };
        let msg = SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        Notify {
            service_uuid: crate::frame::SERVICE_UUID,
            method_id: opcode::SYNC_NEAR_DELTA_INFO,
            payload,
        }
    }

    #[test]
    fn attr_state_dead_on_player_emits_entity_state_dead() {
        let mut out = Vec::new();
        decode_notify(&attr_state_notify(ATTACKER_UUID, 9), 100, &mut out, None);
        let ev = out
            .iter()
            .find_map(|ev| match ev {
                ProtocolEvent::EntityState {
                    uid,
                    kind,
                    is_dead,
                    timestamp_ms,
                } => Some((*uid, *kind, *is_dead, *timestamp_ms)),
                _ => None,
            })
            .expect("expected an EntityState event");
        assert_eq!(ev, (uid_of(ATTACKER_UUID), EntityKind::Player, true, 100));
    }

    #[test]
    fn attr_state_non_dead_on_monster_emits_entity_state_alive() {
        let mut out = Vec::new();
        decode_notify(&attr_state_notify(TARGET_UUID, 8), 0, &mut out, None);
        let ev = out
            .iter()
            .find_map(|ev| match ev {
                ProtocolEvent::EntityState {
                    uid, kind, is_dead, ..
                } => Some((*uid, *kind, *is_dead)),
                _ => None,
            })
            .expect("expected an EntityState event");
        assert_eq!(ev, (uid_of(TARGET_UUID), EntityKind::Monster, false));
    }

    #[test]
    fn no_attr_state_attr_emits_no_entity_state_event() {
        let mut out = Vec::new();
        decode_notify(&skill_attr_notify(ATTACKER_UUID, 1550), 0, &mut out, None);
        assert!(
            !out.iter()
                .any(|ev| matches!(ev, ProtocolEvent::EntityState { .. }))
        );
    }
}
