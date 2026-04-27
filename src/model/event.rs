use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::enums::{SekaiEventStatus, SekaiEventType, SekaiServerRegion, SekaiUnit, SekaiWorldBloomType};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldBloomChapterStatus {
    pub server: SekaiServerRegion,
    pub event_id: i64,
    pub character_id: i64,
    pub chapter_status: SekaiEventStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventStatus {
    pub server: SekaiServerRegion,
    pub event_id: i64,
    pub event_type: SekaiEventType,
    pub event_status: SekaiEventStatus,
    pub remain: String,
    pub assetbundle_name: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub chapter_statuses: HashMap<i64, WorldBloomChapterStatus>,
    pub detail: Event,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldBloom {
    pub id: i64,
    pub event_id: i64,
    #[serde(default)]
    pub game_character_id: i64,
    pub world_bloom_chapter_type: SekaiWorldBloomType,
    pub chapter_no: i64,
    pub chapter_start_at: i64,
    pub aggregate_at: i64,
    pub chapter_end_at: i64,
    pub is_supplemental: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: i64,
    pub event_type: SekaiEventType,
    pub name: String,
    pub assetbundle_name: String,
    pub bgm_assetbundle_name: String,
    pub event_only_component_display_start_at: i64,
    pub start_at: i64,
    pub aggregate_at: i64,
    pub ranking_announce_at: i64,
    pub distribution_start_at: i64,
    pub event_only_component_display_end_at: i64,
    pub closed_at: i64,
    pub distribution_end_at: i64,
    #[serde(default)]
    pub virtual_live_id: i64,
    pub unit: SekaiUnit,
    #[serde(default)]
    pub is_count_leader_character_play: bool,
    #[serde(default)]
    pub event_point_assetbundle_name: String,
    #[serde(default)]
    pub standby_screen_display_start_at: i64,
}
