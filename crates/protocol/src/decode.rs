//! Opcode dispatch: `Notify.method_id` → `ProtocolEvent` (plan §T1.4).
//!
//! `Decoder` is the crate's only public façade — `push_stream` feeds raw
//! reassembled TCP bytes in and gets fully-typed events out. A decode error
//! anywhere in this module is dropped with a debug log; nothing here panics
//! or propagates an error to the caller.

use prost::Message;

use std::sync::Arc;

use crate::attrs::{enemy_hp_from_attrs, player_info_from_attrs};
use crate::event::{DamageEvent, EntityKind, PlayerInfo, ProtocolEvent, kind_of, uid_of};
use crate::frame::{Desync, MAX_TAIL_LEN, Notify, parse_frame, split_frames};
use crate::inspect::InspectSink;
use crate::pb::{self, AoiSyncDelta, Class, EDamageType};

pub mod opcode {
    pub const SYNC_NEAR_ENTITIES: u32 = 0x0000_0006;
    pub const SYNC_CONTAINER_DATA: u32 = 0x0000_0015;
    pub const SYNC_NEAR_DELTA_INFO: u32 = 0x0000_002d;
    pub const SYNC_TO_ME_DELTA_INFO: u32 = 0x0000_002e;
}

/// Decodes one Notify's payload and appends any resulting events to `out`.
/// Unknown opcodes are skipped silently; a prost decode failure is dropped
/// with a debug log. `sink` is the issue #25 slice A diagnostic hook: `None`
/// on every normal (non-diagnostic) call site.
pub fn decode_notify(
    n: &Notify,
    now_ms: u64,
    out: &mut Vec<ProtocolEvent>,
    sink: Option<&dyn InspectSink>,
) {
    match n.method_id {
        opcode::SYNC_NEAR_ENTITIES => match pb::SyncNearEntities::decode(n.payload.as_slice()) {
            Ok(msg) => on_sync_near_entities(&msg, now_ms, out, sink),
            Err(_) => log::debug!("bpsr-protocol: SyncNearEntities decode failed"),
        },
        opcode::SYNC_CONTAINER_DATA => match pb::SyncContainerData::decode(n.payload.as_slice()) {
            Ok(msg) => on_sync_container_data(&msg, out),
            Err(_) => log::debug!("bpsr-protocol: SyncContainerData decode failed"),
        },
        opcode::SYNC_NEAR_DELTA_INFO => match pb::SyncNearDeltaInfo::decode(n.payload.as_slice()) {
            Ok(msg) => {
                for delta in &msg.delta_infos {
                    on_aoi_sync_delta(delta, delta.uuid, now_ms, out, sink);
                }
            }
            Err(_) => log::debug!("bpsr-protocol: SyncNearDeltaInfo decode failed"),
        },
        opcode::SYNC_TO_ME_DELTA_INFO => {
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
    }
    let Some(effects) = &delta.skill_effects else {
        return;
    };
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
        out.push(ProtocolEvent::Damage(DamageEvent {
            attacker_uid: uid_of(attacker_uuid),
            attacker_kind: kind_of(attacker_uuid),
            skill_id: dmg.owner_id,
            value,
            crit: dmg.type_flag & 1 != 0,
            lucky: dmg.lucky_value != 0,
            hp_lessen: dmg.hp_lessen_value,
            is_miss: dmg.is_miss || dmg.r#type == EDamageType::Miss as i32,
            is_heal: dmg.r#type == EDamageType::Heal as i32,
            target_uid,
            target_kind,
            timestamp_ms: now_ms,
        }));
    }
}

fn on_sync_container_data(msg: &pb::SyncContainerData, out: &mut Vec<ProtocolEvent>) {
    let Some(v_data) = &msg.v_data else {
        return;
    };
    let Some(char_base) = &v_data.char_base else {
        return;
    };
    let name = if char_base.name.is_empty() {
        None
    } else {
        Some(char_base.name.clone())
    };
    let class = v_data
        .profession_list
        .as_ref()
        .map(|p| Class::from(p.cur_profession_id));
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
    }));
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
        };
        let msg = SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        Notify {
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
        };
        let msg = SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let n = Notify {
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
                }),
                uuid: outer_uuid,
            }),
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        Notify {
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
        };
        let msg = SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let n = Notify {
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
        };
        let msg = SyncNearDeltaInfo {
            delta_infos: vec![delta],
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let n = Notify {
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
}
