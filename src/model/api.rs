use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedRankingSchema {
    pub timestamp: i64,
    pub user_id: String,
    pub score: i64,
    pub rank: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedWorldBloomRankingSchema {
    #[serde(flatten)]
    pub base: RecordedRankingSchema,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub character_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum RecordedRankData {
    Normal(RecordedRankingSchema),
    WorldBloom(RecordedWorldBloomRankingSchema),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedUserNameSchema {
    pub user_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cheerful_team_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UserLatestRankingQueryResponseSchema {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank_data: Option<RecordedRankData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<RecordedUserNameSchema>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UserAllRankingDataQueryResponseSchema {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rank_data: Vec<RecordedRankData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_data: Option<RecordedUserNameSchema>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RankingLineScoreSchema {
    pub rank: i64,
    pub score: i64,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventStatusResponseSchema {
    pub timestamp: i64,
    pub status: i8,
    pub status_desc: String,
    pub time_ago: i64,
}
