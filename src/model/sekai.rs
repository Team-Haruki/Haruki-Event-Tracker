use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserCheerfulCarnival {
    pub cheerful_carnival_team_id: Option<i64>,
    pub event_id: Option<i64>,
    pub register_at: Option<i64>,
    pub team_change_count: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserCard {
    pub card_id: Option<i64>,
    pub level: Option<i64>,
    pub master_rank: Option<i64>,
    pub special_training_status: Option<String>,
    pub default_image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserProfile {
    pub word: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserProfileHonor {
    pub seq: Option<i64>,
    pub profile_honor_type: Option<String>,
    pub honor_id: Option<i64>,
    pub honor_level: Option<i64>,
    pub bonds_honor_view_type: Option<String>,
    pub bonds_honor_word_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserPlayerFrame {
    pub player_frame_id: Option<i64>,
    pub player_frame_attach_status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerRankingSchema {
    pub is_own: Option<bool>,
    pub name: Option<String>,
    pub rank: Option<i64>,
    pub score: Option<i64>,
    pub user_id: Option<i64>,
    pub user_card: Option<UserCard>,
    pub user_profile: Option<UserProfile>,
    #[serde(default)]
    pub user_profile_honors: Vec<UserProfileHonor>,
    pub user_cheerful_carnival: Option<UserCheerfulCarnival>,
    #[serde(default)]
    pub user_honor_missions: Vec<Value>,
    #[serde(default)]
    pub user_player_frames: Vec<UserPlayerFrame>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extended_player_profile_fields() {
        let raw = r#"{
            "userId": 132110839700791298,
            "score": 89688280,
            "rank": 1,
            "isOwn": false,
            "name": "星屑ユートピア",
            "userCard": {
                "cardId": 1404,
                "level": 60,
                "masterRank": 5,
                "specialTrainingStatus": "done",
                "defaultImage": "special_training"
            },
            "userProfile": {
                "userId": 132110839700791298,
                "word": "なーんせんす文学",
                "twitterId": "NanSense0430",
                "profileImageType": "leader"
            },
            "userProfileHonors": [
                {
                    "seq": 1,
                    "profileHonorType": "normal",
                    "honorId": 95,
                    "honorLevel": 9,
                    "bondsHonorViewType": "none",
                    "bondsHonorWordId": 0
                }
            ],
            "userCheerfulCarnival": {},
            "userHonorMissions": [
                {
                    "honorMissionType": "character",
                    "honorMissionId": 1001,
                    "progress": 3
                }
            ],
            "userPlayerFrames": [
                {
                    "playerFrameId": 10050,
                    "playerFrameAttachStatus": "first"
                }
            ]
        }"#;

        let row: PlayerRankingSchema = sonic_rs::from_str(raw).unwrap();
        assert_eq!(row.user_id, Some(132110839700791298));
        assert_eq!(row.user_card.as_ref().unwrap().card_id, Some(1404));
        assert_eq!(
            row.user_profile.as_ref().unwrap().word.as_deref(),
            Some("なーんせんす文学")
        );
        assert_eq!(row.user_profile_honors[0].honor_id, Some(95));
        assert_eq!(row.user_honor_missions.len(), 1);
        assert_eq!(row.user_player_frames[0].player_frame_id, Some(10050));

        let honors_json = sonic_rs::to_string(&row.user_profile_honors).unwrap();
        assert!(honors_json.contains("profileHonorType"));
        let missions_json = sonic_rs::to_string(&row.user_honor_missions).unwrap();
        assert!(missions_json.contains("honorMissionType"));
        let missions_roundtrip: Vec<Value> = sonic_rs::from_str(&missions_json).unwrap();
        assert_eq!(missions_roundtrip.len(), 1);
        let frames_json = sonic_rs::to_string(&row.user_player_frames).unwrap();
        assert!(frames_json.contains("playerFrameAttachStatus"));
    }
}
