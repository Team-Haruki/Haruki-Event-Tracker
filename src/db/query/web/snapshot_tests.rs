//! Rank snapshots on generated tracker histories: close-score swaps,
//! entries and exits at the bottom edge, and — the reported failure — a
//! rank a player left that is only rewritten a few samples later, so the
//! plain "latest row per rank" lists that player twice. Rows are written
//! through the real flush path (`batch_insert_flush`), several samples per
//! flush. The SQLite run is part of `cargo test`; the PostgreSQL run is
//! gated on `HET_TEST_PG_URL`.

use std::collections::{BTreeMap, HashMap, HashSet};

use sea_orm::{ConnectionTrait, DatabaseBackend};

use crate::db::engine::DatabaseEngine;
use crate::db::query::batch::batch_insert_flush;
use crate::db::query::edge::tests::{Rng, quiet_connect};
use crate::db::query::user::PublicUserIdMode;
use crate::db::query::web::{
    RankSnapshotCut, RankingPageRow, WorldBloomRankingPageRow, latest_rank_cut, rank_snapshot_rows,
    user_rank_as_of, world_bloom_rank_snapshot_rows,
};
use crate::db::schema::create_event_tables;
use crate::db::table_name::{TableKind, intern};
use crate::model::enums::SekaiServerRegion;
use crate::model::tracker::{
    PlayerEventRankingRecordSchema, PlayerProfileSchema, PlayerWorldBloomRankingRecordSchema,
};
use crate::privacy::UidAnonymizer;

const T0: i64 = 1_760_000_000;
const TOP: i64 = 100;
const PLAYERS: i64 = 130;
const SAMPLES: usize = 160;
const CHARACTER: i64 = 17;

/// One stored row: `(timestamp, player, score, rank)`.
type Row = (i64, i64, i64, i64);

struct History {
    /// Rows in write order, grouped by flush.
    flushes: Vec<Vec<Row>>,
}

fn record(row: &Row) -> PlayerEventRankingRecordSchema {
    let (timestamp, player, score, rank) = *row;
    PlayerEventRankingRecordSchema {
        timestamp,
        user_id: player.to_string(),
        name: format!("p{player}"),
        score,
        rank,
        cheerful_team_id: None,
        profile: PlayerProfileSchema::default(),
    }
}

/// Standings evolve by small score gains (so neighbours swap often and
/// players cross the rank-100 edge both ways); each sample stores only the
/// ranks whose occupant or score changed, like the rank-based diff. With
/// `lag_pct`, a changed bottom-edge rank is held back a few samples — the
/// history a writer produced when a sample listed a player twice or
/// dropped rows — so stale occupants exist in the table.
fn generate(rng: &mut Rng, lag_pct: u64) -> History {
    let mut scores: Vec<i64> = (0..PLAYERS).map(|p| 1_000_000 - p * 100).collect();
    let mut stored: HashMap<i64, (i64, i64)> = HashMap::new();
    let mut delayed: Vec<(usize, i64)> = Vec::new();
    let mut flushes = Vec::new();
    let mut flush = Vec::new();
    let mut flush_left = rng.range(1, 6);
    let mut ts = T0;
    for sample in 0..SAMPLES {
        ts += rng.range(1, 3);
        for _ in 0..rng.range(1, 12) {
            let player = rng.range(0, PLAYERS - 1) as usize;
            scores[player] += rng.range(1, 400);
        }
        let mut order: Vec<i64> = (0..PLAYERS).collect();
        order.sort_by_key(|&p| (std::cmp::Reverse(scores[p as usize]), p));
        let truth: BTreeMap<i64, (i64, i64)> = order
            .iter()
            .take(TOP as usize)
            .enumerate()
            .map(|(i, &p)| (i as i64 + 1, (p, scores[p as usize])))
            .collect();

        let mut write: Vec<i64> = Vec::new();
        for (&rank, &occupant) in &truth {
            if stored.get(&rank) == Some(&occupant) {
                continue;
            }
            if rank >= TOP - 3 && rng.chance(lag_pct) {
                delayed.push((sample + rng.range(1, 8) as usize, rank));
                continue;
            }
            write.push(rank);
        }
        delayed.retain(|&(due, rank)| {
            if due <= sample {
                write.push(rank);
                false
            } else {
                true
            }
        });
        write.sort_unstable();
        write.dedup();
        let mut players = HashSet::new();
        for rank in write {
            let occupant = truth[&rank];
            // One row per player per sample (the primary key); a delayed
            // rank whose occupant was already written this sample waits.
            if stored.get(&rank) == Some(&occupant) || !players.insert(occupant.0) {
                continue;
            }
            stored.insert(rank, occupant);
            flush.push((ts, occupant.0, occupant.1, rank));
        }
        flush_left -= 1;
        if flush_left == 0 || sample + 1 == SAMPLES {
            flushes.push(std::mem::take(&mut flush));
            flush_left = rng.range(1, 6);
        }
    }
    History { flushes }
}

/// Every row stored by the first `flushes` flushes.
fn rows_until(history: &History, flushes: usize) -> Vec<Row> {
    history.flushes[..flushes]
        .iter()
        .flatten()
        .copied()
        .collect()
}

/// The plain per-rank reconstruction the readers used before: latest row
/// per rank, stale occupants included.
fn naive_latest_per_rank(rows: &[Row]) -> BTreeMap<i64, (i64, i64)> {
    let mut out = BTreeMap::new();
    for &(ts, player, _, rank) in rows {
        if out.get(&rank).is_none_or(|&(seen, _)| ts >= seen) {
            out.insert(rank, (ts, player));
        }
    }
    out
}

/// What a snapshot at this state must show: each rank's latest row, minus
/// ranks whose player has a newer row elsewhere.
fn expected_snapshot(rows: &[Row]) -> BTreeMap<i64, (i64, i64, i64)> {
    let mut newest_by_player: HashMap<i64, i64> = HashMap::new();
    for &(ts, player, _, _) in rows {
        let newest = newest_by_player.entry(player).or_insert(ts);
        *newest = (*newest).max(ts);
    }
    let mut latest: BTreeMap<i64, Row> = BTreeMap::new();
    for row in rows {
        if latest.get(&row.3).is_none_or(|seen| row.0 >= seen.0) {
            latest.insert(row.3, *row);
        }
    }
    latest
        .into_iter()
        .filter(|(_, row)| newest_by_player[&row.1] == row.0)
        .map(|(rank, (ts, player, score, _))| (rank, (ts, player, score)))
        .collect()
}

fn snapshot_map(rows: &[RankingPageRow]) -> BTreeMap<i64, (i64, i64, i64)> {
    rows.iter()
        .map(|row| {
            (
                row.rank(),
                (row.timestamp(), row.user_id().parse().unwrap(), row.score()),
            )
        })
        .collect()
}

fn assert_one_rank_per_player(rows: &[RankingPageRow], context: &str) {
    let mut players = HashSet::new();
    let mut ranks = HashSet::new();
    for row in rows {
        assert!(
            players.insert(row.user_id().to_owned()),
            "{context}: player {} listed twice",
            row.user_id()
        );
        assert!(
            ranks.insert(row.rank()),
            "{context}: rank {} twice",
            row.rank()
        );
    }
}

async fn reset_event(engine: &DatabaseEngine, event_id: i64, world_bloom: bool) {
    for kind in [
        TableKind::Event,
        TableKind::WorldBloom,
        TableKind::EventUsers,
        TableKind::TimeId,
    ] {
        engine
            .conn()
            .execute_unprepared(&format!("DROP TABLE IF EXISTS {}", intern(kind, event_id)))
            .await
            .unwrap();
    }
    create_event_tables(engine, SekaiServerRegion::Jp, event_id, world_bloom)
        .await
        .unwrap();
}

async fn run_generated(engine: &DatabaseEngine, first_event_id: i64, seeds: u64) {
    let anonymizer = UidAnonymizer::enabled("snapshot-test");
    let mode = PublicUserIdMode::Raw;
    let top: Vec<i64> = (1..=TOP).collect();
    let mut naive_duplicates = 0;
    for seed in 0..seeds {
        let event_id = first_event_id + seed as i64;
        let mut rng = Rng::new(seed + 11);
        let history = generate(&mut rng, 25);
        reset_event(engine, event_id, false).await;

        let mut pinned: Vec<(i64, Vec<RankingPageRow>)> = Vec::new();
        for (index, flush) in history.flushes.iter().enumerate() {
            let records: Vec<_> = flush.iter().map(record).collect();
            batch_insert_flush(
                engine,
                SekaiServerRegion::Jp,
                event_id,
                &anonymizer,
                &records,
                &[],
                &mut HashMap::new(),
                &mut HashMap::new(),
            )
            .await
            .unwrap();

            let rows = rows_until(&history, index + 1);
            let naive = naive_latest_per_rank(&rows);
            let naive_players: HashSet<i64> = naive.values().map(|&(_, p)| p).collect();
            naive_duplicates += naive.len() - naive_players.len();

            let cut = RankSnapshotCut {
                at: None,
                as_of_time_id: latest_rank_cut(engine, event_id, None).await.unwrap(),
            };
            let snapshot = rank_snapshot_rows(engine, event_id, &top, cut, mode)
                .await
                .unwrap();
            let context = format!("seed {seed} flush {index}");
            assert_one_rank_per_player(&snapshot, &context);
            assert_eq!(
                snapshot_map(&snapshot),
                expected_snapshot(&rows),
                "{context}"
            );

            // A bot's separate rank-N / N±1 lookups at the same cut agree
            // with the full view.
            let full = snapshot_map(&snapshot);
            for _ in 0..4 {
                let n = rng.range(1, TOP);
                let ranks: Vec<i64> = (n - 1..=n + 1).filter(|r| *r >= 1).collect();
                let part = rank_snapshot_rows(engine, event_id, &ranks, cut, mode)
                    .await
                    .unwrap();
                let expected: BTreeMap<_, _> = full
                    .iter()
                    .filter(|(rank, _)| ranks.contains(rank))
                    .map(|(rank, row)| (*rank, *row))
                    .collect();
                assert_eq!(snapshot_map(&part), expected, "{context} ranks {ranks:?}");
            }

            // The rank a player is found at is the rank the snapshot at the
            // same cut shows them at (or, if their row there is stale, they
            // are shown nowhere).
            for _ in 0..4 {
                let player = rng.range(0, PLAYERS - 1);
                let rank = user_rank_as_of(engine, event_id, None, &player.to_string(), cut, mode)
                    .await
                    .unwrap();
                let shown = full
                    .iter()
                    .find(|(_, row)| row.1 == player)
                    .map(|(r, _)| *r);
                if let Some(shown) = shown {
                    assert_eq!(rank, Some(shown), "{context} player {player}");
                }
            }
            pinned.push((index as i64, snapshot));
        }

        // Flushes that landed after a cut do not change what that cut
        // reads: a request pinned to an older epoch's cut stays consistent
        // with everything else answered under it.
        for (index, before) in pinned {
            let rows = rows_until(&history, index as usize + 1);
            let Some(cut_time_id) = rows.iter().map(|row| row.0).max() else {
                continue;
            };
            let cut_time_id = Some(cut_time_id);
            let cut = RankSnapshotCut {
                at: None,
                as_of_time_id: cut_time_id,
            };
            let again = rank_snapshot_rows(engine, event_id, &top, cut, mode)
                .await
                .unwrap();
            assert_eq!(
                snapshot_map(&again),
                snapshot_map(&before),
                "seed {seed}: cut after flush {index} moved"
            );
        }
        reset_event(engine, event_id, false).await;
    }
    assert!(
        naive_duplicates > 0,
        "the generated histories must contain the duplicate the fix removes"
    );
}

#[tokio::test]
async fn rank_snapshots_never_list_a_player_twice_on_sqlite() {
    let engine = quiet_connect("sqlite::memory:", DatabaseBackend::Sqlite).await;
    run_generated(&engine, 9301, 4).await;
}

/// `HET_TEST_PG_URL=postgres://... cargo test --lib -- --ignored
/// rank_snapshots_never_list_a_player_twice_on_postgres`.
#[tokio::test]
#[ignore = "needs HET_TEST_PG_URL"]
async fn rank_snapshots_never_list_a_player_twice_on_postgres() {
    let Ok(url) = std::env::var("HET_TEST_PG_URL") else {
        eprintln!("HET_TEST_PG_URL not set; skipping");
        return;
    };
    let engine = quiet_connect(&url, DatabaseBackend::Postgres).await;
    run_generated(&engine, 9301, 4).await;
}

fn wl_record(row: &Row) -> PlayerWorldBloomRankingRecordSchema {
    PlayerWorldBloomRankingRecordSchema {
        base: record(row),
        character_id: CHARACTER,
    }
}

#[tokio::test]
async fn world_bloom_snapshot_drops_stale_occupant_and_honours_cut() {
    let engine = quiet_connect("sqlite::memory:", DatabaseBackend::Sqlite).await;
    let event_id = 9401;
    reset_event(&engine, event_id, true).await;
    let anonymizer = UidAnonymizer::enabled("snapshot-test");
    let mode = PublicUserIdMode::Raw;
    let mut state = HashMap::new();
    let mut keys = HashMap::new();
    // t0: 1 -> rank 99, 2 -> rank 100. t1: player 1 drops to 100 but rank
    // 99 (now player 3) is only written at t2, in a later flush.
    for flush in [
        vec![(T0, 1, 500, 99), (T0, 2, 400, 100)],
        vec![(T0 + 1, 1, 500, 100)],
        vec![(T0 + 2, 3, 600, 99)],
    ] {
        let rows: Vec<_> = flush.iter().map(wl_record).collect();
        batch_insert_flush(
            &engine,
            SekaiServerRegion::Jp,
            event_id,
            &anonymizer,
            &[],
            &rows,
            &mut state,
            &mut keys,
        )
        .await
        .unwrap();
    }
    let at = |as_of_time_id| RankSnapshotCut {
        at: None,
        as_of_time_id,
    };
    let view = |rows: Vec<WorldBloomRankingPageRow>| -> Vec<(i64, String)> {
        rows.into_iter()
            .map(|row| (row.rank(), row.user_id().to_owned()))
            .collect()
    };
    let ranks = [99, 100];
    let latest = latest_rank_cut(&engine, event_id, Some(CHARACTER))
        .await
        .unwrap();
    assert_eq!(latest, Some(T0 + 2));
    let rows =
        world_bloom_rank_snapshot_rows(&engine, event_id, CHARACTER, &ranks, at(latest), mode)
            .await
            .unwrap();
    assert_eq!(view(rows), vec![(99, "3".into()), (100, "1".into())]);

    // At the middle cut rank 99 still names player 1, who is at 100 by
    // then: the stale rank is left out rather than listing 1 twice.
    let rows = world_bloom_rank_snapshot_rows(
        &engine,
        event_id,
        CHARACTER,
        &ranks,
        at(Some(T0 + 1)),
        mode,
    )
    .await
    .unwrap();
    assert_eq!(view(rows), vec![(100, "1".into())]);
    assert_eq!(
        user_rank_as_of(&engine, event_id, Some(CHARACTER), "1", at(Some(T0)), mode)
            .await
            .unwrap(),
        Some(99)
    );
    assert_eq!(
        latest_rank_cut(&engine, event_id, Some(CHARACTER + 1))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn flush_writes_main_and_world_bloom_rows_atomically() {
    let engine = quiet_connect("sqlite::memory:", DatabaseBackend::Sqlite).await;
    let event_id = 9402;
    reset_event(&engine, event_id, true).await;
    engine
        .conn()
        .execute_unprepared(&format!(
            "DROP TABLE {}",
            intern(TableKind::WorldBloom, event_id)
        ))
        .await
        .unwrap();
    let anonymizer = UidAnonymizer::enabled("snapshot-test");
    let main = vec![record(&(T0, 1, 500, 1))];
    let wl = vec![wl_record(&(T0, 1, 500, 1))];
    let mut state = HashMap::new();
    let result = batch_insert_flush(
        &engine,
        SekaiServerRegion::Jp,
        event_id,
        &anonymizer,
        &main,
        &wl,
        &mut state,
        &mut HashMap::new(),
    )
    .await;
    assert!(result.is_err(), "the chapter insert must fail");
    assert!(state.is_empty(), "state only advances after a commit");
    assert_eq!(
        latest_rank_cut(&engine, event_id, None).await.unwrap(),
        None,
        "main rows of a failed flush must not land on their own"
    );
}
