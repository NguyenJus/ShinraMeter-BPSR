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

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Entity {
    #[prost(int64, tag = "1")]
    pub uuid: i64,
    #[prost(enumeration = "EEntityType", tag = "2")]
    pub ent_type: i32,
    #[prost(message, optional, tag = "3")]
    pub attrs: Option<AttrCollection>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DisappearEntity {
    #[prost(int64, tag = "1")]
    pub uuid: i64,
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

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CharSerialize {
    #[prost(int64, tag = "1")]
    pub char_id: i64,
    #[prost(message, optional, tag = "2")]
    pub char_base: Option<CharBaseInfo>,
    #[prost(message, optional, tag = "3")]
    pub scene_data: Option<SceneData>,
    #[prost(message, optional, tag = "61")]
    pub profession_list: Option<ProfessionList>,
}

/// Dungeon/instance identity, confirmed against `winjwinj/bpsr-logs`'
/// `src-tauri/src/protocol/pb.proto` (`message SceneData { uint32
/// level_map_id = 6; uint32 line_id = 15; }`). `line_id` (the instance's
/// party-line number, not its identity) is not decoded — nothing downstream
/// needs it.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SceneData {
    #[prost(uint32, tag = "6")]
    pub level_map_id: u32,
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
}
