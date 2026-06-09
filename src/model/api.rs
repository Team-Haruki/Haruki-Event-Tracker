use sea_orm::FromQueryResult;
use serde::{Deserialize, Serialize};

use crate::model::sekai::{UserPlayerFrame, UserProfileHonor};

#[derive(Debug, Clone, Serialize, Deserialize, FromQueryResult)]
#[serde(rename_all = "camelCase")]
pub struct RecordedRankingSchema {
    pub timestamp: i64,
    pub user_id: String,
    pub score: i64,
    pub rank: i64,
}

/// Same wire shape as the Go version: `RecordedRankingSchema` is embedded so
/// JSON output is flat — fields are duplicated here rather than nested via
/// `serde(flatten)` so this type can also be `FromQueryResult`-derived from
/// the World Bloom join (which selects `character_id` as an extra column).
#[derive(Debug, Clone, Serialize, Deserialize, FromQueryResult)]
#[serde(rename_all = "camelCase")]
pub struct RecordedWorldBloomRankingSchema {
    pub timestamp: i64,
    pub user_id: String,
    pub score: i64,
    pub rank: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub character_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RecordedRankData {
    Normal(RecordedRankingSchema),
    WorldBloom(RecordedWorldBloomRankingSchema),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedUserNameSchema {
    pub user_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cheerful_team_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_level: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_master_rank: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_special_training_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_default_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_word: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub profile_honors: Vec<UserProfileHonor>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub user_player_frames: Vec<UserPlayerFrame>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UserLatestRankingQueryResponseSchema {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank_data: Option<RecordedRankData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<RecordedUserNameSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UserAllRankingDataQueryResponseSchema {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rank_data: Vec<RecordedRankData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<RecordedUserNameSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchAllRankingDataItemSchema {
    pub rank: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rank_data: Vec<RecordedRankData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BatchAllRankingDataQueryResponseSchema {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<BatchAllRankingDataItemSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromQueryResult)]
#[serde(rename_all = "camelCase")]
pub struct RankingLineScoreSchema {
    pub rank: i64,
    pub score: i64,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RankingScoreGrowthSchema {
    pub rank: i64,
    pub timestamp_latest: i64,
    pub score_latest: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_earlier: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_earlier: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_diff: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub growth: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventStatusResponseSchema {
    pub timestamp: i64,
    pub status: i8,
    pub status_desc: String,
    pub time_ago: i64,
}
