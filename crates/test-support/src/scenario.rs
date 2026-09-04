//! The scenario DSL (`docs/plans/system-test-harness.md` §2): a fluent
//! builder that scripts an encounter as a sequence of `Step`s. App-free — it
//! produces wire bytes (via `crate::wire`) and `bpsr_protocol::ProtocolEvent`s
//! only. `crates/app/tests/common/mod.rs`'s `Rig` consumes a `Scenario` and
//! drives it through the real decode/pipeline stack.

use bpsr_protocol::ProtocolEvent;
use bpsr_protocol::attrs::attr_id;
use bpsr_protocol::decode::opcode;
use bpsr_protocol::pb;

use crate::wire;

/// How one step's bytes reach the TcpReassembler.
#[derive(Clone, Debug)]
pub enum Delivery {
    /// One segment carrying the whole step.
    Whole,
    /// Split at the given byte offsets (relative to this step's bytes), each
    /// piece delivered as its own TCP segment, in stream order.
    SplitAt(Vec<usize>),
    /// Split at the given offsets, then delivered in the given permutation of
    /// piece indices (sequence numbers still follow stream order).
    SplitAndReorder { at: Vec<usize>, order: Vec<usize> },
}

#[derive(Clone, Debug)]
pub enum Step {
    /// Wire bytes decoded with `now_ms = at_ms`.
    Bytes {
        at_ms: u64,
        bytes: Vec<u8>,
        delivery: Delivery,
    },
    /// An event the decoder can never produce (today: only `ServerChanged`,
    /// synthesized by `crates/capture/src/win.rs`).
    Inject { at_ms: u64, event: ProtocolEvent },
    /// `Pipeline::tick(at_ms)` — the idle-timeout clock.
    Tick { at_ms: u64 },
    /// `Pipeline::snapshot(at_ms)`, captured and compared to golden `label`.
    Capture { at_ms: u64, label: &'static str },
}

/// One damage entry, expressed in meter terms; `wire::damage_info` turns it
/// into a `pb::SyncDamageInfo`.
#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub attacker_uid: i64,
    /// Non-zero credits the hit to that uid (pet/summon path). 0 = not a pet.
    pub summoner_uid: i64,
    /// Must be non-zero, or the decoder drops the entry (`owner_id == 0`).
    pub skill_id: i32,
    pub value: i64,
    pub crit: bool,
    /// Sets `lucky_value = value` and zeroes `value`.
    pub lucky: bool,
    pub miss: bool,
    pub heal: bool,
    /// `is_dead`.
    pub kills_target: bool,
}

impl Hit {
    pub fn new(attacker_uid: i64, skill_id: i32, value: i64) -> Self {
        Self {
            attacker_uid,
            summoner_uid: 0,
            skill_id,
            value,
            crit: false,
            lucky: false,
            miss: false,
            heal: false,
            kills_target: false,
        }
    }

    pub fn crit(mut self) -> Self {
        self.crit = true;
        self
    }

    pub fn lucky(mut self) -> Self {
        self.lucky = true;
        self
    }

    pub fn kill(mut self) -> Self {
        self.kills_target = true;
        self
    }

    /// Marks this hit as landed by a pet/summon whose owner is `owner_uid`
    /// (`top_summoner_id = player_uuid(owner_uid)`).
    pub fn by_pet_of(mut self, owner_uid: i64) -> Self {
        self.summoner_uid = owner_uid;
        self
    }
}

pub struct Scenario {
    pub name: &'static str,
    pub steps: Vec<Step>,
    at_ms: u64,
    compressed: bool,
    delivery: Delivery,
    nested: bool,
}

impl Scenario {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            steps: Vec::new(),
            at_ms: 0,
            compressed: false,
            delivery: Delivery::Whole,
            nested: false,
        }
    }

    /// Move the clock cursor. Every later verb uses this until changed.
    pub fn at(mut self, ms: u64) -> Self {
        self.at_ms = ms;
        self
    }

    /// zstd-compress the frames emitted by later verbs (default: false).
    /// Flip it mid-scenario to cover both paths in one run.
    pub fn compressed(mut self, on: bool) -> Self {
        self.compressed = on;
        self
    }

    /// Apply this delivery to the NEXT verb only, then revert to Whole.
    pub fn next_delivery(mut self, d: Delivery) -> Self {
        self.delivery = d;
        self
    }

    /// Nest the NEXT verb's frame inside a FrameDown wrapper.
    pub fn nested(mut self) -> Self {
        self.nested = true;
        self
    }

    // --- wire verbs (each appends exactly one Step::Bytes) ---

    pub fn enter_scene(mut self, scene_id: u32) -> Self {
        let payload = wire::enter_scene_payload(scene_id);
        let bytes = self.wrap_frame(opcode::ENTER_SCENE, &payload);
        self.push_bytes(bytes);
        self
    }

    pub fn player_appear(
        mut self,
        uid: i64,
        name: &str,
        profession_id: i32,
        fight_point: i32,
    ) -> Self {
        let uuid = wire::player_uuid(uid);
        let entity = wire::appear_entity(
            uuid,
            pb::EEntityType::EntChar as i32,
            vec![
                wire::name_attr(name),
                wire::varint_attr(attr_id::PROFESSION_ID, profession_id as u64),
                wire::varint_attr(attr_id::FIGHT_POINT, fight_point as u64),
            ],
        );
        let payload = wire::sync_near_entities_payload(vec![entity]);
        let bytes = self.wrap_frame(opcode::SYNC_NEAR_ENTITIES, &payload);
        self.push_bytes(bytes);
        self
    }

    pub fn container_data(
        mut self,
        uid: i64,
        name: &str,
        profession_id: i32,
        fight_point: i32,
    ) -> Self {
        let payload = wire::sync_container_data_payload(uid, name, profession_id, fight_point);
        let bytes = self.wrap_frame(opcode::SYNC_CONTAINER_DATA, &payload);
        self.push_bytes(bytes);
        self
    }

    pub fn monster_appear(mut self, uid: i64, monster_id: u32, hp: u64, max_hp: u64) -> Self {
        let uuid = wire::monster_uuid(uid);
        let entity = wire::appear_entity(
            uuid,
            pb::EEntityType::EntMonster as i32,
            vec![
                wire::varint_attr(attr_id::MONSTER_ID, monster_id as u64),
                wire::varint_attr(attr_id::HP, hp),
                wire::varint_attr(attr_id::MAX_HP, max_hp),
            ],
        );
        let payload = wire::sync_near_entities_payload(vec![entity]);
        let bytes = self.wrap_frame(opcode::SYNC_NEAR_ENTITIES, &payload);
        self.push_bytes(bytes);
        self
    }

    /// One SyncNearDeltaInfo carrying HP attrs for a monster (the boss-HP curve).
    pub fn monster_hp(mut self, uid: i64, hp: u64) -> Self {
        let uuid = wire::monster_uuid(uid);
        let payload = wire::attr_delta_payload(uuid, vec![wire::varint_attr(attr_id::HP, hp)]);
        let bytes = self.wrap_frame(opcode::SYNC_NEAR_DELTA_INFO, &payload);
        self.push_bytes(bytes);
        self
    }

    /// One SyncNearDeltaInfo carrying N damage entries against `target_uid`
    /// (a monster uid, packed via `wire::monster_uuid`).
    pub fn hits(mut self, target_uid: i64, dmgs: Vec<Hit>) -> Self {
        let target_uuid = wire::monster_uuid(target_uid);
        let infos = dmgs.iter().map(wire::damage_info).collect();
        let payload = wire::damage_delta_multi(target_uuid, infos);
        let bytes = self.wrap_frame(opcode::SYNC_NEAR_DELTA_INFO, &payload);
        self.push_bytes(bytes);
        self
    }

    /// Sugar for a single normal hit.
    pub fn hit(self, attacker_uid: i64, target_uid: i64, skill_id: i32, value: i64) -> Self {
        self.hits(target_uid, vec![Hit::new(attacker_uid, skill_id, value)])
    }

    /// A monster's hit on a player — the reverse attacker/target direction
    /// from `hit`/`hits` (which always model a player attacking a monster).
    /// `kills_target` sets `is_dead` on the wire `SyncDamageInfo`, which is
    /// how `Meter::record_death` learns a player went down — the way to
    /// script a party wipe.
    pub fn monster_hits_player(
        mut self,
        attacker_monster_uid: i64,
        target_player_uid: i64,
        skill_id: i32,
        value: i64,
        kills_target: bool,
    ) -> Self {
        let target_uuid = wire::player_uuid(target_player_uid);
        let info = wire::monster_damage_info(attacker_monster_uid, skill_id, value, kills_target);
        let payload = wire::damage_delta(target_uuid, info);
        let bytes = self.wrap_frame(opcode::SYNC_NEAR_DELTA_INFO, &payload);
        self.push_bytes(bytes);
        self
    }

    // --- non-wire verbs ---

    pub fn inject(mut self, event: ProtocolEvent) -> Self {
        self.steps.push(Step::Inject {
            at_ms: self.at_ms,
            event,
        });
        self
    }

    pub fn tick(mut self) -> Self {
        self.steps.push(Step::Tick { at_ms: self.at_ms });
        self
    }

    pub fn capture(mut self, label: &'static str) -> Self {
        self.steps.push(Step::Capture {
            at_ms: self.at_ms,
            label,
        });
        self
    }

    /// Wraps `payload` in a Notify frame at the current compression
    /// setting, then in a FrameDown frame too if `nested()` was armed for
    /// this verb. Consumes the one-shot `nested` flag.
    fn wrap_frame(&mut self, opcode: u32, payload: &[u8]) -> Vec<u8> {
        let frame = wire::notify(opcode, payload, self.compressed);
        if std::mem::take(&mut self.nested) {
            wire::framedown(&frame, self.compressed)
        } else {
            frame
        }
    }

    /// Appends a `Step::Bytes` at the current clock cursor, consuming the
    /// one-shot `delivery` override.
    fn push_bytes(&mut self, bytes: Vec<u8>) {
        let delivery = std::mem::replace(&mut self.delivery, Delivery::Whole);
        self.steps.push(Step::Bytes {
            at_ms: self.at_ms,
            bytes,
            delivery,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_records_steps_in_order() {
        let scenario = Scenario::new("smoke")
            .at(1_000)
            .enter_scene(1101)
            .player_appear(1001, "Aria", wire::prof::STORMBLADE, 12_000)
            .monster_appear(2001, 103, 5_000_000, 5_000_000)
            .at(2_000)
            .hit(1001, 2001, 101, 40_000)
            .at(30_000)
            .capture("smoke");

        assert_eq!(scenario.steps.len(), 5);
        assert!(matches!(
            scenario.steps[0],
            Step::Bytes { at_ms: 1_000, .. }
        ));
        assert!(matches!(
            scenario.steps[3],
            Step::Bytes { at_ms: 2_000, .. }
        ));
        assert!(matches!(
            scenario.steps[4],
            Step::Capture {
                at_ms: 30_000,
                label: "smoke"
            }
        ));
    }

    #[test]
    fn nested_wraps_only_the_next_verb() {
        let plain = Scenario::new("x").hit(1, 2, 101, 10);
        let Step::Bytes {
            bytes: plain_bytes, ..
        } = &plain.steps[0]
        else {
            panic!("expected Bytes step");
        };

        let wrapped = Scenario::new("x")
            .nested()
            .hit(1, 2, 101, 10)
            .hit(1, 2, 101, 10);
        let Step::Bytes {
            bytes: nested_bytes,
            ..
        } = &wrapped.steps[0]
        else {
            panic!("expected Bytes step");
        };
        let Step::Bytes {
            bytes: unnested_bytes,
            ..
        } = &wrapped.steps[1]
        else {
            panic!("expected Bytes step");
        };

        assert_ne!(nested_bytes.len(), plain_bytes.len());
        assert_eq!(unnested_bytes, plain_bytes);
    }
}
