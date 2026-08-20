//! End-to-end framing tests through the public `Decoder` façade (plan
//! T1.5): frames split across many `push_stream` calls at awkward
//! boundaries, zstd-compressed and uncompressed Notifies, FrameDown nesting,
//! and unknown opcodes interleaved with known ones.

use bpsr_protocol::{Decoder, EntityKind, ProtocolEvent, pb};
use bpsr_test_support::wire::*;

const ATTACKER_UUID: i64 = (10i64 << 16) | 640; // player uid 10
const TARGET_UUID: i64 = (20i64 << 16) | 64; // monster uid 20

fn build_multi_frame_stream() -> Vec<u8> {
    let mut stream = Vec::new();
    for i in 0..5u32 {
        let dmg = base_damage(ATTACKER_UUID, (i + 1) as i32, 100 + i as i64 * 10);
        stream.extend(damage_notify_frame(TARGET_UUID, dmg, i % 2 == 0));
    }
    stream
}

/// A frame delivered one byte at a time across (at least) 200 `push_stream`
/// calls yields the same events as a single call.
#[test]
fn byte_at_a_time_delivery_matches_single_push() {
    let stream = build_multi_frame_stream();
    assert!(
        stream.len() > 200,
        "fixture should span at least 200 bytes to exercise 200+ pushes: {}",
        stream.len()
    );

    let mut whole = Decoder::new();
    let expected = whole.push_stream(&stream, 1_000);
    assert_eq!(expected.len(), 5);

    let mut incremental = Decoder::new();
    let mut actual = Vec::new();
    for &byte in &stream {
        actual.extend(incremental.push_stream(&[byte], 1_000));
    }

    assert_eq!(actual, expected);
}

/// Splitting at arbitrary byte offsets (mid `total_len`, mid `service_uuid`,
/// mid zstd payload) instead of one-byte-at-a-time still reassembles
/// correctly.
#[test]
fn split_across_pushes_at_awkward_field_boundaries() {
    let dmg = base_damage(ATTACKER_UUID, 3, 777);
    let stream = damage_notify_frame(TARGET_UUID, dmg, true);
    assert!(stream.len() > 12);

    let mut cut_points = vec![2usize, 5, 9, stream.len() / 2, stream.len() - 3];
    cut_points.sort_unstable();
    cut_points.dedup();

    let mut decoder = Decoder::new();
    let mut events = Vec::new();
    let mut prev = 0usize;
    for cut in cut_points {
        let cut = cut.min(stream.len());
        if cut > prev {
            events.extend(decoder.push_stream(&stream[prev..cut], 9));
            prev = cut;
        }
    }
    events.extend(decoder.push_stream(&stream[prev..], 9));

    assert_eq!(events.len(), 1);
    match &events[0] {
        ProtocolEvent::Damage(d) => assert_eq!(d.value, 777),
        other => panic!("expected Damage, got {other:?}"),
    }
}

/// A zstd-compressed FrameDown carrying three uncompressed Notifies inside
/// it; the splitter must recurse into the decompressed nested stream.
#[test]
fn compressed_framedown_carries_three_notifies() {
    let mut nested = Vec::new();
    for i in 0..3u32 {
        let dmg = base_damage(ATTACKER_UUID, (i + 1) as i32, 50 + i as i64);
        nested.extend(damage_notify_frame(TARGET_UUID, dmg, false));
    }
    let stream = framedown(&nested, true);

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 42);
    assert_eq!(events.len(), 3);
    for (i, ev) in events.iter().enumerate() {
        match ev {
            ProtocolEvent::Damage(d) => assert_eq!(d.skill_id, i as i32 + 1),
            other => panic!("expected Damage, got {other:?}"),
        }
    }
}

/// Unknown opcodes (and a zstd-flagged Notify whose payload is genuinely
/// valid zstd but for an opcode nobody handles) interleaved with known
/// opcodes: only the known ones produce events.
#[test]
fn interleaved_known_and_unknown_opcodes_only_known_produce_events() {
    let mut stream = Vec::new();
    stream.extend(notify(0x0000_0099, b"unknown-1", false));
    stream.extend(damage_notify_frame(
        TARGET_UUID,
        base_damage(ATTACKER_UUID, 7, 321),
        false,
    ));
    stream.extend(notify(0x0000_00aa, b"unknown-2", true));
    // `CharSerialize.char_id` is already a uid (the entity uuid's high bits),
    // so it is emitted as-is — no `>> 16` — and lands in the same id space as
    // `DamageEvent.attacker_uid` above.
    stream.extend(notify(
        bpsr_protocol::decode::opcode::SYNC_CONTAINER_DATA,
        &sync_container_data_payload(10, "Ari", 2, 0),
        false,
    ));
    stream.extend(notify(0x0000_00bb, b"unknown-3", false));

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 5);
    assert_eq!(events.len(), 2);
    match &events[0] {
        ProtocolEvent::Damage(d) => assert_eq!(d.value, 321),
        other => panic!("expected Damage, got {other:?}"),
    }
    match &events[1] {
        ProtocolEvent::Player(p) => {
            assert_eq!(p.uid, 10);
            assert_eq!(p.name.as_deref(), Some("Ari"));
        }
        other => panic!("expected Player, got {other:?}"),
    }
}

/// `CharBaseInfo.fight_point` (ability score) is carried straight through
/// into `PlayerInfo.ability_score`.
#[test]
fn container_data_fight_point_becomes_ability_score() {
    let mut stream = Vec::new();
    stream.extend(notify(
        bpsr_protocol::decode::opcode::SYNC_CONTAINER_DATA,
        &sync_container_data_payload(11, "Zed", 2, 98_765),
        false,
    ));

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 5);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ProtocolEvent::Player(p) => assert_eq!(p.ability_score, Some(98_765)),
        other => panic!("expected Player, got {other:?}"),
    }
}

/// A zero `fight_point` (the wire default when the server doesn't populate
/// the field) is treated as absent, matching how an empty `name` is treated
/// as absent.
#[test]
fn container_data_zero_fight_point_is_no_ability_score() {
    let mut stream = Vec::new();
    stream.extend(notify(
        bpsr_protocol::decode::opcode::SYNC_CONTAINER_DATA,
        &sync_container_data_payload(12, "Yin", 2, 0),
        false,
    ));

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 5);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ProtocolEvent::Player(p) => assert_eq!(p.ability_score, None),
        other => panic!("expected Player, got {other:?}"),
    }
}

/// A player transforming into an Imagine (issue #37) then reverting: the
/// transform packet (`cur_profession_id` 8, Dorothy) must decode to
/// `class: None` — never `Some(Class::Unknown)` — and the revert packet must
/// decode back to the player's real class, both for the same uid.
#[test]
fn container_data_imagine_transform_then_revert_round_trips() {
    let mut stream = Vec::new();
    stream.extend(notify(
        bpsr_protocol::decode::opcode::SYNC_CONTAINER_DATA,
        &sync_container_data_payload(13, "Ren", 1, 0), // Stormblade
        false,
    ));
    stream.extend(notify(
        bpsr_protocol::decode::opcode::SYNC_CONTAINER_DATA,
        &sync_container_data_payload(13, "Ren", 8, 0), // Dorothy (Imagine)
        false,
    ));
    stream.extend(notify(
        bpsr_protocol::decode::opcode::SYNC_CONTAINER_DATA,
        &sync_container_data_payload(13, "Ren", 1, 0), // reverts to Stormblade
        false,
    ));

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 5);
    assert_eq!(events.len(), 3);
    match &events[0] {
        ProtocolEvent::Player(p) => assert_eq!(p.class, Some(bpsr_protocol::Class::Stormblade)),
        other => panic!("expected Player, got {other:?}"),
    }
    match &events[1] {
        ProtocolEvent::Player(p) => assert_eq!(p.class, None),
        other => panic!("expected Player, got {other:?}"),
    }
    match &events[2] {
        ProtocolEvent::Player(p) => assert_eq!(p.class, Some(bpsr_protocol::Class::Stormblade)),
        other => panic!("expected Player, got {other:?}"),
    }
}

/// `attr_id::SEASON_LEVEL` / `attr_id::SEASON_STRENGTH` on an entity's attr
/// list (the only confirmed source for season data — see `attrs.rs`'s
/// provenance comment) decode into `PlayerInfo.season_level` /
/// `PlayerInfo.season_strength`, for an arbitrary nearby entity, not just the
/// local player.
#[test]
fn season_attrs_on_entity_decode_into_player_info() {
    let player = appear_entity(
        ATTACKER_UUID,
        10,
        vec![
            varint_attr(bpsr_protocol::attrs::attr_id::SEASON_LEVEL, 42),
            varint_attr(bpsr_protocol::attrs::attr_id::SEASON_STRENGTH, 12_345),
        ],
    );
    let payload = sync_near_entities_payload(vec![player]);
    let stream = notify(
        bpsr_protocol::decode::opcode::SYNC_NEAR_ENTITIES,
        &payload,
        false,
    );

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 1);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ProtocolEvent::Player(p) => {
            assert_eq!(p.season_level, Some(42));
            assert_eq!(p.season_strength, Some(12_345));
        }
        other => panic!("expected Player, got {other:?}"),
    }
}

/// A zero raw value for either season attr (the wire default when the server
/// hasn't populated the field) is treated as absent, matching `FIGHT_POINT`'s
/// zero-is-absent treatment.
#[test]
fn zero_season_attrs_yield_no_season_data() {
    let player = appear_entity(
        ATTACKER_UUID,
        10,
        vec![
            varint_attr(bpsr_protocol::attrs::attr_id::SEASON_LEVEL, 0),
            varint_attr(bpsr_protocol::attrs::attr_id::SEASON_STRENGTH, 0),
        ],
    );
    let payload = sync_near_entities_payload(vec![player]);
    let stream = notify(
        bpsr_protocol::decode::opcode::SYNC_NEAR_ENTITIES,
        &payload,
        false,
    );

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 1);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ProtocolEvent::Player(p) => {
            assert_eq!(p.season_level, None);
            assert_eq!(p.season_strength, None);
        }
        other => panic!("expected Player, got {other:?}"),
    }
}

/// Attr `0x74` (`SKILL_LEVEL_ID_LIST`, issue #33) reaches `PlayerInfo` for an
/// arbitrary nearby entity, not just the local player — same wiring as
/// `season_attrs_on_entity_decode_into_player_info` above.
#[test]
fn skill_level_id_list_attr_on_entity_decodes_into_player_info() {
    let player = appear_entity(
        ATTACKER_UUID,
        10,
        vec![skill_list_attr(&[3905, 102640, 71000])],
    );
    let payload = sync_near_entities_payload(vec![player]);
    let stream = notify(
        bpsr_protocol::decode::opcode::SYNC_NEAR_ENTITIES,
        &payload,
        false,
    );

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 1);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ProtocolEvent::Player(p) => {
            assert_eq!(p.skill_ids, vec![3905, 102640, 71000]);
        }
        other => panic!("expected Player, got {other:?}"),
    }
}

/// A `SyncToMeDeltaInfo` (opcode `0x2e`) carries the entity's identity on the
/// *outer* `AoiSyncToMeDelta.uuid`; `base_delta.uuid` is 0. The decoder must
/// read the outer uuid, otherwise every to-me update decodes as uid 0 /
/// `EntityKind::Unknown` and is dropped.
#[test]
fn sync_to_me_delta_info_uses_outer_uuid() {
    use prost::Message;

    let msg = bpsr_protocol::pb::SyncToMeDeltaInfo {
        delta_info: Some(bpsr_protocol::pb::AoiSyncToMeDelta {
            base_delta: Some(bpsr_protocol::pb::AoiSyncDelta {
                uuid: 0,
                attrs: None,
                skill_effects: Some(bpsr_protocol::pb::SkillEffect {
                    damages: vec![base_damage(ATTACKER_UUID, 8, 512)],
                }),
            }),
            uuid: TARGET_UUID,
        }),
    };
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    let stream = notify(
        bpsr_protocol::decode::opcode::SYNC_TO_ME_DELTA_INFO,
        &payload,
        true,
    );

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 8);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ProtocolEvent::Damage(d) => {
            assert_eq!(d.value, 512);
            assert_eq!(d.attacker_uid, 10);
            assert_eq!(d.target_uid, 20);
            assert_eq!(d.target_kind, EntityKind::Monster);
        }
        other => panic!("expected Damage, got {other:?}"),
    }
}

/// `SyncNearEntities` with a player and a monster in `appear` emits a
/// `Player` event and an `EnemyHp` event, decompressed from a zstd Notify.
#[test]
fn sync_near_entities_emits_player_and_enemy_hp_events() {
    let player = appear_entity(ATTACKER_UUID, 10, vec![name_attr("Ari")]);
    let monster = appear_entity(
        TARGET_UUID,
        1,
        vec![
            varint_attr(bpsr_protocol::attrs::attr_id::HP, 8_000),
            varint_attr(bpsr_protocol::attrs::attr_id::MAX_HP, 10_000),
        ],
    );
    let payload = sync_near_entities_payload(vec![player, monster]);
    let stream = notify(
        bpsr_protocol::decode::opcode::SYNC_NEAR_ENTITIES,
        &payload,
        true,
    );

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 77);
    assert_eq!(events.len(), 2);
    match &events[0] {
        ProtocolEvent::Player(p) => {
            assert_eq!(p.uid, 10);
            assert_eq!(p.name.as_deref(), Some("Ari"));
        }
        other => panic!("expected Player, got {other:?}"),
    }
    match &events[1] {
        ProtocolEvent::EnemyHp(hp) => {
            assert_eq!(hp.uid, 20);
            assert_eq!(hp.curr_hp, Some(8_000));
            assert_eq!(hp.max_hp, Some(10_000));
        }
        other => panic!("expected EnemyHp, got {other:?}"),
    }
    assert_eq!(
        bpsr_protocol::event::kind_of(ATTACKER_UUID),
        EntityKind::Player
    );
}

/// Five good Notify frames followed by a garbage length prefix in one TCP
/// packet: a desync at the tail must not discard the frames parsed before
/// it — only the trailing garbage is dropped.
#[test]
fn desync_after_good_frames_still_yields_their_events() {
    let mut stream = build_multi_frame_stream(); // 5 good Notify frames
    stream.extend_from_slice(&5u32.to_be_bytes()); // garbage: below MIN_FRAME_LEN

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 3);
    assert_eq!(events.len(), 5);
    for (i, ev) in events.iter().enumerate() {
        match ev {
            ProtocolEvent::Damage(d) => assert_eq!(d.value, 100 + i as i64 * 10),
            other => panic!("expected Damage, got {other:?}"),
        }
    }
}

/// A length prefix above `MAX_FRAME_LEN` must cause that frame's whole body
/// to be skipped, not merely the buffered tail to be dropped: otherwise the
/// stream resumes mid-body and every following length prefix is garbage.
#[test]
fn oversized_frame_body_is_skipped_so_the_next_frame_realigns() {
    const OVERSIZED: u32 = bpsr_protocol::frame::MAX_FRAME_LEN + 1;
    const HEAD: usize = 64; // length prefix + the first slice of the huge body

    let mut first = OVERSIZED.to_be_bytes().to_vec();
    first.extend_from_slice(&2u16.to_be_bytes()); // Notify: a well-formed header
    first.resize(HEAD, 0xAB);
    let mut decoder = Decoder::new();
    assert!(decoder.push_stream(&first, 0).is_empty());

    // The remainder of the oversized body, immediately followed by a good
    // frame in the same push: the good frame must still decode.
    let mut second = vec![0xABu8; OVERSIZED as usize - HEAD];
    second.extend(damage_notify_frame(
        TARGET_UUID,
        base_damage(ATTACKER_UUID, 4, 99),
        false,
    ));

    let events = decoder.push_stream(&second, 1);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ProtocolEvent::Damage(d) => assert_eq!(d.value, 99),
        other => panic!("expected Damage, got {other:?}"),
    }
}

/// Two independent frames delivered in a single push, plus a partial third
/// frame held over to the next push, both surface correctly.
#[test]
fn two_full_frames_plus_partial_tail_across_two_pushes() {
    let mut first_push = damage_notify_frame(TARGET_UUID, base_damage(ATTACKER_UUID, 1, 11), false);
    first_push.extend(damage_notify_frame(
        TARGET_UUID,
        base_damage(ATTACKER_UUID, 2, 22),
        false,
    ));
    let third = damage_notify_frame(TARGET_UUID, base_damage(ATTACKER_UUID, 3, 33), true);
    let split = third.len() / 2;
    first_push.extend_from_slice(&third[..split]);

    let mut decoder = Decoder::new();
    let first_events = decoder.push_stream(&first_push, 1);
    assert_eq!(first_events.len(), 2);

    let second_events = decoder.push_stream(&third[split..], 2);
    assert_eq!(second_events.len(), 1);
    match &second_events[0] {
        ProtocolEvent::Damage(d) => assert_eq!(d.value, 33),
        other => panic!("expected Damage, got {other:?}"),
    }
}

/// `GrpcTeamNtf.NotifyJoinTeam` end-to-end through `Decoder::push_stream`
/// (issue #146): a roster of a fully-populated member, a name-only member,
/// and a bot-like member with no `social_data` at all. Also exercises the
/// method-id collision this slice exists to prevent — `0x3` is
/// `NotifyJoinTeam` on `TEAM_NTF_SERVICE_UUID` and `ENTER_SCENE` on the main
/// `SERVICE_UUID` — by delivering it on the team service uuid and asserting
/// it decodes as a roster, not a scene change.
#[test]
fn team_ntf_notify_join_team_decodes_roster_end_to_end() {
    let payload = notify_join_team_payload(vec![
        team_member(101, "Ari", 1, 12_345), // Stormblade
        pb::TeamMemData {
            char_id: 102,
            scene_id: 0,
            group_id: 0,
            social_data: Some(pb::TeamMemberSocialData {
                basic_data: Some(pb::TeamBasicData {
                    char_id: 102,
                    name: "NameOnly".to_string(),
                    level: 0,
                }),
                profession_data: None,
                user_attr_data: None,
            }),
        },
        bot_team_member(103),
    ]);
    let stream = notify_with_service(
        bpsr_protocol::frame::TEAM_NTF_SERVICE_UUID,
        bpsr_protocol::decode::team_opcode::NOTIFY_JOIN_TEAM,
        &payload,
        false,
    );

    let mut decoder = Decoder::new();
    let events = decoder.push_stream(&stream, 5);
    assert_eq!(events.len(), 2, "the bot-like member must yield no event");
    match &events[0] {
        ProtocolEvent::Player(p) => {
            assert_eq!(p.uid, 101);
            assert_eq!(p.name.as_deref(), Some("Ari"));
            assert_eq!(p.class, Some(bpsr_protocol::Class::Stormblade));
            assert_eq!(p.ability_score, Some(12_345));
        }
        other => panic!("expected Player, got {other:?}"),
    }
    match &events[1] {
        ProtocolEvent::Player(p) => {
            assert_eq!(p.uid, 102);
            assert_eq!(p.name.as_deref(), Some("NameOnly"));
            assert_eq!(p.class, None);
            assert_eq!(p.ability_score, None);
        }
        other => panic!("expected Player, got {other:?}"),
    }
}
