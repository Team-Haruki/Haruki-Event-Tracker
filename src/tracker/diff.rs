//! Pure helpers for the tracker tick: rank-based diff, top100/border
//! merge, and conversions from upstream `PlayerRankingSchema` shapes into
//! the DB record schemas. No I/O, no clock, no Redis — these are unit
//! tested in this file and re-used from `tracker::base` / `tracker::daemon`.
//!
//! Maps directly onto the Go helpers in `tracker/trackerbase.go`:
//!   `diffRankBased`, `mergeRankings`,
//!   `mergeWorldBloomRankingsForCharacter`,
//!   `processWorldBloomChapter`, `extractWorldBloomRankings`,
//!   `extractWorldBloomBorderRankings`, `buildEventRecords`,
//!   `buildWorldBloomRows`, `extractCheerfulTeamID`.
//!
//! Go's `buildEventRows` and `getFilterFunc` are dead code in the Go
//! repo (no live caller; the trivial filter is unreachable) and are
//! intentionally not ported.

use std::collections::{HashMap, HashSet};

use crate::model::enums::SekaiEventStatus;
use crate::model::event::WorldBloomChapterStatus;
use crate::model::sekai::{
    PlayerRankingSchema, UserWorldBloomChapterRanking, UserWorldBloomChapterRankingBorder,
};
use crate::model::tracker::{
    PlayerEventRankingRecordSchema, PlayerWorldBloomRankingRecordSchema, RankState,
};

/// Pull `cheerful_carnival_team_id` out of a player ranking, if present.
pub fn extract_cheerful_team_id(r: &PlayerRankingSchema) -> Option<i64> {
    r.user_cheerful_carnival
        .as_ref()
        .and_then(|c| c.cheerful_carnival_team_id)
}

/// Indices into `rankings` whose `(user_id, score)` differ from
/// `prev_rank_state[rank]`. Mutates `prev_rank_state` in-place to reflect
/// the new state, and returns the per-rank changes so the caller can
/// persist them to Redis (key `haruki:tracker:<server>:<event>:rank_state`).
///
/// Rows missing any of `rank` / `score` / `user_id` are silently skipped —
/// matches Go's `if r.Rank == nil || r.Score == nil || r.UserID == nil`.
pub fn diff_rank_based(
    rankings: &[PlayerRankingSchema],
    prev_rank_state: &mut HashMap<i64, RankState>,
) -> (Vec<usize>, HashMap<i64, RankState>) {
    let mut changed_idx = Vec::new();
    let mut changed_ranks = HashMap::new();
    for (i, r) in rankings.iter().enumerate() {
        let (Some(rank), Some(score), Some(uid)) = (r.rank, r.score, r.user_id) else {
            continue;
        };
        let user_id = uid.to_string();
        let new_state = RankState {
            user_id: user_id.clone(),
            score,
        };
        match prev_rank_state.get(&rank) {
            Some(prev) if prev.score == score && prev.user_id == user_id => {}
            _ => {
                prev_rank_state.insert(rank, new_state.clone());
                changed_ranks.insert(rank, new_state);
                changed_idx.push(i);
            }
        }
    }
    (changed_idx, changed_ranks)
}

/// Top-100 rankings concatenated with border rankings whose rank is not
/// already covered by top-100. Owned input → owned output to match Go's
/// pointer semantics in a Rust idiom.
pub fn merge_rankings(
    top100: Vec<PlayerRankingSchema>,
    border: Vec<PlayerRankingSchema>,
) -> Vec<PlayerRankingSchema> {
    let top_ranks: HashSet<i64> = top100.iter().filter_map(|r| r.rank).collect();
    let mut out = Vec::with_capacity(top100.len() + border.len());
    out.extend(top100);
    for b in border {
        match b.rank {
            Some(rk) if !top_ranks.contains(&rk) => out.push(b),
            _ => {}
        }
    }
    out
}

pub fn merge_world_bloom_rankings_for_character(
    top100: Vec<PlayerRankingSchema>,
    border: Vec<PlayerRankingSchema>,
) -> Vec<PlayerRankingSchema> {
    if border.is_empty() {
        return top100;
    }
    merge_rankings(top100, border)
}

/// Decides whether a single World Bloom chapter row should be tracked
/// this tick. Returns `(character_id, rankings)` when yes; `None` when:
/// the chapter is missing a character id, isn't in the tracker's
/// `statuses` map, is in the upstream-aggregating phase, hasn't started
/// yet, or already had its final-row write (`is_chapter_ended[char] == true`).
pub fn process_world_bloom_chapter(
    chapter: UserWorldBloomChapterRanking,
    statuses: &HashMap<i64, WorldBloomChapterStatus>,
    is_chapter_ended: &HashMap<i64, bool>,
) -> Option<(i64, Vec<PlayerRankingSchema>)> {
    let char_id = chapter.base.game_character_id?;
    let status = statuses.get(&char_id)?;
    if chapter.base.is_world_bloom_chapter_aggregate.unwrap_or(false) {
        return None;
    }
    let chapter_ended = is_chapter_ended.get(&char_id).copied().unwrap_or(false);
    let should_track = match status.chapter_status {
        SekaiEventStatus::Ongoing => true,
        SekaiEventStatus::Ended if !chapter_ended => true,
        _ => false,
    };
    if should_track && !chapter.rankings.is_empty() {
        Some((char_id, chapter.rankings))
    } else {
        None
    }
}

/// Find the border-ranking entry for `character_id`. Returns the rankings
/// slice unchanged if found (even when empty), `None` otherwise.
pub fn extract_world_bloom_border_rankings(
    borders: Vec<UserWorldBloomChapterRankingBorder>,
    character_id: i64,
) -> Vec<PlayerRankingSchema> {
    for b in borders {
        if b.base.game_character_id == Some(character_id) {
            return b.border_rankings;
        }
    }
    Vec::new()
}

/// Build the per-character "merged top100+border" map for World Bloom
/// events. Equivalent to Go `extractWorldBloomRankings`.
pub fn extract_world_bloom_rankings(
    top100_chapters: Vec<UserWorldBloomChapterRanking>,
    border_chapters: Vec<UserWorldBloomChapterRankingBorder>,
    statuses: &HashMap<i64, WorldBloomChapterStatus>,
    is_chapter_ended: &HashMap<i64, bool>,
) -> HashMap<i64, Vec<PlayerRankingSchema>> {
    let mut out = HashMap::new();
    if top100_chapters.is_empty() {
        return out;
    }
    let mut borders_by_char: HashMap<i64, Vec<PlayerRankingSchema>> = HashMap::new();
    for b in border_chapters {
        if let Some(cid) = b.base.game_character_id {
            borders_by_char.insert(cid, b.border_rankings);
        }
    }
    for chapter in top100_chapters {
        if let Some((char_id, rankings)) = process_world_bloom_chapter(chapter, statuses, is_chapter_ended) {
            let border = borders_by_char.remove(&char_id).unwrap_or_default();
            out.insert(char_id, merge_world_bloom_rankings_for_character(rankings, border));
        }
    }
    out
}

/// Convert rank-based diff'd rows into DB-bound records, deduplicating by
/// `user_id` (the same user can occupy two ranks in flight; we want one
/// row). Rows missing any of `name` / `rank` / `score` / `user_id` are
/// dropped — matches Go's nil-pointer skip in `addRecord`.
pub fn build_event_records(
    record_time: i64,
    diffed: &[&PlayerRankingSchema],
) -> Vec<PlayerEventRankingRecordSchema> {
    let mut seen: HashMap<String, PlayerEventRankingRecordSchema> = HashMap::new();
    for r in diffed {
        let (Some(rank), Some(score), Some(uid)) = (r.rank, r.score, r.user_id) else {
            continue;
        };
        let Some(name) = r.name.clone() else {
            continue;
        };
        let user_id = uid.to_string();
        seen.entry(user_id.clone()).or_insert(PlayerEventRankingRecordSchema {
            timestamp: record_time,
            user_id,
            name,
            score,
            rank,
            cheerful_team_id: extract_cheerful_team_id(r),
        });
    }
    seen.into_values().collect()
}

pub fn build_world_bloom_rows(
    record_time: i64,
    per_char: &HashMap<i64, Vec<PlayerRankingSchema>>,
) -> Vec<PlayerWorldBloomRankingRecordSchema> {
    let mut out = Vec::new();
    for (&character_id, rankings) in per_char {
        for r in rankings {
            let (Some(rank), Some(score), Some(uid)) = (r.rank, r.score, r.user_id) else {
                continue;
            };
            let Some(name) = r.name.clone() else {
                continue;
            };
            out.push(PlayerWorldBloomRankingRecordSchema {
                base: PlayerEventRankingRecordSchema {
                    timestamp: record_time,
                    user_id: uid.to_string(),
                    name,
                    score,
                    rank,
                    cheerful_team_id: extract_cheerful_team_id(r),
                },
                character_id,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::sekai::UserCheerfulCarnival;

    fn ranking(rank: i64, user_id: i64, score: i64, name: &str) -> PlayerRankingSchema {
        PlayerRankingSchema {
            name: Some(name.to_string()),
            rank: Some(rank),
            score: Some(score),
            user_id: Some(user_id),
            user_cheerful_carnival: None,
        }
    }

    #[test]
    fn diff_first_pass_marks_everything_changed() {
        let rows = vec![ranking(1, 100, 1000, "a"), ranking(2, 200, 900, "b")];
        let mut state = HashMap::new();
        let (changed, deltas) = diff_rank_based(&rows, &mut state);
        assert_eq!(changed, vec![0, 1]);
        assert_eq!(deltas.len(), 2);
        assert_eq!(state.len(), 2);
        assert_eq!(state[&1].user_id, "100");
        assert_eq!(state[&1].score, 1000);
    }

    #[test]
    fn diff_second_pass_no_changes_yields_empty() {
        let rows = vec![ranking(1, 100, 1000, "a")];
        let mut state = HashMap::new();
        diff_rank_based(&rows, &mut state);
        let (changed, deltas) = diff_rank_based(&rows, &mut state);
        assert!(changed.is_empty());
        assert!(deltas.is_empty());
    }

    #[test]
    fn diff_score_change_only_marks_that_rank() {
        let initial = vec![ranking(1, 100, 1000, "a"), ranking(2, 200, 900, "b")];
        let mut state = HashMap::new();
        diff_rank_based(&initial, &mut state);

        let updated = vec![ranking(1, 100, 1100, "a"), ranking(2, 200, 900, "b")];
        let (changed, deltas) = diff_rank_based(&updated, &mut state);
        assert_eq!(changed, vec![0]);
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[&1].score, 1100);
        assert_eq!(state[&1].score, 1100);
        assert_eq!(state[&2].score, 900);
    }

    #[test]
    fn diff_user_change_at_same_rank_is_a_change() {
        let mut state = HashMap::new();
        diff_rank_based(&[ranking(1, 100, 1000, "a")], &mut state);
        let (changed, _) = diff_rank_based(&[ranking(1, 999, 1000, "x")], &mut state);
        assert_eq!(changed, vec![0]);
        assert_eq!(state[&1].user_id, "999");
    }

    #[test]
    fn diff_skips_rows_with_missing_fields() {
        let rows = vec![PlayerRankingSchema {
            name: Some("a".into()),
            rank: None,
            score: Some(1),
            user_id: Some(1),
            user_cheerful_carnival: None,
        }];
        let mut state = HashMap::new();
        let (changed, deltas) = diff_rank_based(&rows, &mut state);
        assert!(changed.is_empty());
        assert!(deltas.is_empty());
        assert!(state.is_empty());
    }

    #[test]
    fn merge_dedupes_by_rank_keeping_top100() {
        let top = vec![ranking(1, 100, 1000, "a"), ranking(2, 200, 900, "b")];
        // Border rank=2 must be dropped, rank=1000 kept.
        let border = vec![ranking(2, 999, 0, "ghost"), ranking(1000, 300, 50, "c")];
        let merged = merge_rankings(top, border);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].user_id, Some(100));
        assert_eq!(merged[2].user_id, Some(300));
    }

    #[test]
    fn merge_drops_border_rows_with_no_rank() {
        let top = vec![ranking(1, 100, 1000, "a")];
        let border = vec![PlayerRankingSchema {
            name: Some("x".into()),
            rank: None,
            score: Some(0),
            user_id: Some(2),
            user_cheerful_carnival: None,
        }];
        let merged = merge_rankings(top, border);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn extract_cheerful_handles_nested_options() {
        let mut r = ranking(1, 100, 1000, "a");
        assert_eq!(extract_cheerful_team_id(&r), None);
        r.user_cheerful_carnival = Some(UserCheerfulCarnival {
            cheerful_carnival_team_id: Some(7),
        });
        assert_eq!(extract_cheerful_team_id(&r), Some(7));
    }

    #[test]
    fn build_event_records_dedupes_by_user_id() {
        let r1 = ranking(1, 100, 1000, "a");
        let r2 = ranking(50, 100, 500, "a"); // same user at two ranks
        let diffed: Vec<&PlayerRankingSchema> = vec![&r1, &r2];
        let recs = build_event_records(123, &diffed);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].user_id, "100");
    }

    #[test]
    fn process_chapter_skips_aggregating_flag() {
        let mut statuses = HashMap::new();
        statuses.insert(
            5,
            WorldBloomChapterStatus {
                server: crate::model::enums::SekaiServerRegion::Jp,
                event_id: 100,
                character_id: 5,
                chapter_status: SekaiEventStatus::Ongoing,
            },
        );
        let chapter = UserWorldBloomChapterRanking {
            base: crate::model::sekai::UserWorldBloomChapterRankingBase {
                event_id: Some(100),
                game_character_id: Some(5),
                is_world_bloom_chapter_aggregate: Some(true),
            },
            rankings: vec![ranking(1, 100, 1000, "a")],
        };
        assert!(process_world_bloom_chapter(chapter, &statuses, &HashMap::new()).is_none());
    }

    #[test]
    fn process_chapter_tracks_ended_chapter_once() {
        let mut statuses = HashMap::new();
        statuses.insert(
            5,
            WorldBloomChapterStatus {
                server: crate::model::enums::SekaiServerRegion::Jp,
                event_id: 100,
                character_id: 5,
                chapter_status: SekaiEventStatus::Ended,
            },
        );
        let make_chapter = || UserWorldBloomChapterRanking {
            base: crate::model::sekai::UserWorldBloomChapterRankingBase {
                event_id: Some(100),
                game_character_id: Some(5),
                is_world_bloom_chapter_aggregate: Some(false),
            },
            rankings: vec![ranking(1, 100, 1000, "a")],
        };
        let mut ended = HashMap::new();
        // First time: should record the final write.
        assert!(process_world_bloom_chapter(make_chapter(), &statuses, &ended).is_some());
        // Second time, after the daemon flips the flag, should skip.
        ended.insert(5, true);
        assert!(process_world_bloom_chapter(make_chapter(), &statuses, &ended).is_none());
    }
}
