use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserCheerfulCarnival {
    pub cheerful_carnival_team_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerRankingSchema {
    pub name: Option<String>,
    pub rank: Option<i64>,
    pub score: Option<i64>,
    pub user_id: Option<i64>,
    pub user_cheerful_carnival: Option<UserCheerfulCarnival>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserWorldBloomChapterRankingBase {
    pub event_id: Option<i64>,
    pub game_character_id: Option<i64>,
    pub is_world_bloom_chapter_aggregate: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserWorldBloomChapterRanking {
    #[serde(flatten)]
    pub base: UserWorldBloomChapterRankingBase,
    #[serde(default)]
    pub rankings: Vec<PlayerRankingSchema>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserWorldBloomChapterRankingBorder {
    #[serde(flatten)]
    pub base: UserWorldBloomChapterRankingBase,
    #[serde(default)]
    pub border_rankings: Vec<PlayerRankingSchema>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Top100RankingResponse {
    pub is_event_aggregate: Option<bool>,
    #[serde(default)]
    pub rankings: Vec<PlayerRankingSchema>,
    #[serde(default)]
    pub user_ranking_status: String,
    #[serde(default)]
    pub user_world_bloom_chapter_rankings: Vec<UserWorldBloomChapterRanking>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BorderRankingResponse {
    pub event_id: Option<i64>,
    pub is_event_aggregate: Option<bool>,
    #[serde(default)]
    pub border_rankings: Vec<PlayerRankingSchema>,
    #[serde(default)]
    pub user_world_bloom_chapter_ranking_borders: Vec<UserWorldBloomChapterRankingBorder>,
}
