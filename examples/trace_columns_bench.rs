//! Size and encode-time comparison of a trace's row array against its
//! columns object (`traceFormat=columns`) on synthetic traces shaped like
//! production ones (64-hex public ids, second-level samples, a few seat
//! changes in a rank trace, rank moves in a player trace).
//!
//! `cargo run --release --example trace_columns_bench [out_dir]`
//! With `out_dir`, every payload is written there for external
//! compressors (`zstd -3`, `gzip -6`, ...).

use std::io::Write;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;
use haruki_event_tracker::model::api::{
    RecordedRankData, RecordedRankingSchema, RecordedWorldBloomRankingSchema,
};
use haruki_event_tracker::model::trace_columns::TraceColumns;
use sha2::{Digest, Sha256};

const T0: i64 = 1_780_000_000;
const ITERATIONS: u32 = 20;

fn public_id(seed: u64) -> String {
    let digest = Sha256::digest(seed.to_le_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A tiny deterministic generator (xorshift), so runs are comparable.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

fn rank_trace(n: usize, seat_changes: usize, character_id: Option<i64>) -> Vec<RecordedRankData> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut change_at: Vec<usize> = (0..seat_changes)
        .map(|_| rng.below(n as u64) as usize)
        .collect();
    change_at.sort_unstable();
    let holders: Vec<String> = (0..=seat_changes as u64)
        .map(|k| public_id(1000 + k % 3))
        .collect();
    let mut holder = 0;
    let mut timestamp = T0;
    let mut score = 12_345_678;
    let mut rows = Vec::with_capacity(n);
    for index in 0..n {
        timestamp += 1 + rng.below(3) as i64 + if rng.below(50) == 0 { 40 } else { 0 };
        score += 500 + rng.below(4_000) as i64;
        while change_at.get(holder).is_some_and(|at| *at <= index) {
            holder += 1;
        }
        rows.push(row(
            timestamp,
            &holders[holder.min(seat_changes)],
            score,
            100,
            character_id,
        ));
    }
    rows
}

fn player_trace(n: usize, character_id: Option<i64>) -> Vec<RecordedRankData> {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let user = public_id(7);
    let mut timestamp = T0;
    let mut score = 12_345_678;
    let mut rank = 100;
    let mut rows = Vec::with_capacity(n);
    for _ in 0..n {
        timestamp += 1 + rng.below(4) as i64;
        score += 500 + rng.below(4_000) as i64;
        if rng.below(8) == 0 {
            rank += rng.below(5) as i64 - 2;
            rank = rank.max(1);
        }
        rows.push(row(timestamp, &user, score, rank, character_id));
    }
    rows
}

fn row(
    timestamp: i64,
    user_id: &str,
    score: i64,
    rank: i64,
    character_id: Option<i64>,
) -> RecordedRankData {
    match character_id {
        Some(_) => RecordedRankData::WorldBloom(RecordedWorldBloomRankingSchema {
            timestamp,
            user_id: user_id.to_owned(),
            score,
            rank,
            character_id,
        }),
        None => RecordedRankData::Normal(RecordedRankingSchema {
            timestamp,
            user_id: user_id.to_owned(),
            score,
            rank,
        }),
    }
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(bytes.len() / 4), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn timed<T>(mut op: impl FnMut() -> T) -> (T, Duration) {
    let mut best = Duration::MAX;
    let mut value = None;
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        let result = op();
        best = best.min(start.elapsed());
        value = Some(result);
    }
    (value.unwrap(), best)
}

fn report(name: &str, rows: &[RecordedRankData], out_dir: Option<&str>) {
    let (rows_json, rows_encode) = timed(|| sonic_rs::to_vec(rows).unwrap());
    let (columns_json, columns_encode) = timed(|| {
        let columns = TraceColumns::from_rows(rows);
        sonic_rs::to_vec(&columns).unwrap()
    });
    let columns: TraceColumns = sonic_rs::from_slice(&columns_json).unwrap();
    assert_eq!(
        sonic_rs::to_vec(&columns.to_rows().unwrap()).unwrap(),
        rows_json
    );
    let (rows_gz, rows_gzip) = timed(|| gzip(&rows_json));
    let (columns_gz, columns_gzip) = timed(|| gzip(&columns_json));
    println!(
        "{name}: {} rows\n  rows    raw {:>9} B  gzip {:>8} B  encode {:>8.3} ms  gzip {:>8.3} ms\n  columns raw {:>9} B  gzip {:>8} B  encode {:>8.3} ms  gzip {:>8.3} ms\n  ratio   raw {:>5.1}%      gzip {:>5.1}%",
        rows.len(),
        rows_json.len(),
        rows_gz.len(),
        rows_encode.as_secs_f64() * 1e3,
        rows_gzip.as_secs_f64() * 1e3,
        columns_json.len(),
        columns_gz.len(),
        columns_encode.as_secs_f64() * 1e3,
        columns_gzip.as_secs_f64() * 1e3,
        columns_json.len() as f64 / rows_json.len() as f64 * 100.0,
        columns_gz.len() as f64 / rows_gz.len() as f64 * 100.0,
    );
    if let Some(dir) = out_dir {
        std::fs::write(format!("{dir}/{name}.rows.json"), &rows_json).unwrap();
        std::fs::write(format!("{dir}/{name}.columns.json"), &columns_json).unwrap();
    }
}

fn main() {
    let out_dir = std::env::args().nth(1);
    if let Some(dir) = out_dir.as_deref() {
        std::fs::create_dir_all(dir).unwrap();
    }
    for (n, seat_changes) in [(6_300, 6), (34_000, 40)] {
        report(
            &format!("rank100-{n}"),
            &rank_trace(n, seat_changes, None),
            out_dir.as_deref(),
        );
        report(
            &format!("player-{n}"),
            &player_trace(n, None),
            out_dir.as_deref(),
        );
        report(
            &format!("wl-rank100-{n}"),
            &rank_trace(n, seat_changes, Some(17)),
            out_dir.as_deref(),
        );
    }
}
