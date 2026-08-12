//! End-to-end framing tests through the public `Decoder` façade (plan
//! T1.5): frames split across many `push_stream` calls at awkward
//! boundaries, zstd-compressed and uncompressed Notifies, FrameDown nesting,
//! and unknown opcodes interleaved with known ones.

mod common;

use bpsr_protocol::{Decoder, EntityKind, ProtocolEvent};
use common::*;

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
    stream.extend(notify(
        bpsr_protocol::decode::opcode::SYNC_CONTAINER_DATA,
        &sync_container_data_payload(ATTACKER_UUID, "Ari", 2),
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
            assert_eq!(p.uid, ATTACKER_UUID);
            assert_eq!(p.name.as_deref(), Some("Ari"));
        }
        other => panic!("expected Player, got {other:?}"),
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
    assert_eq!(bpsr_protocol::event::kind_of(ATTACKER_UUID), EntityKind::Player);
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
