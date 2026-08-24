//! Hand-written `derive(prost::Message)` structs for the ~15 messages the MVP
//! needs (see plan §0.4). No `build.rs` / `.proto` / `prost-build` — prost
//! skips unknown tags automatically, so these stay forward-compatible.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, ::prost::Enumeration)]
#[repr(i32)]
pub enum EDamageType {
    Normal = 0,
    Miss = 1,
    Heal = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, ::prost::Enumeration)]
#[repr(i32)]
pub enum EEntityType {
    EntErrType = 0,
    EntMonster = 1,
    EntChar = 10,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SyncDamageInfo {
    #[prost(bool, tag = "2")]
    pub is_miss: bool,
    #[prost(enumeration = "EDamageType", tag = "4")]
    pub r#type: i32,
    #[prost(int32, tag = "5")]
    pub type_flag: i32,
    #[prost(int64, tag = "6")]
    pub value: i64,
    #[prost(int64, tag = "8")]
    pub lucky_value: i64,
    #[prost(int64, tag = "9")]
    pub hp_lessen_value: i64,
    #[prost(int64, tag = "11")]
    pub attacker_uuid: i64,
    #[prost(int32, tag = "12")]
    pub owner_id: i32,
    #[prost(bool, tag = "17")]
    pub is_dead: bool,
    #[prost(int64, tag = "21")]
    pub top_summoner_id: i64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SkillEffect {
    #[prost(message, repeated, tag = "2")]
    pub damages: Vec<SyncDamageInfo>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AoiSyncDelta {
    #[prost(int64, tag = "1")]
    pub uuid: i64,
    #[prost(message, optional, tag = "2")]
    pub attrs: Option<AttrCollection>,
    #[prost(message, optional, tag = "7")]
    pub skill_effects: Option<SkillEffect>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AoiSyncToMeDelta {
    #[prost(message, optional, tag = "1")]
    pub base_delta: Option<AoiSyncDelta>,
    #[prost(int64, tag = "5")]
    pub uuid: i64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SyncToMeDeltaInfo {
    #[prost(message, optional, tag = "1")]
    pub delta_info: Option<AoiSyncToMeDelta>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SyncNearDeltaInfo {
    #[prost(message, repeated, tag = "1")]
    pub delta_infos: Vec<AoiSyncDelta>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Attr {
    #[prost(int32, tag = "1")]
    pub id: i32,
    #[prost(bytes = "vec", tag = "2")]
    pub raw_data: Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AttrCollection {
    #[prost(int64, tag = "1")]
    pub uuid: i64,
    #[prost(message, repeated, tag = "2")]
    pub attrs: Vec<Attr>,
}

/// One entry of a player's skill list, inside a `SkillLevelIdList` (issue #33).
///
/// This carries *every* skill the player has, not just their equipped
/// Imagines: deciding which entries are Imagines needs a lookup table this
/// crate deliberately does not carry, and happens in `crates/app`.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SkillLevelInfo {
    #[prost(int32, tag = "1")]
    pub skill_id: i32,
    #[prost(int32, tag = "2")]
    pub current_level: i32,
    #[prost(int32, tag = "3")]
    pub remodel_level: i32,
}

/// Wrapper for `attr_id::SKILL_LEVEL_ID_LIST` (`0x74`, issue #33).
///
/// **Unverified constant.** The wrapper field's tag (assumed `1`) has not
/// been confirmed against live traffic — no capture is available. Same
/// documented exception as `attr_id::SEASON_LEVEL` and
/// `IMAGINE_PROFESSION_IDS`: reference-derived, unconfirmed, per
/// `docs/packet-inspection.md`'s "Recording a result" procedure. Re-verify
/// against a real capture if one ever becomes available.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SkillLevelIdList {
    #[prost(message, repeated, tag = "1")]
    pub skills: Vec<SkillLevelInfo>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Entity {
    #[prost(int64, tag = "1")]
    pub uuid: i64,
    #[prost(enumeration = "EEntityType", tag = "2")]
    pub ent_type: i32,
    #[prost(message, optional, tag = "3")]
    pub attrs: Option<AttrCollection>,
}

/// Why an entity left the client's area of interest, carried on
/// [`DisappearEntity`]'s tag 2 (issue #276).
///
/// Two independent references model this field identically: `resonance-logs`'
/// `src-tauri/.../blueprotobuf_package.rs:5843`
/// (`#[prost(enumeration = "EDisappearType", optional, tag = "2")]`), and
/// BPSR-ZDPS' `BPSR-ZDPSLib/protos/EnumEDisappearType.cs`, the game's own
/// generated descriptor for `enum_e_disappear_type.proto`. The variant names
/// here drop the reference sources' `EDisappear` prefix, exactly as
/// [`EDamageType`] drops `EDamage`.
///
/// **Live-capture evidence** (house rule for a decoded field, see
/// `docs/packet-inspection.md`), re-derived over every non-empty
/// `inspect/dump-*.jsonl` on disk — 6 files, 32,260 records, 983
/// `SyncNearEntities`, **851 disappear entries**. Tag 2 was present on 469 of
/// them and absent on 382; the observed behaviour matches the reference names
/// exactly, with "reappeared" meaning the same uuid turned up again in a later
/// `appear` list of the same capture:
///
/// ```text
/// tag 2                monsters (ent_type 1)          other entity types
/// -------------------- ------------------------------ -----------------------------
/// absent               23, 43% reappeared (med 2.5s)   359, 61% reappeared
/// 1 Dead               58,  14% reappeared (med 14.9s) 329 (types 3/6/11/14), 0%
/// 2 Destroy            50,   0% reappeared             24, 1 reappear (one type 3)
/// 3 TransferLeave       0                              8, characters only, 0%
/// 0 Normal / 4 …Line…   never observed in these captures
/// ```
///
/// The absent column is a classic AOI range-out: high reappear rate, median
/// 2.5s. `Dead` and `Destroy` are both terminal for monsters; the only 8
/// `Dead` monsters that came back are six trash uids sharing one despawn
/// timestamp, returning after 14.9s on uids spaced at exact 65536 boundaries
/// — spawn-slot reuse on respawn, not a wipe. `TransferLeave` was seen on
/// characters only: a player zoning out.
///
/// This is what lets `bpsr_meter` stop inferring death from an HP threshold —
/// see `bpsr_meter::Meter::apply_enemy_gone`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, ::prost::Enumeration)]
#[repr(i32)]
pub enum EDisappearType {
    Normal = 0,
    Dead = 1,
    Destroy = 2,
    TransferLeave = 3,
    TransferPassLineLeave = 4,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DisappearEntity {
    #[prost(int64, tag = "1")]
    pub uuid: i64,
    /// The reason the entity vanished — see [`EDisappearType`] for the
    /// reference sourcing and the capture evidence. Genuinely optional on
    /// the wire (382 of 851 observed entries carry no tag 2 at all), so a
    /// `None` here means "the server did not say", not "normal".
    #[prost(enumeration = "EDisappearType", optional, tag = "2")]
    pub disappear_type: Option<i32>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SyncNearEntities {
    #[prost(message, repeated, tag = "1")]
    pub appear: Vec<Entity>,
    #[prost(message, repeated, tag = "2")]
    pub disappear: Vec<DisappearEntity>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SyncContainerData {
    #[prost(message, optional, tag = "1")]
    pub v_data: Option<CharSerialize>,
}

/// `WorldNtf.EnterScene` (opcode 3, `decode::opcode::ENTER_SCENE`, issue
/// #35). Only field 1 (the scene attrs, via [`EnterSceneInfo`]) is modeled —
/// the message also carries character data (fields 1.2: 1, 2, 3, 4, 7) and
/// top-level fields 7/8/9, dumped from a real capture but not needed by
/// anything downstream today; prost skips them automatically (see this
/// file's module doc), so adding them later is non-breaking.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct EnterScene {
    #[prost(message, optional, tag = "1")]
    pub info: Option<EnterSceneInfo>,
}

/// The scene-enter payload nested inside `EnterScene`'s field 1. Field 1 is
/// an `AttrCollection` — the same `{2: repeated Attr}` shape already used by
/// `SyncNearEntities`' `Entity.attrs` — carrying the scene attrs
/// (`attr_id::SCENE_BASIC_ID` and its siblings), confirmed against a real
/// capture (issue #35).
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct EnterSceneInfo {
    #[prost(message, optional, tag = "1")]
    pub attrs: Option<AttrCollection>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CharSerialize {
    #[prost(int64, tag = "1")]
    pub char_id: i64,
    #[prost(message, optional, tag = "2")]
    pub char_base: Option<CharBaseInfo>,
    // Field 3 was `scene_data: Option<SceneData>`, ported unverified from
    // `winjwinj/bpsr-logs`' `pb.proto` (issue #35). **Disproven**: a live
    // 19,667-message capture's single `SyncContainerData.CharSerialize` has
    // no field 3 at all (its field numbers jump 2 -> 5), and zero messages
    // anywhere in that capture match the `SceneData{level_map_id}` shape.
    // The scene id actually rides `opcode::ENTER_SCENE`'s attr channel — see
    // `attr_id::SCENE_BASIC_ID` and `decode::on_enter_scene`.
    #[prost(message, optional, tag = "61")]
    pub profession_list: Option<ProfessionList>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CharBaseInfo {
    #[prost(int64, tag = "1")]
    pub char_id: i64,
    #[prost(string, tag = "5")]
    pub name: String,
    #[prost(int32, tag = "35")]
    pub fight_point: i32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ProfessionList {
    #[prost(int32, tag = "1")]
    pub cur_profession_id: i32,
}

/// Player class, derived from `ATTR_PROFESSION_ID` / `cur_profession_id`
/// (plan §0.6). Any id not listed maps to `Unknown`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    Stormblade,
    FrostMage,
    TwinStriker,
    WindKnight,
    VerdantOracle,
    HeavyGuardian,
    Marksman,
    ShieldKnight,
    BeatPerformer,
    Unknown,
}

/// **Decoders should not call `Class::from` directly on a raw profession
/// id** — use [`class_of_profession_id`] instead, which routes an "Imagine"
/// transform id (see [`IMAGINE_PROFESSION_IDS`]) to `None` rather than
/// `Some(Class::Unknown)`. This impl is kept as the plain, total id -> class
/// table `class_of_profession_id` and `Class::from` in general both build
/// on; any id not listed here (including an Imagine id) falls through to
/// `Unknown`.
impl From<i32> for Class {
    fn from(id: i32) -> Self {
        match id {
            1 => Class::Stormblade,
            2 => Class::FrostMage,
            3 => Class::TwinStriker,
            4 => Class::WindKnight,
            5 => Class::VerdantOracle,
            9 => Class::HeavyGuardian,
            11 => Class::Marksman,
            12 => Class::ShieldKnight,
            13 => Class::BeatPerformer,
            _ => Class::Unknown,
        }
    }
}

impl Class {
    pub fn name(&self) -> &'static str {
        match self {
            Class::Stormblade => "Stormblade",
            Class::FrostMage => "FrostMage",
            Class::TwinStriker => "TwinStriker",
            Class::WindKnight => "WindKnight",
            Class::VerdantOracle => "VerdantOracle",
            Class::HeavyGuardian => "HeavyGuardian",
            Class::Marksman => "Marksman",
            Class::ShieldKnight => "ShieldKnight",
            Class::BeatPerformer => "BeatPerformer",
            Class::Unknown => "Unknown",
        }
    }
}

/// Profession ids belonging to "Imagine" skills (issue #37): temporary
/// transforms into an NPC-like character (Dorothy, Dark Spirit Dance, Lucy,
/// Natsu), not real player classes. A transformed player's
/// `cur_profession_id` / `ATTR_PROFESSION_ID` reads one of these for the
/// duration of the transform.
///
/// Reference-derived, **not confirmed against a live capture** (issue #37):
/// reimplemented from BPSR-ZDPS's `EProfessionId` (`Dorothy = 8`,
/// `DarkSpiritDance = 10`, `Lucy = 14`, `Natsu = 15`) because no packet
/// capture was available while fixing this — the repo owner sanctioned this
/// as the same kind of exception `attr_id::SEASON_LEVEL` /
/// `attr_id::SEASON_STRENGTH` document in `crates/protocol/src/attrs.rs`
/// (see also `docs/packet-inspection.md`). Re-verify against a real capture
/// if one ever becomes available; do not extend this list without one.
pub const IMAGINE_PROFESSION_IDS: [i32; 4] = [8, 10, 14, 15];

/// True for a profession id that identifies an Imagine transform rather
/// than a real player class. See [`IMAGINE_PROFESSION_IDS`].
pub fn is_imagine_profession_id(id: i32) -> bool {
    IMAGINE_PROFESSION_IDS.contains(&id)
}

/// Resolves a raw profession id to the `Class` it represents — except an
/// Imagine id (see [`is_imagine_profession_id`]), which yields `None`
/// rather than `Some(Class::Unknown)`. Decoders must call this instead of
/// `Class::from` directly: a transform id is not "an unrecognized class",
/// it is "no class information in this packet", so it must merge the same
/// way an absent profession id does (leaving a cached class untouched)
/// rather than clobbering it with `Unknown`.
pub fn class_of_profession_id(id: i32) -> Option<Class> {
    if is_imagine_profession_id(id) {
        None
    } else {
        Some(Class::from(id))
    }
}

/// `GrpcTeamNtf.NotifyJoinTeam` (`decode::team_opcode::NOTIFY_JOIN_TEAM`,
/// issue #146): the bulk party-roster push. Every tag on this struct and
/// the `NotifyJoinTeamRequest` -> `TeamMemData` -> `TeamMemberSocialData` ->
/// `{TeamBasicData, TeamProfessionData, TeamUserAttrData}` tree below was
/// read directly off protoc-generated `FieldNumber` constants in the
/// BPSR-ZDPS reference tool's .NET metadata, the same provenance discipline
/// as `CharSerialize`/`EnterScene` above — not inferred from field order or
/// ported from another project. All plain varint fields are `int32`/`int64`
/// (never `sint*`/zigzag), matching how BPSR-ZDPS declares them.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct NotifyJoinTeam {
    #[prost(message, optional, tag = "1")]
    pub v_request: Option<NotifyJoinTeamRequest>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct NotifyJoinTeamRequest {
    #[prost(message, optional, tag = "1")]
    pub base_info: Option<TeamBaseInfo>,
    #[prost(message, repeated, tag = "2")]
    pub member_data: Vec<TeamMemData>,
}

/// An empty placeholder struct (issue #146): BPSR-ZDPS's `TeamBaseInfo` has
/// no field tags documented in the issue's table, so nothing is modeled
/// here yet. Kept as a distinct type (rather than dropping `base_info`
/// entirely) so a future slice can add fields without touching
/// `NotifyJoinTeamRequest`'s shape.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TeamBaseInfo {}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TeamMemData {
    #[prost(int64, tag = "1")]
    pub char_id: i64,
    #[prost(int32, tag = "6")]
    pub scene_id: i32,
    #[prost(int32, tag = "8")]
    pub group_id: i32,
    #[prost(message, optional, tag = "9")]
    pub social_data: Option<TeamMemberSocialData>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TeamMemberSocialData {
    #[prost(message, optional, tag = "1")]
    pub basic_data: Option<TeamBasicData>,
    #[prost(message, optional, tag = "4")]
    pub profession_data: Option<TeamProfessionData>,
    #[prost(message, optional, tag = "8")]
    pub user_attr_data: Option<TeamUserAttrData>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TeamBasicData {
    #[prost(int64, tag = "1")]
    pub char_id: i64,
    #[prost(string, tag = "3")]
    pub name: String,
    /// Decoded but deliberately unemitted (issue #146 spec decision 3):
    /// `PlayerInfo` has no `level` field and nothing downstream consumes
    /// one, so `decode::on_notify_join_team` reads this only to leave it
    /// unused, never mapping it into an event.
    #[prost(int32, tag = "6")]
    pub level: i32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TeamProfessionData {
    #[prost(int32, tag = "1")]
    pub profession_id: i32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TeamUserAttrData {
    #[prost(int64, tag = "4")]
    pub fight_point: i64,
    #[prost(int32, tag = "5")]
    pub season_strength: i32,
}

// -- Dungeon state / objectives (issue #139) --------------------------------
//
// Every tag below was read directly off BPSR-ZDPS's protoc-generated
// `FieldNumber` constants (`BPSR-ZDPSLib/protos/StruDungeonSyncData.cs:169-457`,
// `StruDungeonFlowInfo.cs:99-183`, `StruDungeonTarget.cs:89`,
// `StruDungeonTargetData.cs:89-113`, `StruDungeonVarData.cs`, and
// `Csharp.cs:30399` for `SyncDungeonDirtyData`) — the same provenance
// discipline `CharSerialize`/`EnterScene` above and `NotifyJoinTeam` follow,
// not inferred from field order. All plain varint fields are `int32`/
// `uint32` (never `sint*`/zigzag), matching how BPSR-ZDPS declares them.

/// `SyncDungeonDirtyData` (`decode::opcode::SYNC_DUNGEON_DIRTY_DATA`, `0x18`,
/// issue #139): the outer protobuf wrapper around the blob-encoded dungeon
/// dirty-data channel. `v_data.buffer` is **not** protobuf — see
/// `crate::blob`'s module doc for its wire format, and that module's
/// `detect_stream_safe` for why `v_data.stream_type` must not be trusted to
/// say whether `buffer` is padded.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SyncDungeonDirtyData {
    #[prost(message, optional, tag = "1")]
    pub v_data: Option<BufferStream>,
}

/// The blob-carrying wrapper nested inside `SyncDungeonDirtyData`, and, per
/// BPSR-ZDPS, reused for `SyncContainerDirtyData` (`0x16`) — the channel
/// this crate deliberately still leaves undecoded (see `decode.rs`'s
/// comment on that opcode) even though this same blob reader could now
/// parse it.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BufferStream {
    #[prost(bytes = "vec", tag = "1")]
    pub buffer: Vec<u8>,
    #[prost(int32, tag = "2")]
    pub stream_type: i32,
}

/// `WorldNtf.SyncDungeonData` (`decode::opcode::SYNC_DUNGEON_DATA`, `0x17`,
/// issue #139): the plain-protobuf full dungeon sync, as opposed to
/// `SyncDungeonDirtyData`'s blob-encoded delta above. All six real capture
/// messages on this opcode were empty (open-world) — `target`/`dungeon_var`
/// have therefore never been observed populated, so they are modeled as
/// empty placeholders ([`DungeonTarget`], [`DungeonVar`]) rather than
/// guessed at; their populated shape is mirrored, with real capture
/// evidence, by [`crate::blob::HashmapDelta`]/[`crate::blob::VarData`] on
/// the `0x18` blob path instead.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DungeonSyncData {
    #[prost(uint32, tag = "1")]
    pub scene_uuid: u32,
    #[prost(message, optional, tag = "2")]
    pub flow_info: Option<DungeonFlowInfo>,
    #[prost(message, optional, tag = "4")]
    pub target: Option<DungeonTarget>,
    #[prost(message, optional, tag = "10")]
    pub dungeon_var: Option<DungeonVar>,
}

/// `DungeonSyncData.flow_info`'s message type. Only field 1 (`state`) is
/// modeled — the same field [`crate::blob::FlowInfo`] decodes from the
/// blob format.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DungeonFlowInfo {
    #[prost(int32, tag = "1")]
    pub state: i32,
}

/// An empty placeholder — see [`DungeonSyncData`]'s doc comment.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DungeonTarget {}

/// An empty placeholder — see [`DungeonSyncData`]'s doc comment.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DungeonVar {}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[test]
    fn round_trip_sync_damage_info() {
        let msg = SyncDamageInfo {
            is_miss: false,
            r#type: EDamageType::Normal as i32,
            type_flag: 1,
            value: 12345,
            lucky_value: 0,
            hp_lessen_value: 12345,
            attacker_uuid: 999,
            owner_id: 42,
            is_dead: false,
            top_summoner_id: 0,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let decoded = SyncDamageInfo::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded, msg);
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    struct SyncDamageInfoWithExtra {
        #[prost(bool, tag = "2")]
        pub is_miss: bool,
        #[prost(int64, tag = "6")]
        pub value: i64,
        #[prost(int64, tag = "11")]
        pub attacker_uuid: i64,
        #[prost(string, tag = "99")]
        pub unknown_field: String,
    }

    #[test]
    fn round_trip_skill_level_id_list() {
        let msg = SkillLevelIdList {
            skills: vec![
                SkillLevelInfo {
                    skill_id: 3905,
                    current_level: 1,
                    remodel_level: 0,
                },
                SkillLevelInfo {
                    skill_id: 102640,
                    current_level: 2,
                    remodel_level: 1,
                },
            ],
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let decoded = SkillLevelIdList::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn decode_survives_unknown_tag() {
        let extra = SyncDamageInfoWithExtra {
            is_miss: true,
            value: 555,
            attacker_uuid: 777,
            unknown_field: "future-field".to_string(),
        };
        let mut buf = Vec::new();
        extra.encode(&mut buf).unwrap();
        let decoded = SyncDamageInfo::decode(buf.as_slice()).unwrap();
        assert!(decoded.is_miss);
        assert_eq!(decoded.value, 555);
        assert_eq!(decoded.attacker_uuid, 777);
    }

    #[test]
    fn class_from_id() {
        assert_eq!(Class::from(1), Class::Stormblade);
        assert_eq!(Class::from(3), Class::TwinStriker);
        assert_eq!(Class::from(13), Class::BeatPerformer);
        assert_eq!(Class::from(999), Class::Unknown);
        assert_eq!(Class::Stormblade.name(), "Stormblade");
        assert_eq!(Class::TwinStriker.name(), "TwinStriker");
    }

    // -- Imagine profession ids (issue #37) --------------------------------

    #[test]
    fn class_of_profession_id_maps_known_ids_like_class_from() {
        assert_eq!(class_of_profession_id(1), Some(Class::Stormblade));
        assert_eq!(class_of_profession_id(13), Some(Class::BeatPerformer));
    }

    #[test]
    fn class_of_profession_id_yields_none_for_imagine_ids() {
        for id in IMAGINE_PROFESSION_IDS {
            assert_eq!(
                class_of_profession_id(id),
                None,
                "imagine profession id {id} must not resolve to a class"
            );
        }
    }

    #[test]
    fn class_of_profession_id_yields_unknown_for_genuinely_unrecognized_ids() {
        for id in [0, 6, 7, 999] {
            assert_eq!(
                class_of_profession_id(id),
                Some(Class::Unknown),
                "unrecognized (non-Imagine) id {id} must still map to Class::Unknown"
            );
        }
    }

    #[test]
    fn is_imagine_profession_id_matches_exactly_the_documented_four() {
        for id in IMAGINE_PROFESSION_IDS {
            assert!(
                is_imagine_profession_id(id),
                "id {id} should be an Imagine id"
            );
        }
        for id in [0, 1, 6, 7, 9, 13, 999] {
            assert!(
                !is_imagine_profession_id(id),
                "id {id} should not be an Imagine id"
            );
        }
    }
}
