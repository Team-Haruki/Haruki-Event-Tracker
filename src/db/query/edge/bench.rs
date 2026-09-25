//! Old-vs-new timings of the overview's hot queries on a synthetic
//! end-of-event CN table (~5.8M rows: top 100 sampled every second for
//! 60 h, ~27% of ranks changing per sample, plus 19 border ranks every
//! 30 s). Gated: run through `tools/bench-rank-edges.sh`, which provides a
//! throwaway `postgres:17` and sets `HET_BENCH_PG_URL`.

use std::time::{Duration, Instant};

use sea_orm::sea_query::SelectStatement;
use sea_orm::{ConnectionTrait, Database, DatabaseBackend, FromQueryResult, Statement};

use crate::db::engine::DatabaseEngine;
use crate::db::query::lines::{RankEdge, RankEdgeSpec, grouped_rank_edge_select, rank_edge_select};
use crate::db::query::user::PublicUserIdMode;
use crate::db::query::web::{
    PlayerGrowthRow, RankingPageRow, WebRankingFilter, earliest_player_rows_select,
    grouped_latest_rank, latest_rank_window_join, legacy_player_rows_select,
};
use crate::db::schema::{create_event_tables, create_query_indexes};
use crate::db::table_name::{TableKind, intern};
use crate::model::api::RankingLineScoreSchema;
use crate::model::enums::{SEKAI_EVENT_RANKING_LINES_NORMAL, SekaiServerRegion};

const EVENT_ID: i64 = 180;
const T0: i64 = 1_758_000_000;
const SECONDS: i64 = 60 * 3600;
const INTERVAL: i64 = 3600;
const RUNS: usize = 7;

async fn sql(engine: &DatabaseEngine, sql: &str) {
    engine
        .conn()
        .execute_unprepared(sql)
        .await
        .unwrap_or_else(|err| panic!("{err}: {sql}"));
}

async fn seed(engine: &DatabaseEngine) {
    let (time_tbl, users_tbl, event_tbl) = (
        intern(TableKind::TimeId, EVENT_ID),
        intern(TableKind::EventUsers, EVENT_ID),
        intern(TableKind::Event, EVENT_ID),
    );
    for tbl in [event_tbl, users_tbl, time_tbl] {
        sql(engine, &format!("DROP TABLE IF EXISTS {tbl}")).await;
    }
    create_event_tables(engine, SekaiServerRegion::Cn, EVENT_ID, false)
        .await
        .unwrap();
    for suffix in ["rank_time", "user_time", "time_rank", "time_score"] {
        sql(
            engine,
            &format!("DROP INDEX IF EXISTS idx_{EVENT_ID}_{suffix}"),
        )
        .await;
    }
    let last = SECONDS - 1;
    sql(
        engine,
        &format!(
            "INSERT INTO {time_tbl} (time_id, timestamp, status) \
             SELECT {T0} + s, {T0} + s, 0 FROM generate_series(0, {last}) s"
        ),
    )
    .await;
    sql(
        engine,
        &format!(
            "INSERT INTO {users_tbl} (user_id_key, user_id, unique_id, name, profile_word, \
             profile_honors_json, player_frames_json) \
             SELECT k, k::text, md5(k::text) || md5('x' || k::text), 'player ' || k, 'hello', \
             '[{{\"seq\":1,\"profileHonorType\":\"normal\",\"honorId\":95,\"honorLevel\":1}}]', \
             '[]' FROM generate_series(1, 400) k"
        ),
    )
    .await;
    sql(engine, "SELECT setseed(0.18)").await;
    sql(
        engine,
        &format!(
            "INSERT INTO {event_tbl} (time_id, user_id_key, score, rank) \
             SELECT {T0} + s, ((r + s / 600) % 300) + 1, 30000000 - r * 10000 + s * 50, r \
             FROM generate_series(0, {last}) s CROSS JOIN generate_series(1, 100) r \
             WHERE random() < 0.27"
        ),
    )
    .await;
    let borders = SEKAI_EVENT_RANKING_LINES_NORMAL
        .iter()
        .filter(|rank| **rank > 100)
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    sql(
        engine,
        &format!(
            "INSERT INTO {event_tbl} (time_id, user_id_key, score, rank) \
             SELECT {T0} + s, 300 + b.i, 20000000 / b.i + s * 5, b.rank \
             FROM generate_series(0, {last}, 30) s \
             CROSS JOIN unnest(ARRAY[{borders}]::bigint[]) WITH ORDINALITY AS b(rank, i)"
        ),
    )
    .await;
    create_query_indexes(engine, EVENT_ID, false).await.unwrap();
    sql(engine, &format!("VACUUM ANALYZE {time_tbl}")).await;
    sql(engine, &format!("VACUUM ANALYZE {users_tbl}")).await;
    sql(engine, &format!("VACUUM ANALYZE {event_tbl}")).await;
}

#[derive(FromQueryResult)]
struct Count {
    n: i64,
}

async fn time<T: FromQueryResult>(
    engine: &DatabaseEngine,
    stmt: &SelectStatement,
) -> (Duration, Vec<T>) {
    let built = engine.backend().build(stmt);
    let mut samples = Vec::with_capacity(RUNS);
    let mut rows = Vec::new();
    // One warm-up run, then the median of `RUNS`.
    for i in 0..=RUNS {
        let started = Instant::now();
        rows = T::find_by_statement(built.clone())
            .all(engine.conn())
            .await
            .unwrap();
        if i > 0 {
            samples.push(started.elapsed());
        }
    }
    samples.sort_unstable();
    (samples[RUNS / 2], rows)
}

async fn explain(engine: &DatabaseEngine, label: &str, stmt: &SelectStatement) {
    if std::env::var("HET_BENCH_EXPLAIN").is_err() {
        return;
    }
    let built = engine.backend().build(stmt);
    let explain = Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        format!("EXPLAIN (ANALYZE, BUFFERS, COSTS OFF) {}", built.sql),
        built.values.map(|v| v.0).unwrap_or_default(),
    );
    let rows = engine.conn().query_all_raw(explain).await.unwrap();
    println!("---- EXPLAIN {label}");
    for row in rows {
        let line: String = row.try_get_by_index(0).unwrap();
        println!("{line}");
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

struct Report(Vec<(String, f64, f64)>);

impl Report {
    fn add(&mut self, label: &str, old: Duration, new: Duration) {
        println!(
            "{label:<44} old {:>9.2} ms   new {:>7.2} ms   x{:.0}",
            ms(old),
            ms(new),
            ms(old) / ms(new).max(0.001)
        );
        self.0.push((label.to_owned(), ms(old), ms(new)));
    }
}

fn sorted_lines(rows: Vec<RankingLineScoreSchema>) -> Vec<(i64, i64, i64)> {
    let mut out: Vec<_> = rows
        .into_iter()
        .map(|r| (r.rank, r.timestamp, r.score))
        .collect();
    out.sort_unstable();
    out
}

#[tokio::test]
#[ignore = "needs HET_BENCH_PG_URL (tools/bench-rank-edges.sh)"]
async fn bench_overview_queries_on_postgres() {
    let Ok(url) = std::env::var("HET_BENCH_PG_URL") else {
        eprintln!("HET_BENCH_PG_URL not set; skipping");
        return;
    };
    let conn = Database::connect(url).await.unwrap();
    let engine = DatabaseEngine::from_connection(conn, DatabaseBackend::Postgres);
    if std::env::var("HET_BENCH_RESEED").is_ok() || !seeded(&engine).await {
        let started = Instant::now();
        seed(&engine).await;
        println!("seeded in {:.1}s", started.elapsed().as_secs_f64());
    }
    let rows = Count::find_by_statement(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT COUNT(*) AS n FROM {}",
            intern(TableKind::Event, EVENT_ID)
        ),
    ))
    .one(engine.conn())
    .await
    .unwrap()
    .unwrap()
    .n;
    println!("{} rows: {rows}", intern(TableKind::Event, EVENT_ID));

    let end = T0 + SECONDS - 1;
    let start = end - INTERVAL;
    let spec = RankEdgeSpec {
        tbl: intern(TableKind::Event, EVENT_ID),
        time_tbl: intern(TableKind::TimeId, EVENT_ID),
        character_id: None,
    };
    let growth_ranks: Vec<i64> = (1..=100)
        .chain(
            SEKAI_EVENT_RANKING_LINES_NORMAL
                .iter()
                .copied()
                .filter(|r| *r > 100),
        )
        .collect();
    let border_ranks: Vec<i64> = SEKAI_EVENT_RANKING_LINES_NORMAL
        .iter()
        .copied()
        .filter(|r| *r > 100)
        .collect();
    let mut report = Report(Vec::new());

    for (label, edge, old_edge) in [
        (
            "A  rank edge MAX, 119 ranks, 1 h window",
            RankEdge::Latest,
            RankEdge::Latest,
        ),
        (
            "A  rank edge MIN, 119 ranks, 1 h window",
            RankEdge::Earliest,
            RankEdge::Earliest,
        ),
    ] {
        let new = rank_edge_select(&spec, &growth_ranks, edge, Some(start), Some(end));
        let old = grouped_rank_edge_select(&spec, &growth_ranks, old_edge, Some(start), Some(end));
        let (t_old, r_old) = time::<RankingLineScoreSchema>(&engine, &old).await;
        let (t_new, r_new) = time::<RankingLineScoreSchema>(&engine, &new).await;
        assert_eq!(sorted_lines(r_old), sorted_lines(r_new), "{label}");
        report.add(label, t_old, t_new);
        explain(&engine, &format!("{label} (old)"), &old).await;
        explain(&engine, &format!("{label} (new)"), &new).await;
    }

    let new = rank_edge_select(&spec, &border_ranks, RankEdge::Latest, None, None);
    let old = grouped_rank_edge_select(&spec, &border_ranks, RankEdge::Latest, None, None);
    let (t_old, r_old) = time::<RankingLineScoreSchema>(&engine, &old).await;
    let (t_new, r_new) = time::<RankingLineScoreSchema>(&engine, &new).await;
    assert_eq!(sorted_lines(r_old), sorted_lines(r_new));
    report.add("   border lines, 19 ranks, latest", t_old, t_new);

    let mode = PublicUserIdMode::Unique;
    let top = WebRankingFilter {
        rank_min: Some(1),
        rank_max: Some(100),
        rank_in: None,
        score_min: None,
        score_max: None,
        start_time: None,
        end_time: None,
        before: None,
        after: None,
        timestamp: None,
        cursor: None,
        limit: 100,
    };
    let old = latest_rank_window_join(
        EVENT_ID,
        &top,
        mode,
        grouped_latest_rank(EVENT_ID, &top, false),
    );
    let new = crate::db::query::web::latest_rank_window_select(EVENT_ID, &top, mode);
    let (t_old, r_old) = time::<RankingPageRow>(&engine, &old).await;
    let (t_new, r_new) = time::<RankingPageRow>(&engine, &new).await;
    let key = |rows: &[RankingPageRow]| {
        rows.iter()
            .map(|r| (r.rank(), r.user_id_key(), r.timestamp(), r.score()))
            .collect::<Vec<_>>()
    };
    assert_eq!(key(&r_old), key(&r_new));
    report.add("   top-100 latest window", t_old, t_new);
    explain(&engine, "top-100 window (new)", &new).await;

    let user_keys: Vec<i64> = r_new.iter().map(RankingPageRow::user_id_key).collect();
    let tbl = intern(TableKind::Event, EVENT_ID);
    let time_tbl = intern(TableKind::TimeId, EVENT_ID);
    let old = legacy_player_rows_select(tbl, time_tbl, None, &user_keys, start, Some(end));
    let new = earliest_player_rows_select(tbl, time_tbl, None, &user_keys, start, Some(end));
    let (t_old, r_old) = time::<PlayerGrowthRow>(&engine, &old).await;
    let (t_new, r_new) = time::<PlayerGrowthRow>(&engine, &new).await;
    let earliest = |rows: Vec<PlayerGrowthRow>| {
        let mut out = std::collections::BTreeMap::new();
        for row in rows {
            out.entry(row.user_id_key)
                .or_insert((row.timestamp, row.score));
        }
        out
    };
    println!(
        "   (B old returns {} rows, new {})",
        r_old.len(),
        r_new.len()
    );
    assert_eq!(earliest(r_old), earliest(r_new));
    report.add("B  top-100 player growth, 1 h window", t_old, t_new);
    explain(&engine, "B (old)", &old).await;
    explain(&engine, "B (new)", &new).await;

    let (old_total, new_total) = report
        .0
        .iter()
        .fold((0.0, 0.0), |(o, n), (_, a, b)| (o + a, n + b));
    println!("overview query total (sequential sum): old {old_total:.1} ms, new {new_total:.1} ms");
}

async fn seeded(engine: &DatabaseEngine) -> bool {
    let probe = Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT COUNT(*) AS n FROM pg_indexes WHERE indexname = 'idx_{EVENT_ID}_time_score'"
        ),
    );
    Count::find_by_statement(probe)
        .one(engine.conn())
        .await
        .ok()
        .flatten()
        .is_some_and(|c| c.n > 0)
}
