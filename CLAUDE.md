# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Haruki Event Tracker is a Rust service that periodically scrapes ranking data from the Haruki Sekai API for the *Project Sekai* (プロジェクトセカイ) mobile game, persists it to a per-server SQL database, and exposes query endpoints (latest rank, trace history, ranking lines, score-growth deltas, heartbeat status) for downstream clients such as HarukiBot.

The repo was rewritten from Go on `rewrite/rust`; `REWRITE_PLAN.md` carries the per-phase decisions and notes any Go behaviour that was intentionally not ported (dead code paths, unused state maps).

## Build & Run

- MSRV: Rust 1.85 (edition 2024).
- Build: `cargo build --release --bin haruki-event-tracker`. Release profile already enables `lto = "thin"`, `codegen-units = 1`, `strip = true`, `opt-level = 3`.
- Run: needs `haruki-tracker-configs.yaml` in the working directory (copy from `haruki-tracker-configs.example.yaml`); `config::load_from_file` returns `ConfigError::{Read, Parse}` and `main` exits non-zero when missing/unparseable. Redis must be reachable before startup. Start with `./target/release/haruki-event-tracker` (or `cargo run --release`).
- Tests: `cargo test --lib` runs the pure-function unit tests (diff/parser/state/db helpers). There is no integration test suite — DB and HTTP behaviour is validated against staging during the cutover.
- Lint: `cargo clippy --all-targets -- -D warnings`. Keep clippy clean before committing — new warnings are treated as build failures.
- Docker: `docker build --build-arg VERSION=<ver> -t haruki-event-tracker .` (multi-stage `rust:1.85-slim-bookworm` builder → `debian:bookworm-slim` runtime). The image expects the config file mounted into `/app`.
- Tagged releases: pushing `v*` tags triggers `.github/workflows/release.yml` (per-target builds: linux-amd64, linux-arm64, macos-arm64, windows-x64) and `.github/workflows/docker.yml` (GHCR push).

## Architecture

The process wires four long-lived subsystems together in `main.rs` → `app::build`:

1. **HTTP layer** (`src/api/`): `axum` 0.8 + `tower-http` (compression, catch-panic) + `axum-server` for unified HTTP / HTTPS via `Handle::graceful_shutdown(10s)`. JSON in/out goes through `api::json::Json<T>` (sonic-rs `IntoResponse`). All routes are GET, registered under `/event/{server}/{event_id}/...` by `api::router::build_router`. `api::extract::resolve_engine` parses `:server` against `AppState`'s per-server `Arc<DatabaseEngine>` map; an unknown server returns 400 via `api::error::ApiError::InvalidServer`.
2. **Per-server database engines** (`src/db/`): one `DatabaseEngine` per enabled `cfg.servers` entry. Backed by `sea-orm` 1.1 with MySQL / PostgreSQL / SQLite drivers, dialect chosen from `DbConfig.dialect`. **Tables are created dynamically per `(server, event_id)`** — `db::schema::create_event_tables` runs the schema bootstrap, and `db::table_name::intern(TableKind, event_id)` returns the `&'static str` name used in `sea-query` aliases. When adding queries, route through `intern` rather than hardcoding names.
3. **Tracker daemons** (`src/tracker/`): one `HarukiEventTracker` per server with `tracker.enabled: true`, scheduled by `tokio_cron_scheduler` (cron expression from config). The `use_second_level_cron: false` (5-field) form is auto-padded with a leading `"0 "` to match the crate's required 6-field schedule. Each tick:
   - `EventDataParser::get_current_event_status` reads `events.json` / `worldBlooms.json` from `master_data_dir` and produces an `EventStatus` for the current wallclock.
   - `HarukiEventTracker::track_ranking_data` reinitialises the inner `EventTrackerBase` when the event id advances, short-circuits if the event is `aggregating` / `ended`, then calls `record_ranking_data`.
   - `EventTrackerBase::handle_ranking_data` calls `HarukiSekaiAPIClient::get_top100` + `get_border`, hashes the border response (SHA-256), and uses `tracker::cache::detect_cache` (Redis hex-encoded match) to skip the merge step when nothing changed. Hex output uses `format!("{:02x}")` to stay byte-compatible with Go's `fmt.Sprintf("%x", hash)` for the cutover.
   - Diffing is **rank-based**: `tracker::diff::diff_rank_based` compares each rank's `(user_id, score)` against `prev_rank_state` and only persists rows that moved. State is mirrored to Redis under `haruki:tracker:<server>:<event>:{rank_state,ended}` so daemons can resume mid-event from state previously written by the Go daemon. (Go also wrote a `user_state` hash but never read it back; that key is intentionally not ported.)
   - Writes go through `db::query::batch::batch_insert_event_rankings` / `batch_insert_world_bloom_rankings`; on API failure or no-change ticks a heartbeat row is still written via `db::query::heartbeat::write_heartbeat` so `/status` reports freshness.
4. **Bootstrap & shutdown** (`src/app.rs`, `src/shutdown.rs`): `app::build` returns an `AppContext { state, dbs, trackers, scheduler }`. `shutdown::signal()` resolves on SIGINT/SIGTERM (Ctrl+C on Windows); `shutdown::run` stops the scheduler, drops the trackers (which closes the shared Redis `ConnectionManager` handle), and `Arc::try_unwrap` + closes each `DatabaseEngine`.

### World Bloom specifics

World Bloom events have per-character chapters tracked in parallel. `EventTrackerBase` keeps `world_bloom_statuses` and `is_world_bloom_chapter_ended` maps; `HarukiEventTracker::handle_world_bloom` iterates *all* chapters each tick (overlap periods are intentional), and `handle_world_bloom_chapter` skips chapters that are `not_started`, `aggregating`, or already finalised. World Bloom rows are persisted via the separate `wl_<event_id>` table built by `intern(TableKind::WorldBloom, _)`.

### Models package

`src/model/` holds *all* shared types — API request/response schemas (`api.rs`), DB config (`db_config.rs`), domain enums (`enums.rs` — `SekaiServerRegion`, `SekaiEventType`, `SekaiEventStatus`, the `SEKAI_EVENT_RANKING_LINES_NORMAL` / `_WORLD_BLOOM` constants used by `/ranking-lines`), event master data structs (`event.rs`), upstream Sekai API DTOs (`sekai.rs`), and tracker state structs (`tracker.rs`). `db` and `tracker` both depend on `model`; `model` depends on nothing internal — keep it that way to avoid cycles.

## Conventions

- **No `mod.rs`**: every module lives in `foo.rs` with optional siblings under `foo/`. `src/lib.rs` declares the top-level modules.
- **Redis key compat**: keys under `haruki:tracker:<server>:<event>:<suffix>` are byte-compatible with the Go version. Don't change suffixes, JSON field names, or hex casing without coordinating a hard cutover.
- **PlayerState/RankState** use serde rename to single-letter keys (`s`/`r`/`u`) for the same Go wire-compat reason.
- **sonic-rs everywhere**: `sonic_rs::{from_str, from_slice, to_vec, to_string}`. `api::json::Json<T>` wraps it for handlers.
- Server identifiers in routes, configs, table names, Redis keys, and span fields are always the lowercase `model::enums::SekaiServerRegion` strings (`jp`/`en`/`tw`/`kr`/`cn`).
- **Dynamic table inserts** must go through `sea-query` (`Query::insert_into(Alias::new(intern(...)))`); SeaORM `ActiveModel` API can't be used because the Entity types carry a non-unit `table_name` field.
- **Comments are sparse** — only when the *why* is non-obvious (cross-language wire compat, lifetime workarounds, Go-version dead code that's intentionally skipped). Don't add narrating comments.
- **TLS**: rustls 0.23 panics in `ServerConfig::builder()` when both `ring` and `aws_lc_rs` providers are reachable in the dep graph (they are, transitively). `main` calls `aws_lc_rs::default_provider().install_default()` once on the SSL branch — keep that line.
- Logging is `tracing` with the `GoStyleFormat` formatter (`[YYYY-MM-DD HH:MM:SS.mmm][LEVEL][target] message`) so log lines stay shape-compatible with the Go build during gradual rollout.
