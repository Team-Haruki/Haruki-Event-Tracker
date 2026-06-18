use crate::model::api::{LeaderboardMetaSchema, RecordedRankData, WebRankingItemSchema};

pub(super) const DEFAULT_INTERVAL_SECONDS: i64 = 3600;

pub(super) fn rank_of_item(item: &WebRankingItemSchema) -> Option<i64> {
    match &item.rank_data {
        RecordedRankData::Normal(data) => Some(data.rank),
        RecordedRankData::WorldBloom(data) => Some(data.rank),
    }
}

pub(super) fn user_id_of_rank_data(rank_data: &RecordedRankData) -> Option<String> {
    match rank_data {
        RecordedRankData::Normal(data) => Some(data.user_id.clone()),
        RecordedRankData::WorldBloom(data) => Some(data.user_id.clone()),
    }
}

pub(super) fn meta(
    server: &str,
    event_id: i64,
    character_id: Option<i64>,
    fetched_at: i64,
) -> LeaderboardMetaSchema {
    LeaderboardMetaSchema {
        server: server.to_owned(),
        event_id,
        scope: match character_id {
            Some(character_id) => format!("world-bloom/{character_id}"),
            None => "total".to_owned(),
        },
        character_id,
        fetched_at,
    }
}

pub(super) fn interval_seconds(interval: Option<i64>) -> i64 {
    interval
        .unwrap_or(DEFAULT_INTERVAL_SECONDS)
        .clamp(1, 86_400)
}

pub(super) fn positive_timestamp(timestamp: Option<i64>) -> Option<i64> {
    timestamp.filter(|timestamp| *timestamp > 0)
}

pub(super) fn join_ranks(ranks: &[i64]) -> String {
    ranks
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_defaults_and_clamps() {
        assert_eq!(interval_seconds(None), DEFAULT_INTERVAL_SECONDS);
        assert_eq!(interval_seconds(Some(0)), 1);
        assert_eq!(interval_seconds(Some(90_000)), 86_400);
    }

    #[test]
    pub(super) fn meta_uses_stable_scope_names() {
        assert_eq!(meta("cn", 170, None, 1).scope, "total");
        assert_eq!(meta("cn", 170, Some(19), 1).scope, "world-bloom/19");
    }

    #[test]
    pub(super) fn join_ranks_preserves_request_order() {
        assert_eq!(join_ranks(&[10, 1, 100]), "10,1,100");
    }
}
