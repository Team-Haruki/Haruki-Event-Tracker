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
    pub user_honor_missions: Vec<sonic_rs::Value>,
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
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub rank_data: Vec<RecordedRankData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<RecordedUserNameSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchAllRankingDataItemSchema {
    pub rank: i64,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub rank_data: Vec<RecordedRankData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BatchAllRankingDataQueryResponseSchema {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
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
pub struct TopRankingPlayerGrowthSchema {
    pub rank: i64,
    pub user_id: String,
    pub score_latest: i64,
    pub timestamp_latest: i64,
    pub score_earlier: i64,
    pub timestamp_earlier: i64,
    pub time_diff: i64,
    pub growth: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub character_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventStatusResponseSchema {
    pub timestamp: i64,
    pub status: i16,
    pub status_desc: String,
    pub time_ago: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebRankingPageSchema {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub items: Vec<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebOverviewSchema {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub top_rankings: Vec<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub top_player_growths: Vec<TopRankingPlayerGrowthSchema>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub top_rank_growths: Vec<RankingScoreGrowthSchema>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub border_lines: Vec<RankingLineScoreSchema>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub border_growths: Vec<RankingScoreGrowthSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<EventStatusResponseSchema>,
    pub interval_seconds: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebRankingItemSchema {
    pub rank_data: RecordedRankData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<RecordedUserNameSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebUserSearchPageSchema {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub items: Vec<RecordedUserNameSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaderboardMetaSchema {
    pub server: String,
    pub event_id: i64,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub character_id: Option<i64>,
    pub fetched_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaderboardOverviewSchema {
    pub meta: LeaderboardMetaSchema,
    #[serde(flatten)]
    pub overview: WebOverviewSchema,
    pub window_start: i64,
    pub window_end: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RankSnapshotSchema {
    pub rank: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<RankingScoreGrowthSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RankSnapshotsResponseSchema {
    pub meta: LeaderboardMetaSchema,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub items: Vec<RankSnapshotSchema>,
    pub interval_seconds: i64,
    pub window_start: i64,
    pub window_end: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubjectTraceMetaSchema {
    pub subject_type: String,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_rank: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubjectTraceResponseSchema {
    pub meta: LeaderboardMetaSchema,
    pub subject: SubjectTraceMetaSchema,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub rank_data: Vec<RecordedRankData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<RecordedUserNameSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudRankInfoSchema {
    pub rank: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub name: String,
    pub score: i64,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_window: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub character_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudRankQueryResponseSchema {
    pub meta: LeaderboardMetaSchema,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub ranks: Vec<CloudRankInfoSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<CloudRankInfoSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<CloudRankInfoSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudCheckRoomResponseSchema {
    pub meta: LeaderboardMetaSchema,
    pub rank: CloudRankInfoSchema,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<CloudRankInfoSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<CloudRankInfoSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudLineResponseSchema {
    pub meta: LeaderboardMetaSchema,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub ranks: Vec<CloudRankInfoSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudSpeedResponseSchema {
    pub meta: LeaderboardMetaSchema,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub speeds: Vec<CloudRankInfoSchema>,
    pub interval_seconds: i64,
    pub unit_seconds: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudTraceResponseSchema {
    pub meta: LeaderboardMetaSchema,
    pub subject: SubjectTraceMetaSchema,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub rank_data: Vec<CloudRankInfoSchema>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebRankDetailResponseSchema {
    pub meta: LeaderboardMetaSchema,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<RankingScoreGrowthSchema>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub rank_trace: Vec<RecordedRankData>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub player_trace: Vec<RecordedRankData>,
    pub interval_seconds: i64,
    pub window_start: i64,
    pub window_end: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebUserDetailResponseSchema {
    pub meta: LeaderboardMetaSchema,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub player_trace: Vec<RecordedRankData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<RecordedUserNameSchema>,
}
