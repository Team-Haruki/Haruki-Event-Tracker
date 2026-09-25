//! Equivalence of the edge-probe queries with the grouped / time-joined
//! forms they replace, on generated events: sequence or timestamp
//! `time_id`s, time gaps, heartbeat-only samples, ranks that never or only
//! briefly appear, tied ranks, orphan ranking rows and empty or inverted
//! windows. `run_equivalence` is backend-agnostic; the SQLite run is part
//! of `cargo test`, the PostgreSQL run is gated (`HET_TEST_PG_URL`).

use std::collections::{BTreeMap, HashMap, HashSet};

use sea_orm::sea_query::{Alias, Query};
use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseBackend, FromQueryResult};

use crate::db::engine::DatabaseEngine;
use crate::db::query::lines::{RankEdge, RankEdgeSpec, grouped_rank_edge_select, rank_edge_select};
use crate::db::query::user::PublicUserIdMode;
use crate::db::query::web::{
    PlayerGrowthRow, RankingPageRow, WebRankingCursor, WebRankingFilter, WebTraceFilter,
    WorldBloomRankingPageRow, earliest_player_rows_select, grouped_latest_rank,
    grouped_latest_world_bloom_rank, latest_rank_window_join, latest_world_bloom_rank_window_join,
    legacy_player_rows_select, search_rank_trace, search_ranking_rows, search_user_trace,
    search_world_bloom_rank_trace, search_world_bloom_ranking_rows, search_world_bloom_user_trace,
};
use crate::db::schema::create_event_tables;
use crate::db::table_name::{TableKind, intern};
use crate::model::api::{RankingLineScoreSchema, RecordedRankData};
use crate::model::enums::SekaiServerRegion;

const T0: i64 = 1_700_000_000;
const TOP_RANKS: i64 = 10;
const BORDER_RANKS: [i64; 2] = [20, 50];
/// Never written: a requested rank with no rows.
const MISSING_RANK: i64 = 7;
const USERS: i64 = 30;
const CHARACTERS: [i64; 2] = [3, 5];

pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    pub(crate) fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    pub(crate) fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdMode {
    /// Current writer: `time_id = timestamp`.
    Timestamp,
    /// Legacy rows: sequence ids, gaps included, order-preserving.
    Sequence,
}

#[derive(Clone, Copy, Debug)]
struct Row {
    time_id: i64,
    user: i64,
    character: i64,
    score: i64,
    rank: i64,
}

struct Fixture {
    event_id: i64,
    /// `time_id -> timestamp`.
    times: BTreeMap<i64, i64>,
    rows: Vec<Row>,
    wl_rows: Vec<Row>,
}

impl Fixture {
    fn first_ts(&self) -> i64 {
        *self.times.values().next().unwrap()
    }

    fn last_ts(&self) -> i64 {
        *self.times.values().next_back().unwrap()
    }
}

fn all_ranks() -> Vec<i64> {
    (1..=TOP_RANKS).chain(BORDER_RANKS).collect()
}

fn generate(event_id: i64, rng: &mut Rng, mode: IdMode) -> Fixture {
    let mut times = BTreeMap::new();
    let mut ts = T0;
    let mut seq = 0;
    let samples = 150;
    for _ in 0..samples {
        ts += rng.range(1, 4);
        seq += rng.range(1, 3);
        let id = match mode {
            IdMode::Timestamp => ts,
            IdMode::Sequence => seq,
        };
        times.insert(id, ts);
    }
    let ids: Vec<i64> = times.keys().copied().collect();
    let mut rows = Vec::new();
    let mut wl_rows = Vec::new();
    for (i, &time_id) in ids.iter().enumerate() {
        // Heartbeat-only sample: a time row without ranking rows.
        if rng.chance(10) {
            continue;
        }
        for character in [0].into_iter().chain(CHARACTERS) {
            let mut used = HashSet::new();
            let mut pick_user = |rng: &mut Rng| loop {
                let user = rng.range(1, USERS);
                if used.insert(user) {
                    break user;
                }
            };
            for rank in all_ranks() {
                if rank == MISSING_RANK || (rank == 50 && i > samples / 3) || !rng.chance(45) {
                    continue;
                }
                let copies = if rng.chance(8) { 2 } else { 1 };
                for _ in 0..copies {
                    let row = Row {
                        time_id,
                        user: pick_user(rng),
                        character,
                        score: rng.range(0, 10_000),
                        rank,
                    };
                    if character == 0 {
                        rows.push(row);
                    } else {
                        wl_rows.push(row);
                    }
                }
            }
        }
    }
    // Orphans: ranking rows whose `time_id` has no time row, inside the
    // id range so they fall within the derived bounds.
    let (lo, hi) = (ids[0], *ids.last().unwrap());
    let mut orphans = 0;
    while orphans < 4 {
        let time_id = rng.range(lo, hi);
        if times.contains_key(&time_id) || rows.iter().any(|r| r.time_id == time_id) {
            continue;
        }
        orphans += 1;
        let rank = rng.range(1, TOP_RANKS);
        rows.push(Row {
            time_id,
            user: rng.range(1, USERS),
            character: 0,
            score: rng.range(0, 10_000),
            rank,
        });
        wl_rows.push(Row {
            time_id,
            user: rng.range(1, USERS),
            character: CHARACTERS[0],
            score: rng.range(0, 10_000),
            rank,
        });
    }
    Fixture {
        event_id,
        times,
        rows,
        wl_rows,
    }
}

async fn exec(engine: &DatabaseEngine, stmt: &sea_orm::sea_query::InsertStatement) {
    engine.conn().execute(stmt).await.unwrap();
}

async fn load(engine: &DatabaseEngine, fx: &Fixture) {
    create_event_tables(engine, SekaiServerRegion::Jp, fx.event_id, true)
        .await
        .unwrap();
    let mut users = Query::insert();
    users
        .into_table(Alias::new(intern(TableKind::EventUsers, fx.event_id)))
        .columns([
            Alias::new("user_id_key"),
            Alias::new("user_id"),
            Alias::new("unique_id"),
            Alias::new("name"),
        ]);
    for key in 1..=USERS {
        users.values_panic([
            key.into(),
            format!("{key}").into(),
            format!("u-{key}").into(),
            format!("user {key}").into(),
        ]);
    }
    exec(engine, &users).await;
    let mut times = Query::insert();
    times
        .into_table(Alias::new(intern(TableKind::TimeId, fx.event_id)))
        .columns([
            Alias::new("time_id"),
            Alias::new("timestamp"),
            Alias::new("status"),
        ]);
    for (&time_id, &ts) in &fx.times {
        times.values_panic([time_id.into(), ts.into(), 0i16.into()]);
    }
    exec(engine, &times).await;
    for chunk in fx.rows.chunks(500) {
        let mut ins = Query::insert();
        ins.into_table(Alias::new(intern(TableKind::Event, fx.event_id)))
            .columns([
                Alias::new("time_id"),
                Alias::new("user_id_key"),
                Alias::new("score"),
                Alias::new("rank"),
            ]);
        for r in chunk {
            ins.values_panic([
                r.time_id.into(),
                r.user.into(),
                r.score.into(),
                r.rank.into(),
            ]);
        }
        exec(engine, &ins).await;
    }
    for chunk in fx.wl_rows.chunks(500) {
        let mut ins = Query::insert();
        ins.into_table(Alias::new(intern(TableKind::WorldBloom, fx.event_id)))
            .columns([
                Alias::new("time_id"),
                Alias::new("user_id_key"),
                Alias::new("character_id"),
                Alias::new("score"),
                Alias::new("rank"),
            ]);
        for r in chunk {
            ins.values_panic([
                r.time_id.into(),
                r.user.into(),
                r.character.into(),
                r.score.into(),
                r.rank.into(),
            ]);
        }
        exec(engine, &ins).await;
    }
}

/// A random timestamp bound: inside the event, just outside it, or open.
fn bound(rng: &mut Rng, fx: &Fixture) -> Option<i64> {
    match rng.below(6) {
        0 => None,
        1 => Some(fx.first_ts() - rng.range(1, 20)),
        2 => Some(fx.last_ts() + rng.range(0, 20)),
        _ => Some(rng.range(fx.first_ts(), fx.last_ts())),
    }
}

fn window(rng: &mut Rng, fx: &Fixture) -> (Option<i64>, Option<i64>) {
    let (a, b) = (bound(rng, fx), bound(rng, fx));
    match (a, b) {
        // Mostly ordered, sometimes inverted (an empty window).
        (Some(x), Some(y)) if x > y && !rng.chance(15) => (Some(y), Some(x)),
        other => other,
    }
}

fn rank_subset(rng: &mut Rng) -> Vec<i64> {
    let mut ranks: Vec<i64> = all_ranks()
        .into_iter()
        .chain([99])
        .filter(|_| rng.chance(60))
        .collect();
    if ranks.is_empty() {
        ranks.push(1);
    }
    ranks
}

async fn fetch<T: FromQueryResult>(
    engine: &DatabaseEngine,
    stmt: &sea_orm::sea_query::SelectStatement,
) -> Vec<T> {
    T::find_by_statement(engine.backend().build(stmt))
        .all(engine.conn())
        .await
        .unwrap_or_else(|err| panic!("{err}: {}", engine.backend().build(stmt)))
}

fn line_tuples(rows: Vec<RankingLineScoreSchema>) -> Vec<(i64, i64, i64)> {
    let mut out: Vec<_> = rows
        .into_iter()
        .map(|r| (r.rank, r.timestamp, r.score))
        .collect();
    out.sort_unstable();
    out
}

fn page_tuples(rows: &[RankingPageRow]) -> Vec<(i64, i64, i64, i64)> {
    rows.iter()
        .map(|r| (r.rank(), r.user_id_key(), r.timestamp(), r.score()))
        .collect()
}

fn wb_page_tuples(rows: &[WorldBloomRankingPageRow]) -> Vec<(i64, i64, i64, i64)> {
    rows.iter()
        .map(|r| (r.rank(), r.user_id_key(), r.timestamp(), r.score()))
        .collect()
}

fn earliest(rows: Vec<PlayerGrowthRow>) -> HashMap<i64, (i64, i64)> {
    let mut out = HashMap::new();
    for row in rows {
        out.entry(row.user_id_key)
            .or_insert((row.timestamp, row.score));
    }
    out
}

fn trace_tuples(rows: Vec<RecordedRankData>) -> Vec<(i64, String, i64, i64)> {
    let mut out: Vec<_> = rows
        .into_iter()
        .map(|row| match row {
            RecordedRankData::Normal(r) => (r.timestamp, r.user_id, r.score, r.rank),
            RecordedRankData::WorldBloom(r) => (r.timestamp, r.user_id, r.score, r.rank),
        })
        .collect();
    out.sort_unstable();
    out
}

/// Brute-force trace: rows matching `keep`, joined to their time row, in
/// the `[start, end]` window and after `cursor`.
fn expected_trace(
    fx: &Fixture,
    rows: &[Row],
    filter: &WebTraceFilter,
    keep: impl Fn(&Row) -> bool,
) -> Vec<(i64, String, i64, i64)> {
    let mut out: Vec<_> = rows
        .iter()
        .filter(|r| keep(r))
        .filter_map(|r| fx.times.get(&r.time_id).map(|ts| (*ts, r)))
        .filter(|(ts, _)| {
            filter.start_time.is_none_or(|s| *ts >= s)
                && filter.end_time.is_none_or(|e| *ts <= e)
                && filter.cursor.is_none_or(|c| *ts > c)
        })
        .map(|(ts, r)| (ts, format!("{}", r.user), r.score, r.rank))
        .collect();
    out.sort_unstable();
    out
}

fn random_window_filter(rng: &mut Rng, fx: &Fixture) -> WebRankingFilter {
    let (start_time, end_time) = if rng.chance(50) {
        window(rng, fx)
    } else {
        (None, None)
    };
    let (rank_min, rank_max, rank_in) = match rng.below(3) {
        0 => {
            let lo = rng.range(1, TOP_RANKS);
            (Some(lo), Some(lo + rng.range(-1, 60)), None)
        }
        1 => (None, None, Some(rank_subset(rng))),
        _ => (Some(1), Some(100), None),
    };
    WebRankingFilter {
        rank_min,
        rank_max,
        rank_in,
        score_min: rng.chance(20).then(|| rng.range(0, 5_000)),
        score_max: rng.chance(20).then(|| rng.range(5_000, 10_000)),
        start_time,
        end_time,
        before: rng
            .chance(15)
            .then(|| rng.range(fx.first_ts(), fx.last_ts())),
        after: rng
            .chance(15)
            .then(|| rng.range(fx.first_ts(), fx.last_ts())),
        timestamp: rng
            .chance(30)
            .then(|| rng.range(fx.first_ts(), fx.last_ts() + 5)),
        cursor: rng.chance(20).then(|| WebRankingCursor {
            timestamp: 0,
            rank: rng.range(1, TOP_RANKS),
            user_id_key: rng.range(0, USERS),
        }),
        limit: if rng.chance(30) {
            rng.range(1, 5) as u64
        } else {
            1000
        },
    }
}

/// Brute-force non-window search: rows in the filters, newest sample
/// first, then `(rank, user_id_key)`.
fn expected_search(fx: &Fixture, rows: &[Row], f: &WebRankingFilter) -> Vec<(i64, i64, i64, i64)> {
    let mut out: Vec<_> = rows
        .iter()
        .filter_map(|r| fx.times.get(&r.time_id).map(|ts| (*ts, r)))
        .filter(|(ts, r)| {
            let ts = *ts;
            f.score_min.is_none_or(|v| r.score >= v)
                && f.score_max.is_none_or(|v| r.score <= v)
                && f.start_time.is_none_or(|v| ts >= v)
                && f.end_time.is_none_or(|v| ts <= v)
                && f.before.is_none_or(|v| ts <= v)
                && f.after.is_none_or(|v| ts >= v)
                && f.timestamp.is_none_or(|v| ts == v)
                && f.cursor.is_none_or(|c| {
                    ts < c.timestamp
                        || (ts == c.timestamp && r.rank > c.rank)
                        || (ts == c.timestamp && r.rank == c.rank && r.user > c.user_id_key)
                })
        })
        .map(|(ts, r)| (r.time_id, ts, r.rank, r.user, r.score))
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
    out.truncate(f.limit as usize);
    out.into_iter()
        .map(|(_, ts, rank, user, score)| (rank, user, ts, score))
        .collect()
}

async fn check_rank_edges(engine: &DatabaseEngine, fx: &Fixture, rng: &mut Rng) {
    let character_id = rng.chance(50).then(|| CHARACTERS[rng.below(2) as usize]);
    let spec = RankEdgeSpec {
        tbl: match character_id {
            Some(_) => intern(TableKind::WorldBloom, fx.event_id),
            None => intern(TableKind::Event, fx.event_id),
        },
        time_tbl: intern(TableKind::TimeId, fx.event_id),
        character_id,
    };
    let ranks = rank_subset(rng);
    let (start, end) = window(rng, fx);
    for (new_edge, old_edge) in [
        (RankEdge::Earliest, RankEdge::Earliest),
        (RankEdge::Latest, RankEdge::Latest),
    ] {
        let label = matches!(new_edge, RankEdge::Latest);
        let new = rank_edge_select(&spec, &ranks, new_edge, start, end);
        let old = grouped_rank_edge_select(&spec, &ranks, old_edge, start, end);
        assert_eq!(
            line_tuples(fetch(engine, &new).await),
            line_tuples(fetch(engine, &old).await),
            "rank edge latest={label} ranks={ranks:?} window={start:?}..{end:?} wb={character_id:?}"
        );
    }
}

async fn check_rank_window(engine: &DatabaseEngine, fx: &Fixture, rng: &mut Rng) {
    let filter = random_window_filter(rng, fx);
    let mode = PublicUserIdMode::Raw;
    let limit = filter.limit as usize;
    let (new, _) = search_ranking_rows(engine, fx.event_id, &filter, mode)
        .await
        .unwrap();
    let old_stmt = latest_rank_window_join(
        fx.event_id,
        &filter,
        mode,
        grouped_latest_rank(fx.event_id, &filter, false),
    );
    let bounded_stmt = latest_rank_window_join(
        fx.event_id,
        &filter,
        mode,
        grouped_latest_rank(fx.event_id, &filter, true),
    );
    let mut old: Vec<RankingPageRow> = fetch(engine, &old_stmt).await;
    let mut bounded: Vec<RankingPageRow> = fetch(engine, &bounded_stmt).await;
    old.truncate(limit);
    bounded.truncate(limit);
    assert_eq!(page_tuples(&new), page_tuples(&old), "window {filter:?}");
    assert_eq!(
        page_tuples(&bounded),
        page_tuples(&old),
        "grouped window {filter:?}"
    );

    let character_id = CHARACTERS[rng.below(2) as usize];
    let (new, _) =
        search_world_bloom_ranking_rows(engine, fx.event_id, character_id, &filter, mode)
            .await
            .unwrap();
    let old_stmt = latest_world_bloom_rank_window_join(
        fx.event_id,
        character_id,
        &filter,
        mode,
        grouped_latest_world_bloom_rank(fx.event_id, character_id, &filter, false),
    );
    let mut old: Vec<WorldBloomRankingPageRow> = fetch(engine, &old_stmt).await;
    old.truncate(limit);
    assert_eq!(
        wb_page_tuples(&new),
        wb_page_tuples(&old),
        "wb window {character_id} {filter:?}"
    );
}

async fn check_plain_search(engine: &DatabaseEngine, fx: &Fixture, rng: &mut Rng) {
    let mut filter = random_window_filter(rng, fx);
    filter.rank_min = None;
    filter.rank_max = None;
    filter.rank_in = None;
    if let Some(cursor) = filter.cursor.as_mut() {
        cursor.timestamp = rng.range(fx.first_ts(), fx.last_ts());
    }
    let (rows, _) = search_ranking_rows(engine, fx.event_id, &filter, PublicUserIdMode::Raw)
        .await
        .unwrap();
    assert_eq!(
        page_tuples(&rows),
        expected_search(fx, &fx.rows, &filter),
        "search {filter:?}"
    );
}

async fn check_player_growths(engine: &DatabaseEngine, fx: &Fixture, rng: &mut Rng) {
    let character_id = rng.chance(50).then(|| CHARACTERS[rng.below(2) as usize]);
    let tbl = match character_id {
        Some(_) => intern(TableKind::WorldBloom, fx.event_id),
        None => intern(TableKind::Event, fx.event_id),
    };
    let time_tbl = intern(TableKind::TimeId, fx.event_id);
    let mut keys: Vec<i64> = (1..=USERS + 2).filter(|_| rng.chance(50)).collect();
    if keys.is_empty() {
        keys.push(1);
    }
    // Duplicate keys, as a tied top page can list the same player twice.
    keys.push(keys[0]);
    let start = bound(rng, fx).unwrap_or(fx.first_ts());
    let end = bound(rng, fx);
    let new = earliest_player_rows_select(tbl, time_tbl, character_id, &keys, start, end);
    let old = legacy_player_rows_select(tbl, time_tbl, character_id, &keys, start, end);
    assert_eq!(
        earliest(fetch(engine, &new).await),
        earliest(fetch(engine, &old).await),
        "player growth keys={keys:?} {start}..{end:?} wb={character_id:?}"
    );
}

async fn check_traces(engine: &DatabaseEngine, fx: &Fixture, rng: &mut Rng) {
    let (start_time, end_time) = window(rng, fx);
    let filter = WebTraceFilter {
        start_time,
        end_time,
        cursor: rng
            .chance(30)
            .then(|| rng.range(fx.first_ts(), fx.last_ts())),
        limit: None,
    };
    let mode = PublicUserIdMode::Raw;
    let rank = all_ranks()[rng.below(all_ranks().len() as u64) as usize];
    let user = rng.range(1, USERS);
    let character_id = CHARACTERS[rng.below(2) as usize];

    let got = search_rank_trace(engine, fx.event_id, rank, &filter, mode)
        .await
        .unwrap();
    assert_eq!(
        trace_tuples(got),
        expected_trace(fx, &fx.rows, &filter, |r| r.rank == rank),
        "rank trace {rank} {filter:?}"
    );
    let got = search_user_trace(engine, fx.event_id, &format!("{user}"), &filter, mode)
        .await
        .unwrap();
    assert_eq!(
        trace_tuples(got),
        expected_trace(fx, &fx.rows, &filter, |r| r.user == user),
        "user trace {user} {filter:?}"
    );
    let got = search_world_bloom_rank_trace(engine, fx.event_id, character_id, rank, &filter, mode)
        .await
        .unwrap();
    assert_eq!(
        trace_tuples(got),
        expected_trace(fx, &fx.wl_rows, &filter, |r| {
            r.rank == rank && r.character == character_id
        }),
        "wb rank trace {character_id}/{rank} {filter:?}"
    );
    let got = search_world_bloom_user_trace(
        engine,
        fx.event_id,
        character_id,
        &format!("{user}"),
        &filter,
        mode,
    )
    .await
    .unwrap();
    assert_eq!(
        trace_tuples(got),
        expected_trace(fx, &fx.wl_rows, &filter, |r| {
            r.user == user && r.character == character_id
        }),
        "wb user trace {character_id}/{user} {filter:?}"
    );
}

async fn drop_event(engine: &DatabaseEngine, event_id: i64) {
    for kind in [
        TableKind::WorldBloom,
        TableKind::Event,
        TableKind::EventUsers,
        TableKind::TimeId,
    ] {
        let sql = format!("DROP TABLE IF EXISTS {}", intern(kind, event_id));
        engine.conn().execute_unprepared(&sql).await.unwrap();
    }
}

/// Generates, loads and cross-checks one event per `(seed, mode)`.
async fn run_equivalence(engine: &DatabaseEngine, first_event_id: i64, seeds: u64, iters: usize) {
    let mut event_id = first_event_id;
    for seed in 0..seeds {
        for mode in [IdMode::Timestamp, IdMode::Sequence] {
            let mut rng = Rng::new(seed * 2 + u64::from(mode == IdMode::Sequence) + 1);
            let fx = generate(event_id, &mut rng, mode);
            drop_event(engine, event_id).await;
            load(engine, &fx).await;
            for _ in 0..iters {
                check_rank_edges(engine, &fx, &mut rng).await;
                check_rank_window(engine, &fx, &mut rng).await;
                check_plain_search(engine, &fx, &mut rng).await;
                check_player_growths(engine, &fx, &mut rng).await;
                check_traces(engine, &fx, &mut rng).await;
            }
            drop_event(engine, event_id).await;
            event_id += 1;
        }
    }
}

/// Statement logging off: thousands of statements would otherwise go
/// through whatever global subscriber another test installed (the logger
/// test asserts that its bounded file sink drops nothing).
pub(super) async fn quiet_connect(url: &str, backend: DatabaseBackend) -> DatabaseEngine {
    let mut opts = ConnectOptions::new(url.to_owned());
    opts.sqlx_logging(false);
    if backend == DatabaseBackend::Sqlite {
        opts.max_connections(1);
    }
    let conn = Database::connect(opts).await.unwrap();
    DatabaseEngine::from_connection(conn, backend)
}

#[tokio::test]
async fn edge_queries_match_grouped_forms_on_sqlite() {
    let engine = quiet_connect("sqlite::memory:", DatabaseBackend::Sqlite).await;
    run_equivalence(&engine, 9001, 6, 40).await;
}

/// `HET_TEST_PG_URL=postgres://... cargo test --lib -- --ignored
/// edge_queries_match_grouped_forms_on_postgres` (see
/// `tools/bench-rank-edges.sh`, which provides a throwaway database).
#[tokio::test]
#[ignore = "needs HET_TEST_PG_URL"]
async fn edge_queries_match_grouped_forms_on_postgres() {
    let Ok(url) = std::env::var("HET_TEST_PG_URL") else {
        eprintln!("HET_TEST_PG_URL not set; skipping");
        return;
    };
    let engine = quiet_connect(&url, DatabaseBackend::Postgres).await;
    run_equivalence(&engine, 9001, 6, 40).await;
}
