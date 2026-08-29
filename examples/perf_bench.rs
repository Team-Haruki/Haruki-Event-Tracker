//! Before/after benchmark harness for the perf/query-and-cache branch.
//!
//! Run the identical workload on `main` and on the perf branch:
//! `cargo run --release --example perf_bench`
//! (on `main`, delete the `rank_in: None,` line — the field doesn't exist there)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;

use haruki_event_tracker::db::engine::DatabaseEngine;
use haruki_event_tracker::db::query::batch::batch_insert_event_rankings;
use haruki_event_tracker::db::query::growth::fetch_ranking_score_growths;
use haruki_event_tracker::db::query::lines::fetch_ranking_lines;
use haruki_event_tracker::db::query::user::PublicUserIdMode;
use haruki_event_tracker::db::query::web::{
    WebRankingFilter, WebTraceFilter, search_rankings, search_user_trace,
};
use haruki_event_tracker::db::schema::create_event_tables;
use haruki_event_tracker::model::db_config::DbConfig;
use haruki_event_tracker::model::enums::{SEKAI_EVENT_RANKING_LINES_NORMAL, SekaiServerRegion};
use haruki_event_tracker::model::sekai::{UserCard, UserProfileHonor};
use haruki_event_tracker::model::tracker::{PlayerEventRankingRecordSchema, PlayerProfileSchema};
use haruki_event_tracker::privacy::UidAnonymizer;

const EVENT_ID: i64 = 424242;
const WRITE_TICKS: usize = 300;
const TOP_USERS: usize = 150;
const BORDER_RANKS: [i64; 12] = [
    200, 300, 500, 1000, 2000, 3000, 5000, 10000, 20000, 30000, 50000, 100000,
];

fn profile(seed: usize) -> PlayerProfileSchema {
    PlayerProfileSchema {
        card: Some(UserCard {
            card_id: Some(1000 + seed as i64 % 400),
            level: Some(60),
            master_rank: Some((seed % 5) as i64),
            special_training_status: Some("done".into()),
            default_image: Some("special_training".into()),
        }),
        profile_word: Some(format!("bench profile word {seed} ・ハルキ・トラッカー")),
        profile_honors: (0..24)
            .map(|i| UserProfileHonor {
                seq: Some(i),
                profile_honor_type: Some("normal".into()),
                honor_id: Some(90 + i + (seed as i64 % 7)),
                honor_level: Some(1 + (i % 9)),
                bonds_honor_view_type: Some("none".into()),
                bonds_honor_word_id: Some(0),
            })
            .collect(),
        honor_missions: (0..8)
            .map(|i| {
                serde_json::from_str(&format!(
                    r#"{{"honorMissionType":"character_{i}","progress":{}}}"#,
                    seed % 1000
                ))
                .unwrap()
            })
            .collect(),
        player_frames: Vec::new(),
    }
}

fn tick_records(tick: usize) -> Vec<PlayerEventRankingRecordSchema> {
    let ts = 1_750_000_000 + (tick as i64) * 60;
    let mut records = Vec::with_capacity(100 + BORDER_RANKS.len());
    for rank in 1..=100i64 {
        // Rotate which user sits at which rank so user rows churn like a
        // real event; every 20th tick their profile JSON changes too.
        let user = (rank as usize + tick / 3) % TOP_USERS;
        records.push(PlayerEventRankingRecordSchema {
            timestamp: ts,
            user_id: format!("{}", 10_000_000 + user),
            name: if tick.is_multiple_of(10) && rank % 7 == 0 {
                format!("Player{user}~{tick}")
            } else {
                format!("Player{user}")
            },
            score: (WRITE_TICKS + tick) as i64 * 10_000 - rank * 3_000 + (tick as i64 % 17) * 31,
            rank,
            cheerful_team_id: None,
            profile: profile(user + (tick / 20) * 1000),
        });
    }
    for (i, rank) in BORDER_RANKS.iter().enumerate() {
        let user = 20_000_000 + i;
        records.push(PlayerEventRankingRecordSchema {
            timestamp: ts,
            user_id: format!("{user}"),
            name: format!("Border{i}"),
            score: (WRITE_TICKS + tick) as i64 * 1_000 - rank * 5 + tick as i64 % 13,
            rank: *rank,
            cheerful_team_id: None,
            profile: profile(i),
        });
    }
    records
}

async fn time_iters<F, Fut, T>(label: &str, iters: usize, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    // warmup
    f().await;
    let start = Instant::now();
    for _ in 0..iters {
        f().await;
    }
    let total = start.elapsed();
    println!(
        "{label:<38} {:>9.3} ms/op  ({iters} iters)",
        total.as_secs_f64() * 1000.0 / iters as f64
    );
}

fn ranking_filter(limit: u64, rank_max: Option<i64>) -> WebRankingFilter {
    WebRankingFilter {
        rank_min: rank_max.map(|_| 1),
        rank_max,
        rank_in: None,
        score_min: None,
        score_max: None,
        start_time: None,
        end_time: None,
        before: None,
        after: None,
        timestamp: None,
        cursor: None,
        limit,
    }
}

// ---------------------------------------------------------------------------
// Self-contained L1 cache strategy comparison (old vs new), branch-independent
// ---------------------------------------------------------------------------

const L1_CAP: usize = 2048;
const L1_KEYS: usize = 4096;
const L1_TASKS: usize = 8;
const L1_OPS_PER_TASK: usize = 200_000;

async fn bench_l1_old(payload: Bytes) -> (f64, u64) {
    let map = Arc::new(tokio::sync::Mutex::new(HashMap::<String, Bytes>::new()));
    let hits = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let start = Instant::now();
    let mut handles = Vec::new();
    for t in 0..L1_TASKS {
        let map = map.clone();
        let hits = hits.clone();
        let payload = payload.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..L1_OPS_PER_TASK {
                let key = format!("k{}", (i * 31 + t * 7) % L1_KEYS);
                if i % 10 == 0 {
                    let mut m = map.lock().await;
                    if m.len() >= L1_CAP {
                        m.clear();
                    }
                    m.insert(key, payload.clone());
                } else if map.lock().await.get(&key).is_some() {
                    hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let ops = (L1_TASKS * L1_OPS_PER_TASK) as f64;
    (
        ops / start.elapsed().as_secs_f64(),
        hits.load(std::sync::atomic::Ordering::Relaxed),
    )
}

async fn bench_l1_new(payload: Bytes) -> (f64, u64) {
    let map = Arc::new(std::sync::Mutex::new(HashMap::<String, Bytes>::new()));
    let hits = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let start = Instant::now();
    let mut handles = Vec::new();
    for t in 0..L1_TASKS {
        let map = map.clone();
        let hits = hits.clone();
        let payload = payload.clone();
        handles.push(tokio::spawn(run_l1_new_task(map, hits, payload, t)));
    }
    for h in handles {
        h.await.unwrap();
    }
    let ops = (L1_TASKS * L1_OPS_PER_TASK) as f64;
    (
        ops / start.elapsed().as_secs_f64(),
        hits.load(std::sync::atomic::Ordering::Relaxed),
    )
}

async fn run_l1_new_task(
    map: Arc<std::sync::Mutex<HashMap<String, Bytes>>>,
    hits: Arc<std::sync::atomic::AtomicU64>,
    payload: Bytes,
    task: usize,
) {
    for i in 0..L1_OPS_PER_TASK {
        let key = format!("k{}", (i * 31 + task * 7) % L1_KEYS);
        if i % 10 == 0 {
            insert_l1_value(&map, key, &payload);
        } else if map.lock().unwrap().get(&key).is_some() {
            hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

fn insert_l1_value(map: &std::sync::Mutex<HashMap<String, Bytes>>, key: String, payload: &Bytes) {
    let mut map = map.lock().unwrap();
    if map.len() >= L1_CAP {
        let excess = map.len() - L1_CAP * 3 / 4;
        let doomed: Vec<String> = map.keys().take(excess).cloned().collect();
        for key in doomed {
            map.remove(&key);
        }
    }
    map.insert(key, payload.clone());
}

#[tokio::main]
async fn main() {
    let db_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/haruki-perf-bench.sqlite".to_string());
    let _ = std::fs::remove_file(&db_path);
    let cfg = DbConfig {
        dialect: "sqlite".into(),
        dsn: format!("sqlite://{db_path}?mode=rwc"),
        ..Default::default()
    };
    let engine = DatabaseEngine::connect(&cfg).await.expect("connect");
    create_event_tables(&engine, SekaiServerRegion::Jp, EVENT_ID, false)
        .await
        .expect("schema");
    let anonymizer = UidAnonymizer::disabled();

    println!("== write path: {WRITE_TICKS} tracker ticks, 112 records each ==");
    let start = Instant::now();
    for tick in 0..WRITE_TICKS {
        let records = tick_records(tick);
        batch_insert_event_rankings(
            &engine,
            SekaiServerRegion::Jp,
            EVENT_ID,
            &anonymizer,
            &records,
        )
        .await
        .expect("tick insert");
    }
    let total = start.elapsed();
    println!(
        "{:<38} {:>9.3} ms/tick ({WRITE_TICKS} ticks, total {:.2}s)",
        "batch_insert_event_rankings",
        total.as_secs_f64() * 1000.0 / WRITE_TICKS as f64,
        total.as_secs_f64()
    );

    println!("== read path (DB now holds {WRITE_TICKS} ticks x 112 rows) ==");
    let ranks = SEKAI_EVENT_RANKING_LINES_NORMAL;
    time_iters("fetch_ranking_lines (25 ranks)", 20, || async {
        fetch_ranking_lines(&engine, EVENT_ID, ranks, None)
            .await
            .expect("lines")
    })
    .await;
    time_iters("fetch_ranking_score_growths (full)", 20, || async {
        fetch_ranking_score_growths(&engine, EVENT_ID, ranks, 0, None)
            .await
            .expect("growths")
    })
    .await;
    let trace_filter = WebTraceFilter {
        start_time: None,
        end_time: None,
        cursor: None,
        limit: None,
    };
    time_iters("search_user_trace (full history)", 50, || async {
        search_user_trace(
            &engine,
            EVENT_ID,
            "10000005",
            &trace_filter,
            PublicUserIdMode::Raw,
        )
        .await
        .expect("trace")
    })
    .await;
    let window = ranking_filter(100, Some(100));
    time_iters("search_rankings (rank window 1..=100)", 50, || async {
        search_rankings(&engine, EVENT_ID, &window, PublicUserIdMode::Raw)
            .await
            .expect("window")
    })
    .await;
    let page = ranking_filter(100, None);
    time_iters("search_rankings (plain page, control)", 50, || async {
        search_rankings(&engine, EVENT_ID, &page, PublicUserIdMode::Raw)
            .await
            .expect("page")
    })
    .await;

    println!("== L1 cache strategy (in-process, branch-independent) ==");
    let payload = Bytes::from(vec![0u8; 1024]);
    let (ops, hits) = bench_l1_old(payload.clone()).await;
    println!(
        "{:<38} {:>9.0} kops/s  (hits {hits})",
        "old: tokio Mutex + clear-on-full",
        ops / 1000.0
    );
    let (ops, hits) = bench_l1_new(payload).await;
    println!(
        "{:<38} {:>9.0} kops/s  (hits {hits})",
        "new: std Mutex + partial eviction",
        ops / 1000.0
    );

    let _ = std::fs::remove_file(&db_path);
}
