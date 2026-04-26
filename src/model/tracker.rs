use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::sekai::PlayerRankingSchema;

#[derive(Debug, Clone)]
pub struct PlayerEventRankingRecordSchema {
    pub timestamp: i64,
    pub user_id: String,
    pub name: String,
    pub score: i64,
    pub rank: i64,
    pub cheerful_team_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct PlayerWorldBloomRankingRecordSchema {
    pub base: PlayerEventRankingRecordSchema,
    pub character_id: i64,
}

/// Per-user latest snapshot. Wire format must match the Go version's short
/// JSON keys (`s`, `r`) so old Redis state remains readable across the cutover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerState {
    #[serde(rename = "s")]
    pub score: i64,
    #[serde(rename = "r")]
    pub rank: i64,
}

/// Per-rank latest snapshot. Same wire-compat rule as `PlayerState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankState {
    #[serde(rename = "u")]
    pub user_id: String,
    #[serde(rename = "s")]
    pub score: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorldBloomKey {
    pub user_id_key: i64,
    pub character_id: i64,
}

#[derive(Debug, Clone)]
pub struct HandledRankingData {
    pub record_time: i64,
    pub rankings: Vec<PlayerRankingSchema>,
    pub world_bloom_rankings: HashMap<i64, Vec<PlayerRankingSchema>>,
}
