use sea_orm::FromQueryResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::sekai::{UserPlayerFrame, UserProfileHonor};
use crate::model::trace_columns::TracePayload;

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

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum RecordedRankData {
    Normal(RecordedRankingSchema),
    WorldBloom(RecordedWorldBloomRankingSchema),
}

/// Decodes each row once into the superset shape and picks the variant by
/// `characterId`. Untagged deserialization buffered every row into an
/// intermediate tree first (slow and allocation-heavy on large cached traces)
/// and, because `Normal` accepts unknown fields, decoded World Bloom rows as
/// `Normal`, dropping `characterId` from every cached World Bloom trace.
impl<'de> Deserialize<'de> for RecordedRankData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let row = RecordedWorldBloomRankingSchema::deserialize(deserializer)?;
        Ok(match row.character_id {
            Some(_) => RecordedRankData::WorldBloom(row),
            None => RecordedRankData::Normal(RecordedRankingSchema {
                timestamp: row.timestamp,
                user_id: row.user_id,
                score: row.score,
                rank: row.rank,
            }),
        })
    }
}

/// The trace rows of a web detail response. A cached trace is spliced in as
/// its raw `rankData` JSON array (`Raw`), so serving a detail never decodes
/// and re-encodes thousands of rows per request; `Rows` holds typed rows.
///
/// `Raw` serializes verbatim only through sonic-rs (the service's JSON
/// encoder), which is asked to emit the text as a raw value the same way
/// its own `LazyValue` is; other serializers would see a one-field struct.
#[derive(Debug, Clone)]
pub enum TraceRows {
    Rows(Vec<RecordedRankData>),
    Raw {
        /// The array's JSON text.
        text: sonic_rs::FastStr,
    },
}

/// sonic-rs's marker for "write this string as raw JSON" (its
/// `LazyValue` serializes through the same struct name and field). The
/// splice tests pin it: a rename would show up as a quoted string.
const SONIC_RAW_VALUE_TOKEN: &str = "$sonic_rs::LazyValue";

impl Default for TraceRows {
    fn default() -> Self {
        Self::Rows(Vec::new())
    }
}

impl From<Vec<RecordedRankData>> for TraceRows {
    fn from(rows: Vec<RecordedRankData>) -> Self {
        Self::Rows(rows)
    }
}

impl TraceRows {
    /// Borrows the `rankData` array of a cached trace response without
    /// decoding its rows, scanning the document to find it. A response
    /// without `rankData` has no rows.
    pub fn from_trace_json(json: &bytes::Bytes) -> sonic_rs::Result<Self> {
        let value = match sonic_rs::get_from_bytes(json, ["rankData"]) {
            Ok(value) => value,
            Err(err) if err.is_not_found() => return Ok(Self::default()),
            Err(err) => return Err(err),
        };
        if !sonic_rs::JsonValueTrait::is_array(&value) {
            return Err(<sonic_rs::Error as serde::de::Error>::custom(
                "rankData is not an array",
            ));
        }
        Ok(Self::Raw {
            text: value.as_raw_faststr(),
        })
    }

    /// Like [`Self::from_trace_json`] with the array's byte range already
    /// known (see [`SubjectTraceResponseSchema::rank_data_range`]): the
    /// array is sliced out of `json` without a scan, sharing its buffer.
    /// `None` means the response has no rows. A range that doesn't frame an
    /// array in these bytes falls back to scanning.
    pub fn from_trace_json_range(
        json: &bytes::Bytes,
        range: Option<std::ops::Range<usize>>,
    ) -> sonic_rs::Result<Self> {
        let Some(range) = range else {
            return Ok(Self::default());
        };
        let framed = range.start < range.end
            && range.end <= json.len()
            && json[range.start] == b'['
            && json[range.end - 1] == b']';
        if !framed {
            tracing::warn!(
                ?range,
                len = json.len(),
                "cached trace range does not frame an array"
            );
            return Self::from_trace_json(json);
        }
        match sonic_rs::FastStr::from_bytes(json.slice(range)) {
            Ok(text) => Ok(Self::Raw { text }),
            Err(err) => {
                tracing::warn!(%err, "cached trace range is not UTF-8");
                Self::from_trace_json(json)
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Rows(rows) => rows.is_empty(),
            Self::Raw { text } => text
                .strip_prefix('[')
                .is_some_and(|rest| rest.trim_start().starts_with(']')),
        }
    }

    /// The rows, decoding a raw array (O(rows); not for the hot path).
    pub fn to_rows(&self) -> sonic_rs::Result<std::borrow::Cow<'_, [RecordedRankData]>> {
        match self {
            Self::Rows(rows) => Ok(std::borrow::Cow::Borrowed(rows)),
            Self::Raw { text } => sonic_rs::from_str(text).map(std::borrow::Cow::Owned),
        }
    }

    /// Decoded rows for assertions.
    #[cfg(test)]
    pub fn rows(&self) -> Vec<RecordedRankData> {
        self.to_rows().expect("trace rows decode").into_owned()
    }

    /// Mutable typed rows, decoding a raw array in place first.
    pub fn rows_mut(&mut self) -> sonic_rs::Result<&mut Vec<RecordedRankData>> {
        if let Self::Raw { text } = self {
            *self = Self::Rows(sonic_rs::from_str(text)?);
        }
        match self {
            Self::Rows(rows) => Ok(rows),
            Self::Raw { .. } => unreachable!("raw rows were just decoded"),
        }
    }
}

impl Serialize for TraceRows {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        match self {
            Self::Rows(rows) => rows.serialize(serializer),
            Self::Raw { text } => {
                let mut raw = serializer.serialize_struct(SONIC_RAW_VALUE_TOKEN, 1)?;
                raw.serialize_field(SONIC_RAW_VALUE_TOKEN, text.as_str())?;
                raw.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for TraceRows {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Vec::<RecordedRankData>::deserialize(deserializer).map(Self::Rows)
    }
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
    pub user_honor_missions: Vec<Value>,
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

/// Every field of a cached `SubjectTraceResponseSchema` except the rows,
/// which stay a lazily skipped raw slice.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // decoded only to check the shape
struct SubjectTraceShape<'a> {
    meta: LeaderboardMetaSchema,
    subject: SubjectTraceMetaSchema,
    #[serde(default)]
    current: Option<WebRankingItemSchema>,
    #[serde(borrow, default)]
    rank_data: Option<sonic_rs::LazyValue<'a>>,
    #[serde(default)]
    user_data: Option<RecordedUserNameSchema>,
}

impl SubjectTraceResponseSchema {
    /// Whether cached bytes decode as this schema, without decoding every row:
    /// the small fields are decoded in full, `rankData` must be an array and
    /// its first row must decode. Cheap enough for every L2 hit, and strict
    /// enough that typed callers and raw splicing never meet a stale shape.
    pub fn json_is_well_formed(json: &[u8]) -> sonic_rs::Result<()> {
        Self::rank_data_range(json).map(|_| ())
    }

    /// [`Self::json_is_well_formed`] that also reports where the `rankData`
    /// array lies in `json` (`None`: no rows). The check already parses the
    /// document, so the range costs nothing extra, and `TraceRows` can
    /// then slice the array out of the same bytes without rescanning.
    pub fn rank_data_range(json: &[u8]) -> sonic_rs::Result<Option<std::ops::Range<usize>>> {
        let shape: SubjectTraceShape<'_> = sonic_rs::from_slice(json)?;
        let Some(rows) = shape.rank_data else {
            return Ok(None);
        };
        if !sonic_rs::JsonValueTrait::is_array(&rows) {
            return Err(<sonic_rs::Error as serde::de::Error>::custom(
                "rankData is not an array",
            ));
        }
        let raw = rows.as_raw_str();
        match sonic_rs::get_from_str(raw, sonic_rs::pointer![0]) {
            Ok(first) => sonic_rs::from_str::<RecordedRankData>(first.as_raw_str()).map(|_| ())?,
            Err(err) if err.is_not_found() => {}
            Err(err) => return Err(err),
        }
        // The lazy value borrows from `json`, so its address gives the
        // offset; anything else means sonic copied and the range is unknown.
        let base = json.as_ptr() as usize;
        let start = (raw.as_ptr() as usize).wrapping_sub(base);
        let end = start.wrapping_add(raw.len());
        if raw.as_ptr() as usize >= base && end <= json.len() && &json[start..end] == raw.as_bytes()
        {
            Ok(Some(start..end))
        } else {
            Err(<sonic_rs::Error as serde::de::Error>::custom(
                "rankData is not addressable in the cached bytes",
            ))
        }
    }
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
    pub average_round: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_pt: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_pt: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min20_times_3_speed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hour_round: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_start_at: Option<i64>,
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
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub ranks: Vec<CloudRankInfoSchema>,
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
    #[serde(skip_serializing_if = "TracePayload::is_empty", default)]
    pub rank_trace: TracePayload,
    #[serde(skip_serializing_if = "TracePayload::is_empty", default)]
    pub player_trace: TracePayload,
    pub interval_seconds: i64,
    pub window_start: i64,
    pub window_end: i64,
}

/// Present only when the caller looked a player up by raw upstream UID;
/// carries both identifiers for that one player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSubjectSchema {
    pub user_id: String,
    pub unique_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebUserDetailResponseSchema {
    pub meta: LeaderboardMetaSchema,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub subject: Option<WebSubjectSchema>,
    /// `false` when the player was tracked in this event but no longer
    /// holds a tracked rank: `current`, `previous` and `next` are then
    /// absent while `playerTrace` and `profile` still describe them.
    #[serde(default = "ranked_by_default")]
    pub ranked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<WebRankingItemSchema>,
    #[serde(skip_serializing_if = "TracePayload::is_empty", default)]
    pub player_trace: TracePayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<RecordedUserNameSchema>,
}

fn ranked_by_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaderboard_overview_cache_round_trips_honor_missions() {
        let response = LeaderboardOverviewSchema {
            meta: LeaderboardMetaSchema {
                server: "cn".to_owned(),
                event_id: 170,
                scope: "world-bloom/20".to_owned(),
                character_id: Some(20),
                fetched_at: 1_781_675_150,
            },
            overview: WebOverviewSchema {
                top_rankings: vec![WebRankingItemSchema {
                    rank_data: RecordedRankData::WorldBloom(RecordedWorldBloomRankingSchema {
                        timestamp: 1_781_675_101,
                        user_id: "100".to_owned(),
                        score: 147_100_930,
                        rank: 1,
                        character_id: Some(20),
                    }),
                    user_data: Some(RecordedUserNameSchema {
                        user_id: "100".to_owned(),
                        name: "Miku".to_owned(),
                        cheerful_team_id: None,
                        card_id: Some(1041),
                        card_level: Some(60),
                        card_master_rank: Some(5),
                        card_special_training_status: Some("done".to_owned()),
                        card_default_image: Some("special_training".to_owned()),
                        profile_word: Some("hello".to_owned()),
                        profile_honors: Vec::new(),
                        user_honor_missions: vec![
                            serde_json::from_str(
                                r#"{"honorMissionType":"character","progress":3}"#,
                            )
                            .unwrap(),
                        ],
                        user_player_frames: Vec::new(),
                    }),
                }],
                interval_seconds: 3600,
                ..WebOverviewSchema::default()
            },
            window_start: 1_781_671_550,
            window_end: 1_781_675_150,
        };

        let bytes = sonic_rs::to_vec(&response).unwrap();
        let decoded: LeaderboardOverviewSchema = sonic_rs::from_slice(&bytes).unwrap();
        assert_eq!(decoded.overview.top_rankings.len(), 1);
    }
}

#[cfg(test)]
mod trace_rows_tests {
    use super::*;

    fn normal(ts: i64) -> RecordedRankData {
        RecordedRankData::Normal(RecordedRankingSchema {
            timestamp: ts,
            user_id: "u1".into(),
            score: ts * 10,
            rank: 3,
        })
    }

    fn world_bloom(ts: i64) -> RecordedRankData {
        RecordedRankData::WorldBloom(RecordedWorldBloomRankingSchema {
            timestamp: ts,
            user_id: "u2".into(),
            score: ts * 20,
            rank: 5,
            character_id: Some(17),
        })
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Detail {
        #[serde(skip_serializing_if = "TraceRows::is_empty")]
        rank_trace: TraceRows,
        tail: i64,
    }

    fn trace_json(rows: Vec<RecordedRankData>) -> bytes::Bytes {
        let trace = UserAllRankingDataQueryResponseSchema {
            rank_data: rows,
            user_data: None,
        };
        bytes::Bytes::from(sonic_rs::to_vec(&trace).unwrap())
    }

    #[test]
    fn rank_data_decode_keeps_world_bloom_character_id() {
        let rows = vec![normal(1), world_bloom(2)];
        let json = sonic_rs::to_string(&rows).unwrap();
        let back: Vec<RecordedRankData> = sonic_rs::from_str(&json).unwrap();
        assert!(matches!(back[0], RecordedRankData::Normal(_)));
        match &back[1] {
            RecordedRankData::WorldBloom(row) => assert_eq!(row.character_id, Some(17)),
            RecordedRankData::Normal(_) => panic!("world bloom row decoded as normal"),
        }
        assert_eq!(sonic_rs::to_string(&back).unwrap(), json);
        let via_serde_json: Vec<RecordedRankData> = serde_json::from_str(&json).unwrap();
        assert_eq!(sonic_rs::to_string(&via_serde_json).unwrap(), json);
    }

    #[test]
    fn raw_trace_rows_serialize_like_typed_rows() {
        for rows in [
            vec![normal(1), normal(2)],
            vec![world_bloom(1), world_bloom(2)],
        ] {
            let json = trace_json(rows.clone());
            let raw = TraceRows::from_trace_json(&json).unwrap();
            assert!(matches!(raw, TraceRows::Raw { .. }));
            assert!(!raw.is_empty());
            let typed = sonic_rs::to_string(&Detail {
                rank_trace: rows.clone().into(),
                tail: 7,
            })
            .unwrap();
            let spliced = sonic_rs::to_string(&Detail {
                rank_trace: raw.clone(),
                tail: 7,
            })
            .unwrap();
            assert_eq!(spliced, typed);
            assert_eq!(
                sonic_rs::to_string(&raw.rows()).unwrap(),
                sonic_rs::to_string(&rows).unwrap()
            );

            // The sliced fast path yields the same bytes as the scan.
            let start = json.iter().position(|b| *b == b'[').unwrap();
            let end = json.iter().rposition(|b| *b == b']').unwrap() + 1;
            let sliced = TraceRows::from_trace_json_range(&json, Some(start..end)).unwrap();
            let fast = sonic_rs::to_string(&Detail {
                rank_trace: sliced.clone(),
                tail: 7,
            })
            .unwrap();
            assert_eq!(fast, typed);
            assert_eq!(
                sonic_rs::to_string(&sliced.rows()).unwrap(),
                sonic_rs::to_string(&rows).unwrap()
            );
        }
    }

    #[test]
    fn raw_trace_rows_from_range_fall_back_to_scanning_when_the_range_is_off() {
        let rows = vec![normal(1), world_bloom(2)];
        let json = trace_json(rows.clone());
        let typed = sonic_rs::to_string(&rows).unwrap();
        let start = json.iter().position(|b| *b == b'[').unwrap();
        let end = json.iter().rposition(|b| *b == b']').unwrap() + 1;
        for range in [
            start + 1..end,
            start..end - 1,
            start..end + 1,
            end..start,
            0..json.len(),
            start..json.len() + 10,
        ] {
            let rows = TraceRows::from_trace_json_range(&json, Some(range.clone())).unwrap();
            assert_eq!(sonic_rs::to_string(&rows).unwrap(), typed, "{range:?}");
        }
        assert!(
            TraceRows::from_trace_json_range(&json, None)
                .unwrap()
                .is_empty()
        );
        // A range that frames some other array is a caller bug the check
        // can't see, so the range must come from the same bytes.
        let text = String::from_utf8(json.to_vec()).unwrap();
        let padded = bytes::Bytes::from(format!("{text}          "));
        let rows = TraceRows::from_trace_json_range(&padded, Some(start..end)).unwrap();
        assert_eq!(sonic_rs::to_string(&rows).unwrap(), typed);
    }

    #[test]
    fn raw_trace_rows_handle_empty_missing_and_malformed_rank_data() {
        let missing = TraceRows::from_trace_json(&trace_json(Vec::new())).unwrap();
        assert!(missing.is_empty());
        let empty =
            TraceRows::from_trace_json(&bytes::Bytes::from_static(br#"{"rankData":[ ]}"#)).unwrap();
        assert!(empty.is_empty());
        assert_eq!(
            sonic_rs::to_string(&Detail {
                rank_trace: empty,
                tail: 1
            })
            .unwrap(),
            r#"{"tail":1}"#
        );
        assert!(
            TraceRows::from_trace_json(&bytes::Bytes::from_static(br#"{"rankData":{}}"#)).is_err()
        );
        assert!(TraceRows::from_trace_json(&bytes::Bytes::from_static(b"not json")).is_err());
    }

    #[test]
    fn raw_is_empty_strips_only_one_bracket() {
        let nested =
            TraceRows::from_trace_json(&bytes::Bytes::from_static(br#"{"rankData":[[],[]]}"#))
                .unwrap();
        assert!(!nested.is_empty());
    }

    fn subject_trace(rows: Vec<RecordedRankData>) -> Vec<u8> {
        sonic_rs::to_vec(&SubjectTraceResponseSchema {
            meta: LeaderboardMetaSchema {
                server: "jp".into(),
                event_id: 1,
                scope: "total".into(),
                character_id: None,
                fetched_at: 1,
            },
            subject: SubjectTraceMetaSchema {
                subject_type: "rank".into(),
                subject: "3".into(),
                resolved_user_id: Some("u1".into()),
                resolved_rank: Some(3),
            },
            current: None,
            rank_data: rows,
            user_data: None,
        })
        .unwrap()
    }

    #[test]
    fn subject_trace_shape_check_accepts_valid_and_rejects_stale_shapes() {
        let valid = subject_trace(vec![normal(1), normal(2)]);
        SubjectTraceResponseSchema::json_is_well_formed(&valid).unwrap();
        SubjectTraceResponseSchema::json_is_well_formed(&subject_trace(Vec::new())).unwrap();
        assert_eq!(
            SubjectTraceResponseSchema::rank_data_range(&subject_trace(Vec::new())).unwrap(),
            None
        );
        let range = SubjectTraceResponseSchema::rank_data_range(&valid)
            .unwrap()
            .unwrap();
        assert_eq!(
            std::str::from_utf8(&valid[range.clone()]).unwrap(),
            sonic_rs::to_string(&vec![normal(1), normal(2)]).unwrap()
        );
        let bytes = bytes::Bytes::from(valid.clone());
        let spliced = TraceRows::from_trace_json_range(&bytes, Some(range)).unwrap();
        assert_eq!(
            sonic_rs::to_string(&spliced).unwrap(),
            sonic_rs::to_string(&TraceRows::from_trace_json(&bytes).unwrap()).unwrap()
        );
        // A World Bloom trace too, whose rows carry an extra field.
        let wb = subject_trace(vec![world_bloom(1)]);
        let range = SubjectTraceResponseSchema::rank_data_range(&wb)
            .unwrap()
            .unwrap();
        assert_eq!(&wb[range.start..range.start + 1], b"[");
        assert_eq!(&wb[range.end - 1..range.end], b"]");
        let text = String::from_utf8(valid).unwrap();
        let rejected = [
            text.replacen(r#""meta":"#, r#""metaOld":"#, 1),
            text.replacen(r#""subjectType":"rank""#, r#""subjectType":7"#, 1),
            text.replacen(r#""rankData":["#, r#""rankData":{"rows":["#, 1)
                .replacen("]}", "]}}", 1),
            text.replacen(r#""score":10"#, r#""points":10"#, 1),
            "not json".to_owned(),
        ];
        for bad in rejected {
            assert!(
                SubjectTraceResponseSchema::json_is_well_formed(bad.as_bytes()).is_err(),
                "{bad}"
            );
            assert!(
                SubjectTraceResponseSchema::rank_data_range(bad.as_bytes()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn rows_mut_decodes_raw_rows_in_place() {
        let mut rows = TraceRows::from_trace_json(&trace_json(vec![normal(1)])).unwrap();
        rows.rows_mut().unwrap()[0] = normal(9);
        assert!(matches!(rows, TraceRows::Rows(_)));
        assert_eq!(
            sonic_rs::to_string(&rows).unwrap(),
            sonic_rs::to_string(&vec![normal(9)]).unwrap()
        );
    }
}
