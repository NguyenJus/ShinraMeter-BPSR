//! Attribute id constants and varint/name decoding (plan §0.6).
//!
//! Every function here is non-panicking: an empty `raw_data`, `id == 0`, or a
//! malformed varint/utf8 payload is skipped rather than propagated as an
//! error.

use std::io::Cursor;

use prost::Message;

use crate::entity::EntityId;
use crate::event::{EnemyHp, PlayerInfo};
use crate::inspect::InspectSink;
use crate::pb;

pub mod attr_id {
    pub const NAME: i32 = 0x01;
    /// `AttrId` (10) — the entity's *type* id, which for a monster is its
    /// row in the game's monster table (what `tables::monster_name` and
    /// `tables::is_boss_monster` key off). Despite the generic name there
    /// is no separate "monster config id" attr: BPSR-ZDPS is explicit that
    /// this is the one that resolves, and rewrites a non-player's uid with
    /// it (`Managers/EncounterManager.cs:637-641`, "*Only the Attribute
    /// named Id (AttrId) is their real type UID which can be resolved into
    /// a name*"). `AttrConfigUid`/`AttrTableUid` exist in the enum but no
    /// reference tracker reads either as a monster identity.
    ///
    /// Corroborated (issue #76) across all four reference trackers:
    /// BPSR-ZDPS `BPSR-ZDPSLib/protos/EnumEAttrType.cs:798`
    /// (`AttrId = 10`), StarResonanceDamageCounter `algo/packet.js:104`,
    /// bpsr-logs `src-tauri/src/protocol/constants.rs:42`, resonance-logs
    /// `src-tauri/src/live/opcodes_models.rs:545`.
    pub const MONSTER_ID: i32 = 0x0A;
    /// `AttrHp` (11310) — current HP. Typed `int64` by the game's own
    /// shipped `Data/FightAttrTable.json:1098-1103` (`"EnumName":
    /// "AttrHp"`, `"IsSyncAoi": true`), so it is decoded at full width
    /// here; the oldest reference tracker narrows it to `int32`, which
    /// corrupts exactly the high-HP raid bosses this meter is for.
    ///
    /// Corroborated (issue #76): BPSR-ZDPS `EnumEAttrType.cs:1306`,
    /// StarResonanceDamageCounter `algo/packet.js:111`, bpsr-logs
    /// `src-tauri/src/protocol/constants.rs:44`, resonance-logs
    /// `src-tauri/src/live/opcodes_models.rs:585`.
    pub const HP: i32 = 0x2C2E;
    /// `AttrMaxHp` (11320) — max HP, read straight off this same attr
    /// channel by every reference tracker (none derives it from a
    /// percentage). Also `int64` per `Data/FightAttrTable.json:1138-1148`.
    /// Not to be confused with `AttrMaxHpTotal` (11321, `0x2C39`), the
    /// rollup of this attr, which no tracker reads.
    ///
    /// Corroborated (issue #76): BPSR-ZDPS `EnumEAttrType.cs:1307`,
    /// StarResonanceDamageCounter `algo/packet.js:112`, bpsr-logs
    /// `src-tauri/src/protocol/constants.rs:45`, resonance-logs
    /// `src-tauri/src/live/opcodes_models.rs:586`.
    ///
    /// Note this attr rides the *appear* packet, not the HP deltas that
    /// follow it — see `encounter::Meter::recompute_boss` for why the boss
    /// heuristic must not require it.
    pub const MAX_HP: i32 = 0x2C38;
    /// `AttrState` (11, `0x0B`) — the entity's current actor state (issue
    /// #339/#272). Only the "dead" value is meaningful to this crate; see
    /// [`entity_state_from_attrs`].
    ///
    /// Corroborated: BPSR-ZDPS `EnumEAttrType.cs:799` (`AttrState = 11`).
    /// Reference-derived, **not yet verified against live traffic** — same
    /// caveat as `SEASON_LEVEL`/`SEASON_STRENGTH` below: no capture on this
    /// build has been confirmed to carry this attr yet.
    pub const STATE: i32 = 0x0B;
    pub const PROFESSION_ID: i32 = 0xDC;
    pub const FIGHT_POINT: i32 = 0x272E;
    /// Reference-derived, **not yet verified against live traffic** (issue
    /// #15): reimplemented from BPSR-ZDPS's `EnumEAttrType.cs`
    /// (`AttrSeasonLevel = 10070`) because no packet capture was available.
    /// `FIGHT_POINT` is the only id in this module confirmed against
    /// *captured traffic* per `docs/packet-inspection.md`'s "Recording a
    /// result" convention. `MONSTER_ID`, `HP`, and `MAX_HP` (above) are
    /// confirmed by source corroboration instead — all four reference
    /// trackers plus the game's own shipped `FightAttrTable.json`, cited
    /// per-constant above (issue #76). This constant and `SEASON_STRENGTH`
    /// (below) rest on BPSR-ZDPS alone and remain single-source; re-verify
    /// them against a real capture if one ever becomes available.
    pub const SEASON_LEVEL: i32 = 0x2756;
    /// Reference-derived, **not yet verified against live traffic** (issue
    /// #15): reimplemented from BPSR-ZDPS's `EnumEAttrType.cs`
    /// (`AttrSeasonStrength = 11440`) because no packet capture was
    /// available. See `SEASON_LEVEL`'s doc comment for the full caveat.
    pub const SEASON_STRENGTH: i32 = 0x2CB0;
    /// Reference-derived, **not confirmed against live traffic** (issue
    /// #33): reimplemented from BPSR-ZDPS's `EnumEAttrType.cs`
    /// (`AttrSkillLevelIdList = 116`) because no packet capture was
    /// available. Same unverified-provenance caveat as `SEASON_LEVEL` above
    /// and `pb::IMAGINE_PROFESSION_IDS`. Re-verify against a real capture if
    /// one ever becomes available.
    pub const SKILL_LEVEL_ID_LIST: i32 = 0x74;
    /// `AttrSkillId` (100, `0x64`) — the skill an entity has just begun
    /// casting (issue #245). BPSR reports a cast by *changing this attr* on
    /// the caster rather than by sending a dedicated cast notify: BPSR-ZDPS
    /// fires `OnSkillActivated` straight off this attr's delta
    /// (`Managers/EncounterManager.cs:716`) and counts it as one cast
    /// (`EncounterManager.cs:1780`), which is the same thing its Skill Cast
    /// Timeline window is drawn from
    /// (`Windows/SkillCastTimelineWindow.cs:91-127`). The id itself is
    /// BPSR-ZDPS's decompiled `EnumEAttrType.cs:835` (`AttrSkillId = 100`).
    ///
    /// The obvious-looking alternative is not usable: `SyncClientUseSkill`
    /// (`0x43`) is a **client -> server** call, so it only ever describes
    /// the local player, and neither resonance-logs nor BPSR-ZDPS wires it
    /// into its dispatch table at all.
    ///
    /// Confirmed against this project's own captures: across four
    /// `inspect/dump-*.jsonl` files (53,371 parsed `AoiSyncDelta`
    /// messages), attr ids 100, 101, 103, 106, 108 and 111 — BPSR-ZDPS's
    /// `AttrSkillId`/`SkillStage`/`SkillLevel`/`SkillBeginTime`/
    /// `SkillStageNum`/`SkillUuid` cluster — each occur exactly 1,612
    /// times, i.e. they ride together as one skill-activation bundle at
    /// ~3% of all deltas. Only `SKILL_ID` is read here; the rest of the
    /// bundle is decoded by nobody and would add nothing the cast count
    /// needs.
    pub const SKILL_ID: i32 = 0x64;
    /// `AttrSceneBasicId` (341, `0x155`) — the scene id `tables::scene_name`
    /// is keyed by (issue #35, resolving its first item). Recovered by
    /// parsing BPSR-ZDPS's `EAttrType` enum out of the reference tool's
    /// .NET metadata; that same parse independently reproduced `NAME` (1),
    /// `MONSTER_ID` (10), `PROFESSION_ID` (220, `0xDC`), `FIGHT_POINT`
    /// (10030, `0x272E`), `HP` (11310), and `MAX_HP` (11320) byte-for-byte
    /// against the constants already above, which is what makes this value
    /// trustworthy rather than another unverified port. Confirmed against a
    /// real capture: `AttrSceneBasicId`'s raw varint decoded to `8`, and
    /// `tables.rs`'s `8 => "Asterleeds"` matches the reference tool's own
    /// `SceneTable["8"].Name`.
    ///
    /// Rides `opcode::ENTER_SCENE`'s attr channel (`decode::on_enter_scene`),
    /// not `SyncContainerData` — see `pb::CharSerialize`'s doc comment for
    /// why the old `SceneData.level_map_id` path was removed.
    ///
    /// Four sibling ids from the same enum block are not modeled here
    /// because nothing needs them yet: `AttrSceneName` (340, `0x154` — a
    /// length-prefixed UTF-8 string, *not* a varint — do not decode it with
    /// `decode_varint_*`), `AttrSceneUuid` (342, `0x156`, the per-instance
    /// world uuid), `AttrSceneChannel` (343, `0x157`, the instance/channel
    /// number), and `AttrSceneLevelId` (345, `0x159`).
    pub const SCENE_BASIC_ID: i32 = 0x155;

    /// `AttrPos` (52, `0x34`) — issue #286: the entity's current world
    /// position, a 3-float `(x, y, z)` submessage. Wire-confirmed against
    /// this project's own captures: fields 1-3, wire type 5 (`fixed32`) —
    /// see [`crate::attrs::decode_position`] for the parse and both real
    /// dumps' sample bytes in that function's tests.
    ///
    /// The name and "current" (as opposed to "target") semantics are
    /// BPSR-ZDPS's own: `BPSR-ZDPSLib/protos/EnumEAttrType.cs:815`
    /// (`AttrPos = 52`), read straight into `entity.SetPosition(...)` at
    /// `Managers/EncounterManager.cs:773`, and decoded as a `Vec3`
    /// submessage in `Managers/MessageManager.cs`'s attr-parse switch —
    /// matching the fixed32-triple shape observed here. This is the same
    /// trust chain `SCENE_BASIC_ID` (above) uses: single-source, but from
    /// the enum block that has already reproduced half a dozen of this
    /// module's other constants byte-for-byte (issue #76). Not
    /// independently confirmed by a live control run that visibly moves an
    /// entity and diffs the two ids (issue #286's own open item) — see
    /// [`TARGET_POSITION`].
    pub const POSITION: i32 = 0x34;

    /// `AttrTargetPos` (53, `0x35`) — issue #286's sibling to [`POSITION`]:
    /// the identical 3-float wire shape. BPSR-ZDPS's enum names it the
    /// entity's *target* position rather than a duplicate of `AttrPos`
    /// (`EnumEAttrType.cs:816`, `AttrTargetPos = 53`).
    ///
    /// That label is corroborated, not just repeated, by this project's own
    /// captures: for a moving entity the two payloads are close but not
    /// identical (e.g. one player observed at `(-22.97, 115.56, 114.55)`
    /// vs. `(-22.07, 115.53, 113.76)` in the same session), which is what
    /// "where I am" vs. "where I'm headed" looks like on the wire — not a
    /// redundant echo of the same value. Issue #286's original evidence also
    /// found the two payloads byte-identical for one uid, consistent with
    /// current == target at spawn. Same single-source caveat as `POSITION`
    /// above: the id *values* are trustworthy (issue #76's reproduction
    /// chain), but the current-vs-target *label* has not been independently
    /// re-derived from a live control run.
    pub const TARGET_POSITION: i32 = 0x35;

    /// `AttrSkillStage` (101, `0x65`) — issue #287, **medium** confidence.
    /// Named from this module's own [`SKILL_ID`] doc comment, which already
    /// identified the 100/101/103/106/108/111 cluster riding together
    /// (`EnumEAttrType.cs:836`, `AttrSkillStage = 101`) but left this one
    /// undecoded until now.
    ///
    /// Every real-capture sample seen so far (two independent dumps) is a
    /// full 10-byte two's-complement varint decoding to `-1`. BPSR-ZDPS's
    /// own `MessageManager.cs` attr-parse switch has no explicit case for
    /// this id, so it falls to the generic `default: reader.ReadInt32()`
    /// arm — i.e. it is a genuine signed `int32`, and `-1` is a real
    /// (probably "no active stage") sentinel, not a corrupt or
    /// out-of-range value. See [`decode_varint_i32_truncating`], which this
    /// id needs instead of the stricter [`decode_varint_i32`].
    pub const SKILL_STAGE: i32 = 0x65;

    /// `AttrSkillLevel` (103, `0x67`) — issue #287, **medium** confidence;
    /// see [`SKILL_STAGE`]'s doc comment for the shared cluster/source
    /// citation (`EnumEAttrType.cs:838`, `AttrSkillLevel = 103`). Observed
    /// as small positive values (`1`, `30`) across two independent
    /// captures — also `default`-cased in BPSR-ZDPS, hence
    /// [`decode_varint_i32_truncating`] here too.
    pub const SKILL_LEVEL: i32 = 0x67;

    /// `AttrSkillBeginTime` (106, `0x6a`) — issue #287, **high** confidence:
    /// the one id in this cluster with independently verified semantics.
    /// Its varint decodes to a Unix-epoch-milliseconds value matching the
    /// capture's own wall-clock date (e.g. `1,787,022,297,550` ms ≈
    /// 2026-08-18, and again `1,787,014,557,724` ms in a second, unrelated
    /// dump) — a real cast *begin* time, not the packet-arrival time every
    /// other event in this crate is stamped with, which network jitter and
    /// reassembly delay can skew.
    ///
    /// `EnumEAttrType.cs:841` (`AttrSkillBeginTime = 106`); explicitly
    /// cased as `reader.ReadInt64()` in `MessageManager.cs` (unlike its
    /// `default`-cased siblings above/below), so [`decode_varint_i64`] —
    /// not the truncating `i32` helper — is the correct width here.
    pub const SKILL_BEGIN_TIME: i32 = 0x6a;

    /// `AttrSkillStageNum` (108, `0x6c`) — issue #287, **medium**
    /// confidence; see [`SKILL_STAGE`]'s doc comment for the shared
    /// cluster/source citation (`EnumEAttrType.cs:843`,
    /// `AttrSkillStageNum = 108`). Observed decoding consistently to `2`
    /// across two independent captures.
    pub const SKILL_STAGE_NUM: i32 = 0x6c;

    /// `AttrSkillUuid` (111, `0x6f`) — issue #287, **medium** confidence;
    /// see [`SKILL_STAGE`]'s doc comment for the shared cluster/source
    /// citation (`EnumEAttrType.cs:846`, `AttrSkillUuid = 111`). Despite the
    /// name this is not an entity uuid in [`crate::event::uid_of`]'s sense —
    /// the observed values (`1,610,613,212` / `1,610,613,327`) don't land on
    /// plausible entity uids when unpacked that way. Treated as an opaque
    /// per-cast identifier until proven otherwise.
    pub const SKILL_UUID: i32 = 0x6f;
    /// `AttrShieldList` (60050, issue #338) — a repeated `ShieldInfo`
    /// (uuid=1, shieldType=2, value=3, initialValue=4, maxValue=5, all
    /// varint fields) tracking this entity's active shield instances.
    /// Corroborated: BPSR-ZDPS `BPSR-ZDPSLib/protos/EnumEAttrType.cs:2086`
    /// (`AttrShieldList = 60050`) and `protos/StruShieldInfo.cs`'s field
    /// layout (`Managers/MessageManager.cs:885-903` decodes it exactly the
    /// way [`decode_shield_total`] does here — a bare loop of
    /// length-prefixed `ShieldInfo` submessages, not one message with a
    /// repeated field).
    pub const SHIELD_LIST: i32 = 60050;
}

/// protobuf varint → `u64`; `None` on empty/malformed input. The widest
/// lossless form — the typed helpers below narrow it with checked casts so an
/// out-of-range value is rejected rather than silently truncated or
/// reinterpreted.
pub fn decode_varint_u64(raw: &[u8]) -> Option<u64> {
    prost::encoding::decode_varint(&mut Cursor::new(raw)).ok()
}

/// protobuf varint → `i64`; `None` on empty/malformed input or a value above
/// `i64::MAX` (which would otherwise read back as a negative number).
pub fn decode_varint_i64(raw: &[u8]) -> Option<i64> {
    decode_varint_u64(raw).and_then(|v| i64::try_from(v).ok())
}

/// protobuf varint → `i32`; `None` on empty/malformed input or a value
/// outside `i32` range (truncating it would yield a plausible-but-wrong id).
pub fn decode_varint_i32(raw: &[u8]) -> Option<i32> {
    decode_varint_u64(raw).and_then(|v| i32::try_from(v).ok())
}

/// protobuf varint → `u32`; `None` on empty/malformed input or a value
/// outside `u32` range (truncating it would yield a plausible-but-wrong id).
pub fn decode_varint_u32(raw: &[u8]) -> Option<u32> {
    decode_varint_u64(raw).and_then(|v| u32::try_from(v).ok())
}

/// protobuf varint → `i32`, reinterpreting the low 32 bits as two's
/// complement rather than rejecting a value that doesn't fit (contrast
/// [`decode_varint_i32`], which is the right choice for an id: an
/// out-of-range value there really is corrupt). Some attrs in this module
/// are genuine protobuf `int32` fields (issue #287's `attr_id::SKILL_STAGE`
/// cluster) whose negative values still get the wire's full 10-byte
/// two's-complement varint encoding — `0xff` x9 then `0x01` is `-1`, a real
/// sentinel value, not a corrupt out-of-range id. `None` only on a truly
/// malformed/empty varint.
pub fn decode_varint_i32_truncating(raw: &[u8]) -> Option<i32> {
    decode_varint_u64(raw).map(|v| v as i64 as i32)
}

/// `EActorState::ActorStateDead` (9) — the only [`attr_id::STATE`] value
/// this crate treats as "dead"; every other actor state (skill, stiff,
/// born, resurrection animation, etc.) reads as alive (issue #339/#272).
/// BPSR-ZDPS `EnumEActorState.cs` (`ActorStateDead = 9`).
const ACTOR_STATE_DEAD: i32 = 9;

/// Decodes [`attr_id::STATE`] off an entity's `Attr` list (issue #339/#272)
/// into `is_dead`: `Some(true)` when the decoded state equals
/// [`ACTOR_STATE_DEAD`], `Some(false)` for any other decoded state, `None`
/// when the attr is absent or malformed. `decode::on_aoi_sync_delta` treats
/// `None` as "no signal" rather than "alive" — matching this module's
/// absent-is-not-a-value convention elsewhere (e.g. `cast_skill_id_from_attrs`).
pub fn entity_state_from_attrs(attrs: &[pb::Attr]) -> Option<bool> {
    attrs
        .iter()
        .find(|attr| attr.id == attr_id::STATE && !attr.raw_data.is_empty())
        .and_then(|attr| decode_varint_i32(&attr.raw_data))
        .map(|state| state == ACTOR_STATE_DEAD)
}

/// `raw_data` for [`attr_id::POSITION`] / [`attr_id::TARGET_POSITION`]
/// (issue #286) → a `[x, y, z]` world-position triple. The wire shape is a
/// tiny 3-field submessage, fields 1-3, wire type 5 (`fixed32`) — a
/// textbook `{ float x = 1; float y = 2; float z = 3; }` (see
/// [`attr_id::POSITION`]'s doc comment for the confirming evidence). Fields
/// are walked generically rather than assumed to sit at a fixed 15-byte
/// offset: proto3 omits a field whose value is the default `0.0`, so a real
/// payload can be shorter than 3 floats, and wire field order is not
/// guaranteed. `None` when no field 1-3 was seen at all (an empty or
/// entirely off-cluster payload), or on any malformed varint/truncated
/// float — never panics. A wire type other than `fixed32` on a fields-1-3
/// tag is malformed for this shape and also yields `None`, rather than
/// guessing how to interpret it; a tag for any *other* field number, of any
/// wire type, is genuinely unknown rather than malformed, so its payload is
/// skipped generically (via `prost::encoding::skip_field`) instead of
/// aborting the whole parse.
pub fn decode_position(raw: &[u8]) -> Option<[f32; 3]> {
    let mut cursor = Cursor::new(raw);
    let mut pos = [0.0f32; 3];
    let mut seen = false;
    while (cursor.position() as usize) < raw.len() {
        let tag = prost::encoding::decode_varint(&mut cursor).ok()?;
        let field = tag >> 3;
        let wire_type = tag & 0x7;
        let is_target_field = (1..=3).contains(&field);
        if wire_type != 5 {
            // A fields-1-3 tag with a wire type other than fixed32 is
            // malformed for this shape (see doc comment above) — bail
            // rather than guess. Any other field is genuinely unknown, so
            // skip its payload generically instead of aborting the whole
            // parse over data we don't care about.
            if is_target_field {
                return None;
            }
            let wire_type = prost::encoding::WireType::try_from(wire_type).ok()?;
            prost::encoding::skip_field(
                wire_type,
                field as u32,
                &mut cursor,
                prost::encoding::DecodeContext::default(),
            )
            .ok()?;
            continue;
        }
        let start = cursor.position() as usize;
        let end = start.checked_add(4)?;
        let bytes = raw.get(start..end)?;
        let value = f32::from_le_bytes(bytes.try_into().expect("checked-length slice"));
        cursor.set_position(end as u64);
        if is_target_field {
            pos[(field - 1) as usize] = value;
            seen = true;
        }
    }
    seen.then_some(pos)
}

/// `raw_data` for [`attr_id::SHIELD_LIST`] (issue #338) → this entity's
/// total *current* shield value, summed across every active shield
/// instance. Unlike [`attr_id::SKILL_LEVEL_ID_LIST`]'s `pb::SkillLevelIdList`
/// wrapper, this attr's raw bytes are *not* one message with a repeated
/// field: BPSR-ZDPS reads it as a bare loop of `len = ReadLength(); ReadMessage
/// (shield)` straight off the attr's own byte span
/// (`MessageManager.cs:885-903`) — each shield instance is its own
/// length-prefixed `ShieldInfo` submessage with no enclosing tag. This walks
/// the same shape: repeated length-prefixed chunks until `raw` is exhausted,
/// summing field 3 (`value`, the shield's current remaining amount — not
/// `initialValue`/`maxValue`, which never shrink as the shield absorbs
/// damage) out of each chunk.
///
/// `Some(0)` for an empty `raw` — this attr's own `isNoValue -> new
/// List<ShieldInfo>()` wire convention, i.e. *known* to have no shields —
/// and `None` only when a non-empty `raw` is malformed (a length prefix that
/// overruns the buffer, or a truncated varint).
pub fn decode_shield_total(raw: &[u8]) -> Option<i64> {
    if raw.is_empty() {
        return Some(0);
    }
    let mut cursor = Cursor::new(raw);
    let mut total: i64 = 0;
    while (cursor.position() as usize) < raw.len() {
        let len = prost::encoding::decode_varint(&mut cursor).ok()?;
        let start = cursor.position() as usize;
        let end = start.checked_add(len as usize)?;
        let chunk = raw.get(start..end)?;
        total += decode_shield_chunk_value(chunk)?;
        cursor.set_position(end as u64);
    }
    Some(total)
}

/// One `ShieldInfo` submessage → its `value` field (tag 3, varint). Every
/// field on this message is a varint (see [`attr_id::SHIELD_LIST`]'s doc
/// comment), so a non-varint wire type is unrecognized shape for this
/// message and its payload is skipped generically rather than guessed at —
/// same convention as [`decode_position`].
fn decode_shield_chunk_value(chunk: &[u8]) -> Option<i64> {
    let mut cursor = Cursor::new(chunk);
    let mut value: i64 = 0;
    while (cursor.position() as usize) < chunk.len() {
        let tag = prost::encoding::decode_varint(&mut cursor).ok()?;
        let field = tag >> 3;
        let wire_type = prost::encoding::WireType::try_from(tag & 0x7).ok()?;
        if wire_type != prost::encoding::WireType::Varint {
            prost::encoding::skip_field(
                wire_type,
                field as u32,
                &mut cursor,
                prost::encoding::DecodeContext::default(),
            )
            .ok()?;
            continue;
        }
        let v = prost::encoding::decode_varint(&mut cursor).ok()?;
        if field == 3 {
            value = v as i64;
        }
    }
    Some(value)
}

/// Issue #287's skill-cast metadata cluster, decoded alongside
/// [`cast_skill_id_from_attrs`] — the same `AoiSyncDelta` that carries
/// [`attr_id::SKILL_ID`] also carries some or all of these siblings. Every
/// field is independently optional: a cast attr delta is not guaranteed to
/// carry every id in the cluster.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SkillCastMetadata {
    /// [`attr_id::SKILL_ID`], decoded in the same pass as the rest of this
    /// cluster — see [`cast_skill_id_from_attrs`] for the id's own doc
    /// comment (same decode rules: strict [`decode_varint_i32`], zero
    /// rejected as "no skill").
    pub skill_id: Option<i32>,
    /// [`attr_id::SKILL_STAGE`] — medium confidence, see that constant's
    /// doc comment.
    pub skill_stage: Option<i32>,
    /// [`attr_id::SKILL_LEVEL`] — medium confidence.
    pub skill_level: Option<i32>,
    /// [`attr_id::SKILL_BEGIN_TIME`] — high confidence, the one
    /// independently timestamp-verified id in the cluster. Milliseconds
    /// since the Unix epoch.
    pub skill_begin_time_ms: Option<i64>,
    /// [`attr_id::SKILL_STAGE_NUM`] — medium confidence.
    pub skill_stage_num: Option<i32>,
    /// [`attr_id::SKILL_UUID`] — medium confidence; not a
    /// [`crate::event::uid_of`]-shaped entity uuid — see that constant's
    /// doc comment.
    pub skill_uuid: Option<i32>,
}

/// Decodes [`attr_id::SKILL_ID`] plus issue #287's skill-cast metadata
/// cluster off an entity's `Attr` list in a single pass — `decode::
/// on_aoi_sync_delta` used to call this alongside a separate
/// [`cast_skill_id_from_attrs`] walk of the same slice; folded together here
/// so the slice is only walked once. Every field stays `None` when its id
/// is absent or malformed, matching this module's zero-is-absent /
/// malformed-is-absent convention rather than erroring.
pub fn skill_cast_metadata_from_attrs(attrs: &[pb::Attr]) -> SkillCastMetadata {
    let mut out = SkillCastMetadata::default();
    for attr in attrs {
        if attr.raw_data.is_empty() || attr.id == 0 {
            continue;
        }
        match attr.id {
            attr_id::SKILL_ID => {
                out.skill_id = decode_varint_i32(&attr.raw_data).filter(|id| *id != 0);
            }
            attr_id::SKILL_STAGE => {
                out.skill_stage = decode_varint_i32_truncating(&attr.raw_data);
            }
            attr_id::SKILL_LEVEL => {
                out.skill_level = decode_varint_i32_truncating(&attr.raw_data);
            }
            attr_id::SKILL_BEGIN_TIME => {
                out.skill_begin_time_ms = decode_varint_i64(&attr.raw_data);
            }
            attr_id::SKILL_STAGE_NUM => {
                out.skill_stage_num = decode_varint_i32_truncating(&attr.raw_data);
            }
            attr_id::SKILL_UUID => {
                out.skill_uuid = decode_varint_i32_truncating(&attr.raw_data);
            }
            _ => {}
        }
    }
    out
}

/// Set once a nonzero `remodel_level` (tier) has been observed on any
/// decoded skill, so `decode_skill_ids` logs the first live sighting
/// exactly once per process rather than once per packet. Issues #169/#170
/// thread this field through as "tier", but its value semantics (0- vs.
/// 1-based, whether 5 is really the max — see `ui::IMAGINE_MAX_TIER`'s doc
/// comment) are inferred from BPSR-ZDPS's naming, not yet observed in a
/// real capture (every sample in `dump-2976-boss-fight.jsonl.zst` decoded
/// to `remodel_level == 0`) — the first real nonzero value is worth a
/// durable log line so a live run can confirm the true range.
static NONZERO_TIER_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// `raw_data` for `attr_id::SKILL_LEVEL_ID_LIST` → the equipped skills, in
/// wire order, as `(skill_id, remodel_level)` pairs, with `skill_id == 0`
/// entries dropped (matching the crate's zero-is-absent convention).
/// `remodel_level` is BPSR-ZDPS's `Tier` (`zdps/BPSR-ZDPS/DataTypes/Skills.cs:28`,
/// `Tier = skillLevelInfo.RemodelLevel`) — see `pb::SkillLevelInfo`'s doc
/// comment for the full wire-tag correspondence (issues #169/#170). `None`
/// on malformed input (prost decode failure), per this module's
/// non-panicking convention — `player_info_from_attrs` treats `None` the
/// same as "no ids".
///
/// Uses `pb::SkillLevelIdList`, whose wrapper tag is an unverified constant
/// (see its doc comment). If that tag is wrong, a non-empty `raw` still
/// parses as *zero* skills rather than erroring — a silent, non-crashing
/// miss (issue #33's open question #2) — so that specific case is logged at
/// `debug` to keep the failure diagnosable from a user's log.
pub fn decode_skill_ids(raw: &[u8]) -> Option<Vec<(i32, i32)>> {
    let list = pb::SkillLevelIdList::decode(raw).ok()?;
    let ids: Vec<(i32, i32)> = list
        .skills
        .into_iter()
        .filter(|s| s.skill_id != 0)
        .map(|s| {
            if s.remodel_level != 0
                && NONZERO_TIER_LOGGED
                    .compare_exchange(
                        false,
                        true,
                        std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
            {
                log::debug!(
                    "attr_id::SKILL_LEVEL_ID_LIST: first observed nonzero remodel_level \
                     (tier) = {} for skill_id {} — live confirmation for issues #169/#170's \
                     tier range (see ui::IMAGINE_MAX_TIER's doc comment)",
                    s.remodel_level,
                    s.skill_id
                );
            }
            (s.skill_id, s.remodel_level)
        })
        .collect();
    if !raw.is_empty() && ids.is_empty() {
        log::debug!(
            "attr_id::SKILL_LEVEL_ID_LIST: {} raw bytes decoded to zero skill ids; \
             the wrapper tag (pb::SkillLevelIdList) may be wrong",
            raw.len()
        );
    }
    Some(ids)
}

/// `raw_data` for a name attr is the string bytes behind a protobuf varint
/// length prefix. When that prefix is self-consistent it is honoured (so a
/// 128+ byte name, whose prefix is two bytes, keeps no stray byte); otherwise
/// a single leading byte is dropped, matching the servers that prepend one
/// opaque tag byte. Invalid UTF-8 or an empty result → `None`, so the caller
/// falls back to `Player <uid>` instead of rendering a blank name.
pub fn decode_name(raw: &[u8]) -> Option<String> {
    let mut cursor = Cursor::new(raw);
    let bytes = match prost::encoding::decode_varint(&mut cursor) {
        Ok(len) => {
            let rest = &raw[cursor.position() as usize..];
            if len == rest.len() as u64 {
                rest
            } else {
                raw.get(1..)?
            }
        }
        Err(_) => raw.get(1..)?,
    };
    if bytes.is_empty() {
        return None;
    }
    std::str::from_utf8(bytes).ok().map(str::to_string)
}

/// Builds a `PlayerInfo` from an entity's `Attr` list, reading `NAME`,
/// `PROFESSION_ID`, `FIGHT_POINT` (ability score), `SEASON_LEVEL`, and
/// `SEASON_STRENGTH`. Empty `raw_data` and `id == 0` are always skipped
/// outright — no decoding, no sink call. Every other id, when `sink` is set
/// (issue #25 diagnostic mode), is reported via `InspectSink::on_attr` —
/// known ids (the five above) as well as unknown ones, each tagged with
/// whether we decode it, instead of only unknown ids reaching the sink
/// (slice B widened this from an unknowns-only hook).
/// The skill id an attr delta says this entity has begun casting (issue
/// #245), or `None` when the delta carries no [`attr_id::SKILL_ID`] — which
/// is the overwhelmingly common case, since most deltas are HP or position.
///
/// Non-panicking like everything else in this module: an empty `raw_data`,
/// an `id == 0` entry, a malformed varint, or a value outside `i32` are all
/// skipped rather than propagated. A zero skill id is rejected too — the
/// attr channel uses `0` for "no skill", and `bpsr_meter`'s per-skill maps
/// are keyed by real ids.
pub fn cast_skill_id_from_attrs(attrs: &[pb::Attr]) -> Option<i32> {
    attrs
        .iter()
        .find(|attr| attr.id == attr_id::SKILL_ID && !attr.raw_data.is_empty())
        .and_then(|attr| decode_varint_i32(&attr.raw_data))
        .filter(|id| *id != 0)
}

pub fn player_info_from_attrs(
    entity: EntityId,
    attrs: &[pb::Attr],
    sink: Option<&dyn InspectSink>,
) -> PlayerInfo {
    let uid = entity.display_uid();
    let mut name = None;
    let mut class = None;
    let mut ability_score = None;
    let mut season_level = None;
    let mut season_strength = None;
    let mut skill_ids = Vec::new();
    let mut position = None;
    let mut target_position = None;
    let mut shield = None;
    for attr in attrs {
        if attr.id == 0 || (attr.raw_data.is_empty() && attr.id != attr_id::SHIELD_LIST) {
            continue;
        }
        if let Some(sink) = sink {
            let known = matches!(
                attr.id,
                attr_id::NAME
                    | attr_id::PROFESSION_ID
                    | attr_id::FIGHT_POINT
                    | attr_id::SEASON_LEVEL
                    | attr_id::SEASON_STRENGTH
                    | attr_id::SKILL_LEVEL_ID_LIST
                    | attr_id::POSITION
                    | attr_id::TARGET_POSITION
                    | attr_id::SHIELD_LIST
            );
            sink.on_attr(uid, attr.id, &attr.raw_data, known);
        }
        match attr.id {
            attr_id::NAME => {
                if let Some(n) = decode_name(&attr.raw_data) {
                    name = Some(n);
                }
            }
            attr_id::PROFESSION_ID => {
                if let Some(id) = decode_varint_i32(&attr.raw_data) {
                    // An Imagine transform id (issue #37) yields `None` here
                    // rather than overwriting `class` with `Unknown` — see
                    // `pb::class_of_profession_id`'s doc comment.
                    if let Some(c) = pb::class_of_profession_id(id) {
                        class = Some(c);
                    }
                }
            }
            attr_id::FIGHT_POINT => {
                // A raw value of 0 is the wire default when the server hasn't
                // populated the field, not a real ability score — treat it as
                // absent, matching `SyncContainerData`'s `char_base.fight_point
                // > 0` guard in decode.rs.
                if let Some(v) = decode_varint_u32(&attr.raw_data).filter(|&v| v > 0) {
                    ability_score = Some(v);
                }
            }
            attr_id::SEASON_LEVEL => {
                // Same absent-vs-zero treatment as FIGHT_POINT above.
                if let Some(v) = decode_varint_u32(&attr.raw_data).filter(|&v| v > 0) {
                    season_level = Some(v);
                }
            }
            attr_id::SEASON_STRENGTH => {
                if let Some(v) = decode_varint_u32(&attr.raw_data).filter(|&v| v > 0) {
                    season_strength = Some(v);
                }
            }
            attr_id::SKILL_LEVEL_ID_LIST => {
                if let Some(ids) = decode_skill_ids(&attr.raw_data) {
                    skill_ids = ids;
                }
            }
            attr_id::POSITION => {
                if let Some(p) = decode_position(&attr.raw_data) {
                    position = Some(p);
                }
            }
            attr_id::TARGET_POSITION => {
                if let Some(p) = decode_position(&attr.raw_data) {
                    target_position = Some(p);
                }
            }
            attr_id::SHIELD_LIST => {
                if let Some(s) = decode_shield_total(&attr.raw_data) {
                    shield = Some(s);
                }
            }
            _ => {}
        }
    }
    PlayerInfo {
        entity,
        uid,
        name,
        class,
        ability_score,
        season_level,
        season_strength,
        skill_ids,
        position,
        target_position,
        shield,
    }
}

/// Builds an `EnemyHp` from an entity's `Attr` list, reading `HP`, `MAX_HP`,
/// and `MONSTER_ID`. Empty `raw_data` and `id == 0` are always skipped
/// outright — no decoding, no sink call. Every other id, when `sink` is set
/// (issue #25 diagnostic mode), is reported via `InspectSink::on_attr` —
/// known ids (the three above) as well as unknown ones, each tagged with
/// whether we decode it, exactly as `player_info_from_attrs` does: an attr
/// id is no less worth discovering for sitting on an enemy entity.
pub fn enemy_hp_from_attrs(
    entity: EntityId,
    attrs: &[pb::Attr],
    now_ms: u64,
    sink: Option<&dyn InspectSink>,
) -> EnemyHp {
    let uid = entity.display_uid();
    let mut curr_hp = None;
    let mut max_hp = None;
    let mut monster_id = None;
    let mut position = None;
    let mut target_position = None;
    for attr in attrs {
        if attr.raw_data.is_empty() || attr.id == 0 {
            continue;
        }
        if let Some(sink) = sink {
            let known = matches!(
                attr.id,
                attr_id::HP
                    | attr_id::MAX_HP
                    | attr_id::MONSTER_ID
                    | attr_id::POSITION
                    | attr_id::TARGET_POSITION
            );
            sink.on_attr(uid, attr.id, &attr.raw_data, known);
        }
        match attr.id {
            attr_id::HP => {
                if let Some(v) = decode_varint_u64(&attr.raw_data) {
                    curr_hp = Some(v);
                }
            }
            attr_id::MAX_HP => {
                if let Some(v) = decode_varint_u64(&attr.raw_data) {
                    max_hp = Some(v);
                }
            }
            attr_id::MONSTER_ID => {
                if let Some(v) = decode_varint_u32(&attr.raw_data) {
                    monster_id = Some(v);
                }
            }
            attr_id::POSITION => {
                if let Some(p) = decode_position(&attr.raw_data) {
                    position = Some(p);
                }
            }
            attr_id::TARGET_POSITION => {
                if let Some(p) = decode_position(&attr.raw_data) {
                    target_position = Some(p);
                }
            }
            _ => {}
        }
    }
    EnemyHp {
        entity,
        uid,
        curr_hp,
        max_hp,
        monster_id,
        timestamp_ms: now_ms,
        position,
        target_position,
    }
}

/// Reads `SCENE_BASIC_ID` off an `EnterScene` payload's attr list (issue
/// #35). Empty `raw_data` and `id == 0` are skipped outright, same as
/// `player_info_from_attrs`/`enemy_hp_from_attrs`; every other non-empty,
/// nonzero id — including the string-valued `AttrSceneName` (340) — still
/// reaches `sink` (tagged `known` only for `SCENE_BASIC_ID`) so slice A can
/// surface the unmodeled scene attrs instead of them silently vanishing.
///
/// There is no entity uid for a scene attr (it isn't attached to any
/// entity), so `sink.on_attr` is called with `uid = 0` — a scene event has
/// no other identity to report it under.
///
/// A raw value of `0` is proto3's unset-scalar wire value, not a real scene
/// id — treated as absent, matching `FIGHT_POINT`'s zero-is-absent rule
/// above.
pub fn scene_id_from_attrs(attrs: &[pb::Attr], sink: Option<&dyn InspectSink>) -> Option<u32> {
    let mut scene_id = None;
    for attr in attrs {
        if attr.raw_data.is_empty() || attr.id == 0 {
            continue;
        }
        if let Some(sink) = sink {
            sink.on_attr(
                0,
                attr.id,
                &attr.raw_data,
                attr.id == attr_id::SCENE_BASIC_ID,
            );
        }
        if attr.id == attr_id::SCENE_BASIC_ID
            && let Some(v) = decode_varint_u32(&attr.raw_data).filter(|&v| v > 0)
        {
            scene_id = Some(v);
        }
    }
    scene_id
}

#[cfg(test)]
mod tests {
    /// A player's whole-uuid identity for display uid `u` — these tests
    /// only care about the attr walk, so the canonical reconstruction is
    /// exactly as good as a captured uuid here.
    fn pid(u: i64) -> EntityId {
        EntityId::from_display_uid(u, crate::event::EntityKind::Player)
    }

    /// The monster counterpart of `pid`.
    fn mid(u: i64) -> EntityId {
        EntityId::from_display_uid(u, crate::event::EntityKind::Monster)
    }

    use super::*;

    #[test]
    fn name_attr_drops_stray_first_byte() {
        let raw = [0xFFu8, b'H', b'i'];
        assert_eq!(decode_name(&raw), Some("Hi".to_string()));
    }

    #[test]
    fn name_attr_invalid_utf8_is_none() {
        let raw = [0x00u8, 0xFF, 0xFE];
        assert_eq!(decode_name(&raw), None);
    }

    #[test]
    fn varint_hp_decodes() {
        let mut buf = Vec::new();
        prost::encoding::encode_varint(123456u64, &mut buf);
        assert_eq!(decode_varint_i64(&buf), Some(123456));
    }

    /// Issue #245: the cast attr is absent from the overwhelming majority
    /// of deltas (HP and position changes), so the common answer is `None`
    /// and it must not be an error.
    #[test]
    fn a_delta_without_the_skill_attr_reports_no_cast() {
        let attrs = vec![pb::Attr {
            id: attr_id::HP,
            raw_data: vec![0x01],
        }];
        assert_eq!(cast_skill_id_from_attrs(&attrs), None);
        assert_eq!(cast_skill_id_from_attrs(&[]), None);
    }

    #[test]
    fn an_empty_or_malformed_skill_attr_reports_no_cast() {
        assert_eq!(
            cast_skill_id_from_attrs(&[pb::Attr {
                id: attr_id::SKILL_ID,
                raw_data: Vec::new(),
            }]),
            None
        );
        // A varint past `i32` is rejected outright, not truncated.
        let mut raw_data = Vec::new();
        prost::encoding::encode_varint(0x1_0000_0001, &mut raw_data);
        assert_eq!(
            cast_skill_id_from_attrs(&[pb::Attr {
                id: attr_id::SKILL_ID,
                raw_data,
            }]),
            None
        );
    }

    #[test]
    fn empty_raw_data_skipped() {
        let attrs = vec![pb::Attr {
            id: attr_id::HP,
            raw_data: Vec::new(),
        }];
        let hp = enemy_hp_from_attrs(mid(1), &attrs, 0, None);
        assert_eq!(hp.curr_hp, None);
    }

    fn varint(v: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        prost::encoding::encode_varint(v, &mut buf);
        buf
    }

    #[test]
    fn varint_out_of_i32_range_is_rejected_not_truncated() {
        let raw = varint(0x1_0000_0001);
        assert_eq!(decode_varint_i32(&raw), None);
    }

    #[test]
    fn varint_above_i64_max_is_rejected_not_reinterpreted() {
        let raw = varint(u64::MAX);
        assert_eq!(decode_varint_i64(&raw), None);
    }

    #[test]
    fn huge_hp_varint_is_not_clamped_to_zero() {
        let attrs = vec![pb::Attr {
            id: attr_id::HP,
            raw_data: varint(u64::MAX),
        }];
        let hp = enemy_hp_from_attrs(mid(1), &attrs, 0, None);
        assert_eq!(hp.curr_hp, Some(u64::MAX));
    }

    #[test]
    fn out_of_range_profession_id_yields_no_class() {
        let attrs = vec![pb::Attr {
            id: attr_id::PROFESSION_ID,
            raw_data: varint(0x1_0000_0001),
        }];
        assert_eq!(player_info_from_attrs(pid(1), &attrs, None).class, None);
    }

    // -- Imagine profession ids (issue #37) --------------------------------

    #[test]
    fn imagine_profession_id_yields_no_class() {
        let attrs = vec![pb::Attr {
            id: attr_id::PROFESSION_ID,
            raw_data: varint(8), // Dorothy
        }];
        assert_eq!(player_info_from_attrs(pid(1), &attrs, None).class, None);
    }

    #[test]
    fn imagine_profession_id_does_not_clobber_a_class_read_earlier_in_the_same_attr_list() {
        let attrs = vec![
            pb::Attr {
                id: attr_id::PROFESSION_ID,
                raw_data: varint(1), // Stormblade
            },
            pb::Attr {
                id: attr_id::PROFESSION_ID,
                raw_data: varint(8), // Dorothy (Imagine) — must not overwrite
            },
        ];
        assert_eq!(
            player_info_from_attrs(pid(1), &attrs, None).class,
            Some(pb::Class::Stormblade)
        );
    }

    /// issue #76: the whole monster attr triple, built from the reference
    /// trackers' own field numbering, decodes to a fully-populated
    /// `EnemyHp`. The literal ids are deliberate — this test pins the
    /// constants' *values*, so renumbering `attr_id` fails here rather
    /// than silently returning `None` for every monster on the wire.
    ///
    /// `AttrId = 10` / `AttrHp = 11310` / `AttrMaxHp = 11320`, corroborated
    /// by all four reference trackers; see the `attr_id` doc comments for
    /// the per-repo file/line citations.
    #[test]
    fn monster_attr_triple_decodes_to_a_full_enemy_hp() {
        let attrs = vec![
            pb::Attr {
                id: 0x0A,
                raw_data: varint(103),
            },
            pb::Attr {
                id: 0x2C2E,
                raw_data: varint(4_200_000),
            },
            pb::Attr {
                id: 0x2C38,
                raw_data: varint(9_000_000),
            },
        ];

        let hp = enemy_hp_from_attrs(mid(42), &attrs, 7, None);

        assert_eq!(hp.uid, 42);
        assert_eq!(hp.monster_id, Some(103));
        assert_eq!(hp.curr_hp, Some(4_200_000));
        assert_eq!(hp.max_hp, Some(9_000_000));
        assert_eq!(hp.timestamp_ms, 7);
    }

    /// The game's own `FightAttrTable.json` types `AttrHp`/`AttrMaxHp` as
    /// `int64`, and BPSR-ZDPS reads both with `ReadInt64()`. A raid boss's
    /// max HP genuinely exceeds `i32`, so narrowing here (as the oldest
    /// reference tracker does with `reader.int32()`) would corrupt exactly
    /// the fights this meter exists for.
    #[test]
    fn max_hp_above_i32_range_decodes_losslessly() {
        let attrs = vec![pb::Attr {
            id: attr_id::MAX_HP,
            raw_data: varint(9_000_000_000),
        }];
        assert_eq!(
            enemy_hp_from_attrs(mid(1), &attrs, 0, None).max_hp,
            Some(9_000_000_000)
        );
    }

    #[test]
    fn out_of_range_monster_id_is_rejected() {
        let attrs = vec![pb::Attr {
            id: attr_id::MONSTER_ID,
            raw_data: varint(u64::from(u32::MAX) + 1),
        }];
        assert_eq!(
            enemy_hp_from_attrs(mid(1), &attrs, 0, None).monster_id,
            None
        );
    }

    #[test]
    fn single_byte_name_is_none_not_empty_string() {
        assert_eq!(decode_name(&[0x00]), None);
    }

    #[test]
    fn long_name_keeps_no_stray_prefix_byte() {
        let name = "a".repeat(130);
        let mut raw = varint(name.len() as u64); // 2-byte length varint
        assert_eq!(raw.len(), 2);
        raw.extend_from_slice(name.as_bytes());
        assert_eq!(decode_name(&raw), Some(name));
    }

    #[test]
    fn fight_point_attr_sets_ability_score() {
        let attrs = vec![pb::Attr {
            id: attr_id::FIGHT_POINT,
            raw_data: varint(123_456),
        }];
        assert_eq!(
            player_info_from_attrs(pid(1), &attrs, None).ability_score,
            Some(123_456)
        );
    }

    #[test]
    fn zero_fight_point_yields_no_ability_score() {
        let attrs = vec![pb::Attr {
            id: attr_id::FIGHT_POINT,
            raw_data: varint(0),
        }];
        assert_eq!(
            player_info_from_attrs(pid(1), &attrs, None).ability_score,
            None
        );
    }

    #[test]
    fn missing_fight_point_attr_yields_no_ability_score() {
        let attrs = vec![pb::Attr {
            id: attr_id::PROFESSION_ID,
            raw_data: varint(1),
        }];
        assert_eq!(
            player_info_from_attrs(pid(1), &attrs, None).ability_score,
            None
        );
    }

    #[test]
    fn out_of_range_fight_point_is_rejected_not_truncated() {
        let attrs = vec![pb::Attr {
            id: attr_id::FIGHT_POINT,
            raw_data: varint(0x1_0000_0001),
        }];
        assert_eq!(
            player_info_from_attrs(pid(1), &attrs, None).ability_score,
            None
        );
    }

    #[test]
    fn unknown_attr_id_ignored() {
        let mut raw = Vec::new();
        prost::encoding::encode_varint(999u64, &mut raw);
        let attrs = vec![pb::Attr {
            id: 0x9999,
            raw_data: raw,
        }];
        let info = player_info_from_attrs(pid(1), &attrs, None);
        assert_eq!(info.name, None);
        assert_eq!(info.class, None);
    }

    // -- Skill level id list (issue #33, tier issues #169/#170) ------------

    /// `pairs` is `(skill_id, remodel_level)` — the same shape
    /// `decode_skill_ids` now returns, so a test can round-trip an
    /// arbitrary tier value through the wire encoding.
    fn encode_skill_list(pairs: &[(i32, i32)]) -> Vec<u8> {
        let msg = pb::SkillLevelIdList {
            skills: pairs
                .iter()
                .map(|&(skill_id, remodel_level)| pb::SkillLevelInfo {
                    skill_id,
                    current_level: 1,
                    remodel_level,
                })
                .collect(),
        };
        let mut buf = Vec::new();
        prost::Message::encode(&msg, &mut buf).unwrap();
        buf
    }

    /// `ids` with an implicit tier of 0 each — for tests that only care
    /// about id classification, not tier.
    fn encode_skill_ids(ids: &[i32]) -> Vec<u8> {
        let pairs: Vec<(i32, i32)> = ids.iter().map(|&id| (id, 0)).collect();
        encode_skill_list(&pairs)
    }

    #[test]
    fn decode_skill_ids_well_formed_payload_decodes_in_order() {
        let raw = encode_skill_ids(&[3905, 102640, 71000]);
        assert_eq!(
            decode_skill_ids(&raw),
            Some(vec![(3905, 0), (102640, 0), (71000, 0)])
        );
    }

    #[test]
    fn decode_skill_ids_empty_payload_yields_empty_list() {
        assert_eq!(decode_skill_ids(&[]), Some(Vec::new()));
    }

    #[test]
    fn decode_skill_ids_truncated_varint_yields_none() {
        // A single 0x80 byte is a continuation bit with nothing following —
        // malformed, must not panic.
        assert_eq!(decode_skill_ids(&[0x80]), None);
    }

    #[test]
    fn decode_skill_ids_garbage_bytes_yield_none() {
        let raw = [0xFFu8, 0x00, 0xAB, 0xCD, 0xEF];
        assert_eq!(decode_skill_ids(&raw), None);
    }

    #[test]
    fn decode_skill_ids_drops_zero_ids() {
        let raw = encode_skill_ids(&[3905, 0, 102640]);
        assert_eq!(decode_skill_ids(&raw), Some(vec![(3905, 0), (102640, 0)]));
    }

    /// Issues #169/#170: `remodel_level` (tag 3, BPSR-ZDPS's `Tier`) must
    /// survive `decode_skill_ids` alongside `skill_id` instead of being
    /// discarded — this is the finding the two issues were blocked on (see
    /// `pb::SkillLevelInfo`'s doc comment for the wire-tag correspondence).
    #[test]
    fn decode_skill_ids_keeps_nonzero_remodel_level_as_tier() {
        let raw = encode_skill_list(&[(3905, 3), (102640, 5)]);
        assert_eq!(decode_skill_ids(&raw), Some(vec![(3905, 3), (102640, 5)]));
    }

    /// Pins `NONZERO_TIER_LOGGED`'s once-per-process gate itself, not just
    /// `decode_skill_ids`'s returned tuples (which the tests above already
    /// cover). The gate is a process-global static shared with every other
    /// test in this binary, so this test resets it immediately before use —
    /// that makes it the sole observer of the false→true transition instead
    /// of racing whichever other test (e.g.
    /// `decode_skill_ids_keeps_nonzero_remodel_level_as_tier`, above) may
    /// already have tripped it. No production code path ever sets the flag
    /// back to `false`, so this reset cannot invalidate another test's
    /// assertions — none of them read the flag.
    #[test]
    fn decode_skill_ids_logs_first_nonzero_tier_only_once_per_process() {
        use std::sync::atomic::Ordering;

        NONZERO_TIER_LOGGED.store(false, Ordering::Relaxed);

        // First sighting of a nonzero remodel_level: the gate's
        // compare_exchange(false, true) succeeds, flipping it.
        let raw = encode_skill_list(&[(3905, 3)]);
        assert_eq!(decode_skill_ids(&raw), Some(vec![(3905, 3)]));
        assert!(
            NONZERO_TIER_LOGGED.load(Ordering::Relaxed),
            "first nonzero remodel_level must flip the gate to logged"
        );

        // Second sighting: the gate must stay tripped and take the
        // already-logged branch rather than resetting — a refactor that
        // reset the flag, or moved the check so it re-evaluates from
        // scratch per call, would leave this false again.
        let raw = encode_skill_list(&[(102640, 5)]);
        assert_eq!(decode_skill_ids(&raw), Some(vec![(102640, 5)]));
        assert!(
            NONZERO_TIER_LOGGED.load(Ordering::Relaxed),
            "gate must remain tripped after a second nonzero remodel_level"
        );
    }

    #[test]
    fn skill_level_id_list_attr_sets_skill_ids_on_player_info() {
        let attrs = vec![pb::Attr {
            id: attr_id::SKILL_LEVEL_ID_LIST,
            raw_data: encode_skill_list(&[(3905, 0), (102640, 4)]),
        }];
        assert_eq!(
            player_info_from_attrs(pid(1), &attrs, None).skill_ids,
            vec![(3905, 0), (102640, 4)]
        );
    }

    #[test]
    fn missing_skill_level_id_list_attr_yields_empty_skill_ids() {
        let attrs = vec![pb::Attr {
            id: attr_id::PROFESSION_ID,
            raw_data: varint(1),
        }];
        assert_eq!(
            player_info_from_attrs(pid(1), &attrs, None).skill_ids,
            Vec::<(i32, i32)>::new()
        );
    }

    // -- InspectSink observation (issue #25 slice A) ----------------------

    /// Test-only `InspectSink` that just records every `on_attr` call, in
    /// order, for assertions.
    /// `(uid, attr_id, raw, known)`.
    type RecordedAttr = (i64, i32, Vec<u8>, bool);

    struct RecordingSink {
        attrs: std::sync::Mutex<Vec<RecordedAttr>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                attrs: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl InspectSink for RecordingSink {
        fn on_notify(
            &self,
            _service_uuid: u64,
            _method_id: u32,
            _payload: &[u8],
            _payload_decoded: bool,
            _now_ms: u64,
        ) {
        }

        fn on_attr(&self, uid: i64, attr_id: i32, raw: &[u8], known: bool) {
            self.attrs
                .lock()
                .unwrap()
                .push((uid, attr_id, raw.to_vec(), known));
        }
    }

    #[test]
    fn unknown_attr_id_is_observed_via_sink_as_not_known() {
        let raw = varint(999);
        let attrs = vec![pb::Attr {
            id: 0x9999,
            raw_data: raw.clone(),
        }];
        let sink = RecordingSink::new();

        let info = player_info_from_attrs(pid(7), &attrs, Some(&sink));

        assert_eq!(info.name, None);
        assert_eq!(*sink.attrs.lock().unwrap(), vec![(7, 0x9999, raw, false)]);
    }

    #[test]
    fn known_attr_id_is_observed_via_sink_as_known() {
        let attrs = vec![pb::Attr {
            id: attr_id::NAME,
            raw_data: vec![0xFF, b'H', b'i'],
        }];
        let sink = RecordingSink::new();

        let info = player_info_from_attrs(pid(1), &attrs, Some(&sink));

        assert_eq!(info.name.as_deref(), Some("Hi"));
        assert_eq!(
            *sink.attrs.lock().unwrap(),
            vec![(1, attr_id::NAME, vec![0xFF, b'H', b'i'], true)]
        );
    }

    #[test]
    fn empty_raw_data_and_zero_id_do_not_trigger_the_attr_sink_call() {
        let attrs = vec![
            pb::Attr {
                id: 0x1234,
                raw_data: Vec::new(),
            },
            pb::Attr {
                id: 0,
                raw_data: varint(1),
            },
        ];
        let sink = RecordingSink::new();

        let _ = player_info_from_attrs(pid(1), &attrs, Some(&sink));

        assert!(sink.attrs.lock().unwrap().is_empty());
    }

    // -- InspectSink observation on enemy entities ------------------------
    //
    // The enemy attr walk reports to the sink exactly as the player one
    // does; an unknown id is no less a discovery for sitting on a monster.

    #[test]
    fn unknown_enemy_attr_id_is_observed_via_sink_as_not_known() {
        let raw = varint(999);
        let attrs = vec![pb::Attr {
            id: 0x9999,
            raw_data: raw.clone(),
        }];
        let sink = RecordingSink::new();

        let hp = enemy_hp_from_attrs(mid(7), &attrs, 0, Some(&sink));

        assert_eq!(hp.curr_hp, None);
        assert_eq!(*sink.attrs.lock().unwrap(), vec![(7, 0x9999, raw, false)]);
    }

    #[test]
    fn known_enemy_attr_id_is_observed_via_sink_as_known() {
        let attrs = vec![pb::Attr {
            id: attr_id::HP,
            raw_data: varint(1234),
        }];
        let sink = RecordingSink::new();

        let hp = enemy_hp_from_attrs(mid(1), &attrs, 0, Some(&sink));

        assert_eq!(hp.curr_hp, Some(1234));
        assert_eq!(
            *sink.attrs.lock().unwrap(),
            vec![(1, attr_id::HP, varint(1234), true)]
        );
    }

    #[test]
    fn empty_raw_data_and_zero_id_do_not_trigger_the_enemy_attr_sink_call() {
        let attrs = vec![
            pb::Attr {
                id: 0x1234,
                raw_data: Vec::new(),
            },
            pb::Attr {
                id: 0,
                raw_data: varint(1),
            },
        ];
        let sink = RecordingSink::new();

        let _ = enemy_hp_from_attrs(mid(1), &attrs, 0, Some(&sink));

        assert!(sink.attrs.lock().unwrap().is_empty());
    }

    // -- Entity position bundle (issue #286) --------------------------------

    /// Manually encodes a 3-float fixed32 triple the way `attr_id::POSITION`
    /// / `attr_id::TARGET_POSITION` ride the wire: tag byte
    /// `(field << 3) | 5` then the float's 4 little-endian bytes, fields 1-3
    /// in order. A `None` component is proto3's own default-omission
    /// behaviour (`0.0` is never written), not a wire absence this crate
    /// invents.
    fn encode_position(x: Option<f32>, y: Option<f32>, z: Option<f32>) -> Vec<u8> {
        let mut buf = Vec::new();
        for (field, v) in [(1u8, x), (2, y), (3, z)] {
            if let Some(v) = v {
                buf.push((field << 3) | 5);
                buf.extend_from_slice(&v.to_le_bytes());
            }
        }
        buf
    }

    #[test]
    fn decode_position_reads_all_three_fields() {
        let raw = encode_position(Some(1.5), Some(-2.25), Some(3.75));
        assert_eq!(decode_position(&raw), Some([1.5, -2.25, 3.75]));
    }

    #[test]
    fn decode_position_fills_a_proto3_omitted_zero_field() {
        // proto3 never writes a float field whose value is the default
        // 0.0 — field 2 (y) is entirely absent from the wire here.
        let raw = encode_position(Some(1.5), None, Some(3.75));
        assert_eq!(decode_position(&raw), Some([1.5, 0.0, 3.75]));
    }

    #[test]
    fn decode_position_empty_payload_is_none() {
        assert_eq!(decode_position(&[]), None);
    }

    #[test]
    fn decode_position_truncated_float_is_none() {
        // A field-1 tag with only 2 of its 4 float bytes present.
        let raw = vec![0x0d, 0x00, 0x00];
        assert_eq!(decode_position(&raw), None);
    }

    #[test]
    fn decode_position_non_fixed32_wire_type_is_none() {
        // Field 1 tagged as a varint (wire type 0), not this bundle's shape.
        let raw = vec![0x08, 0x01];
        assert_eq!(decode_position(&raw), None);
    }

    #[test]
    fn decode_position_skips_a_trailing_unknown_varint_field() {
        // A well-formed x/y/z triple followed by an unrelated field 4,
        // wire type 0 (varint) — not this bundle's fixed32 shape, but not
        // malformed either: it should be skipped, not treated as an abort
        // condition for the whole payload.
        let mut raw = encode_position(Some(1.5), Some(-2.25), Some(3.75));
        raw.push(0x20); // tag: field 4, wire type 0 (varint)
        raw.push(0x05); // value: 5
        assert_eq!(decode_position(&raw), Some([1.5, -2.25, 3.75]));
    }

    /// Issue #286's own evidence: `raw_hex=0d140d8141151b5ad5421d983d0043`,
    /// which the issue decodes to `(16.13, 106.68, 128.24)` — pinned here
    /// byte for byte as a real-capture fixture, not a synthetic one.
    #[test]
    fn decode_position_real_capture_fixture_from_issue_286() {
        let raw = [
            0x0d, 0x14, 0x0d, 0x81, 0x41, 0x15, 0x1b, 0x5a, 0xd5, 0x42, 0x1d, 0x98, 0x3d, 0x00,
            0x43,
        ];
        let pos = decode_position(&raw).expect("well-formed fixed32 triple");
        assert!((pos[0] - 16.131_386).abs() < 1e-3);
        assert!((pos[1] - 106.675_99).abs() < 1e-3);
        assert!((pos[2] - 128.240_6).abs() < 1e-3);
    }

    #[test]
    fn position_and_target_position_attrs_set_on_player_info() {
        let attrs = vec![
            pb::Attr {
                id: attr_id::POSITION,
                raw_data: encode_position(Some(1.0), Some(2.0), Some(3.0)),
            },
            pb::Attr {
                id: attr_id::TARGET_POSITION,
                raw_data: encode_position(Some(4.0), Some(5.0), Some(6.0)),
            },
        ];
        let info = player_info_from_attrs(pid(1), &attrs, None);
        assert_eq!(info.position, Some([1.0, 2.0, 3.0]));
        assert_eq!(info.target_position, Some([4.0, 5.0, 6.0]));
    }

    #[test]
    fn position_attr_sets_on_enemy_hp_and_leaves_target_position_none() {
        let attrs = vec![pb::Attr {
            id: attr_id::POSITION,
            raw_data: encode_position(Some(7.0), Some(8.0), Some(9.0)),
        }];
        let hp = enemy_hp_from_attrs(mid(1), &attrs, 0, None);
        assert_eq!(hp.position, Some([7.0, 8.0, 9.0]));
        assert_eq!(hp.target_position, None);
    }

    #[test]
    fn position_attr_is_reported_to_the_sink_as_known() {
        let attrs = vec![pb::Attr {
            id: attr_id::POSITION,
            raw_data: encode_position(Some(1.0), Some(1.0), Some(1.0)),
        }];
        let sink = RecordingSink::new();
        let _ = player_info_from_attrs(pid(1), &attrs, Some(&sink));
        assert!(sink.attrs.lock().unwrap()[0].3, "POSITION must be known");
    }

    // -- Skill-cast metadata cluster (issue #287) ----------------------------

    #[test]
    fn decode_varint_i32_truncating_reads_small_positive_values() {
        assert_eq!(decode_varint_i32_truncating(&varint(30)), Some(30));
    }

    /// Issue #287: `SKILL_STAGE`'s real-capture sample is a full 10-byte
    /// two's-complement varint for `-1` (`0xff` x9, `0x01`) — the shape a
    /// protobuf `int32` field uses for any negative value. The strict
    /// `decode_varint_i32` helper rejects this (its `u64` doesn't fit an
    /// `i32`); this helper must not.
    #[test]
    fn decode_varint_i32_truncating_reads_the_negative_one_sentinel() {
        let raw = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert_eq!(decode_varint_i32_truncating(&raw), Some(-1));
        // Pins the two helpers' actual difference rather than assuming it.
        assert_eq!(decode_varint_i32(&raw), None);
    }

    #[test]
    fn decode_varint_i32_truncating_empty_is_none() {
        assert_eq!(decode_varint_i32_truncating(&[]), None);
    }

    /// Issue #287's real-capture fixtures (`dump-2976.jsonl` /
    /// `dump-7896.jsonl`, sampled via `inspect-replay`'s attr histogram):
    /// every field in the cluster lands in its own typed slot.
    #[test]
    fn skill_cast_metadata_decodes_the_full_cluster() {
        let attrs = vec![
            pb::Attr {
                id: attr_id::SKILL_STAGE,
                raw_data: vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
            },
            pb::Attr {
                id: attr_id::SKILL_LEVEL,
                raw_data: vec![0x1e],
            },
            pb::Attr {
                id: attr_id::SKILL_BEGIN_TIME,
                raw_data: vec![0xce, 0x93, 0xd1, 0x96, 0x81, 0x34],
            },
            pb::Attr {
                id: attr_id::SKILL_STAGE_NUM,
                raw_data: vec![0x02],
            },
            pb::Attr {
                id: attr_id::SKILL_UUID,
                raw_data: vec![0xdc, 0x83, 0x80, 0x80, 0x06],
            },
        ];
        let meta = skill_cast_metadata_from_attrs(&attrs);
        assert_eq!(meta.skill_stage, Some(-1));
        assert_eq!(meta.skill_level, Some(30));
        assert_eq!(meta.skill_begin_time_ms, Some(1_787_022_297_550));
        assert_eq!(meta.skill_stage_num, Some(2));
        assert_eq!(meta.skill_uuid, Some(1_610_613_212));
    }

    #[test]
    fn skill_cast_metadata_missing_ids_stay_none() {
        assert_eq!(
            skill_cast_metadata_from_attrs(&[]),
            SkillCastMetadata::default()
        );
    }

    #[test]
    fn skill_cast_metadata_malformed_varint_stays_none_for_that_field() {
        let attrs = vec![pb::Attr {
            id: attr_id::SKILL_LEVEL,
            raw_data: vec![0x80], // continuation bit set, nothing follows
        }];
        assert_eq!(
            skill_cast_metadata_from_attrs(&attrs),
            SkillCastMetadata::default()
        );
    }

    /// Issue #339/#272: `AttrState == ActorStateDead (9)` decodes to
    /// `is_dead == true`.
    #[test]
    fn entity_state_dead_value_decodes_true() {
        let attrs = vec![pb::Attr {
            id: attr_id::STATE,
            raw_data: vec![0x09],
        }];
        assert_eq!(entity_state_from_attrs(&attrs), Some(true));
    }

    /// Any other decoded actor state (e.g. `ActorStateAction = 8`) reads as
    /// alive, not just the default state.
    #[test]
    fn entity_state_non_dead_value_decodes_false() {
        let attrs = vec![pb::Attr {
            id: attr_id::STATE,
            raw_data: vec![0x08],
        }];
        assert_eq!(entity_state_from_attrs(&attrs), Some(false));
    }

    #[test]
    fn entity_state_absent_attr_is_none() {
        let attrs = vec![pb::Attr {
            id: attr_id::HP,
            raw_data: vec![0x01],
        }];
        assert_eq!(entity_state_from_attrs(&attrs), None);
    }

    #[test]
    fn entity_state_malformed_varint_is_none() {
        let attrs = vec![pb::Attr {
            id: attr_id::STATE,
            raw_data: vec![0x80], // continuation bit set, nothing follows
        }];
        assert_eq!(entity_state_from_attrs(&attrs), None);
    }

    // -- issue #338: AttrShieldList -----------------------------------------

    /// Encodes one `ShieldInfo` submessage (`uuid=1, shieldType=2, value=3,
    /// initialValue=4, maxValue=5`, all varint) the way BPSR-ZDPS's own
    /// `MessageManager.cs:885-903` loop expects to find it: a plain
    /// varint-tagged field list, no length prefix of its own (the caller —
    /// [`encode_shield_list`] below — adds that).
    fn encode_shield_info(
        uuid: i64,
        shield_type: i32,
        value: i64,
        initial: i64,
        max: i64,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        for (field, v) in [
            (1u64, uuid),
            (2, shield_type as i64),
            (3, value),
            (4, initial),
            (5, max),
        ] {
            prost::encoding::encode_varint(field << 3, &mut buf);
            prost::encoding::encode_varint(v as u64, &mut buf);
        }
        buf
    }

    /// Concatenates length-prefixed `ShieldInfo` chunks the way
    /// [`attr_id::SHIELD_LIST`]'s raw bytes are shaped — a bare
    /// `len, payload, len, payload, ...` sequence, no enclosing tag.
    fn encode_shield_list(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut buf = Vec::new();
        for chunk in chunks {
            prost::encoding::encode_varint(chunk.len() as u64, &mut buf);
            buf.extend_from_slice(chunk);
        }
        buf
    }

    #[test]
    fn decode_shield_total_sums_a_single_shield() {
        let raw = encode_shield_list(&[encode_shield_info(1, 0, 500, 1_000, 1_000)]);
        assert_eq!(decode_shield_total(&raw), Some(500));
    }

    #[test]
    fn decode_shield_total_sums_multiple_shields() {
        let raw = encode_shield_list(&[
            encode_shield_info(1, 0, 500, 1_000, 1_000),
            encode_shield_info(2, 1, 300, 300, 300),
        ]);
        assert_eq!(decode_shield_total(&raw), Some(800));
    }

    #[test]
    fn decode_shield_total_empty_raw_is_known_zero() {
        // `isNoValue` on the wire — a real "no shields" fact, not an
        // unseen attr (see the function's own doc comment).
        assert_eq!(decode_shield_total(&[]), Some(0));
    }

    #[test]
    fn decode_shield_total_malformed_length_prefix_is_none() {
        // A length prefix claiming more bytes than actually follow.
        let mut raw = Vec::new();
        prost::encoding::encode_varint(50u64, &mut raw);
        raw.extend_from_slice(&[0x08, 0x01]);
        assert_eq!(decode_shield_total(&raw), None);
    }

    #[test]
    fn player_info_from_attrs_decodes_shield_from_attrs() {
        let raw = encode_shield_list(&[encode_shield_info(1, 0, 500, 1_000, 1_000)]);
        let attrs = vec![pb::Attr {
            id: attr_id::SHIELD_LIST,
            raw_data: raw,
        }];
        let info = player_info_from_attrs(
            EntityId::from_display_uid(7, crate::event::EntityKind::Player),
            &attrs,
            None,
        );
        assert_eq!(info.shield, Some(500));
    }

    #[test]
    fn player_info_from_attrs_shield_absent_when_no_shield_attr() {
        let info = player_info_from_attrs(
            EntityId::from_display_uid(7, crate::event::EntityKind::Player),
            &[],
            None,
        );
        assert_eq!(info.shield, None);
    }

    #[test]
    fn player_info_from_attrs_empty_shield_list_is_zero_not_latched() {
        // An empty `raw_data` on `SHIELD_LIST` is `isNoValue`'s "expired,
        // zero shields" convention (see `decode_shield_total`'s doc
        // comment), not an unseen attr — it must not be skipped by the
        // generic empty-`raw_data` guard, or an expired shield would keep
        // reporting the last nonzero total forever.
        let attrs = vec![pb::Attr {
            id: attr_id::SHIELD_LIST,
            raw_data: Vec::new(),
        }];
        let info = player_info_from_attrs(
            EntityId::from_display_uid(7, crate::event::EntityKind::Player),
            &attrs,
            None,
        );
        assert_eq!(info.shield, Some(0));
    }

    #[test]
    fn decode_shield_total_chunk_with_no_value_field_contributes_zero() {
        // proto3 omits field 3 entirely when its value is the default 0 —
        // that's absence, not malformed, and must not poison the sum.
        let chunk = encode_shield_info(1, 0, 0, 1_000, 1_000);
        // Strip the encoded `value` field (tag 3) to simulate the omission.
        let mut without_value = Vec::new();
        let mut cursor = Cursor::new(chunk.as_slice());
        while (cursor.position() as usize) < chunk.len() {
            let start = cursor.position() as usize;
            let tag = prost::encoding::decode_varint(&mut cursor).unwrap();
            let field = tag >> 3;
            let _ = prost::encoding::decode_varint(&mut cursor).unwrap();
            let end = cursor.position() as usize;
            if field != 3 {
                without_value.extend_from_slice(&chunk[start..end]);
            }
        }
        let raw = encode_shield_list(&[without_value]);
        assert_eq!(decode_shield_total(&raw), Some(0));
    }

    #[test]
    fn decode_shield_total_truncated_chunk_is_none() {
        // A chunk with a dangling varint tag and no value byte following.
        let raw = encode_shield_list(&[vec![0x08]]);
        assert_eq!(decode_shield_total(&raw), None);
    }
}
