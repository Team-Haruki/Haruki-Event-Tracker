# AGENTS.md

Cross-agent guidance for Haruki Event Tracker. This file is the single source of truth for any AI coding agent (Codex, Cursor, Copilot, Claude Code, etc.) working in this repository; `CLAUDE.md` and `.github/copilot-instructions.md` only point here.

## What this is

Haruki Event Tracker scrapes ranking data from the Haruki Sekai API for *Project Sekai* (プロジェクトセカイ), persists it to a per-server SQL database, and exposes query APIs (cloud/bot leaderboard queries, public web leaderboard APIs, WebSocket realtime updates, heartbeat status) for downstream clients such as HarukiBot and the public website.

## Project state

- Active branch: `main`. The repo was rewritten from Go on `rewrite/rust` and **the Rust port took over production traffic at 2026-04-28 05:01:54Z**; `REWRITE_PLAN.md` is the frozen historical record of that rewrite (all phases `[x]`, cutover verification, Go behaviour intentionally not ported, rollback handle). All cutover follow-ups (GHCR image via `v2.0.0` tag, config migration) are done.
- The project is now on the v4 line (`Cargo.toml` carries the latest released version, currently `4.2.0`; bump it before tagging). v3 added the `/api/v2/{cloud,web}` route surface, a two-tier API cache, WebSocket realtime push, UID anonymization for public web APIs, private (raw-UID) endpoints behind Toolbox ownership checks, and OpenDAL-backed config/master-data locations. v4 added the cluster roles (`CLUSTER_PLAN.md` is the design record). Web API surface details: `WEB_API_CAPABILITIES.md`. Design notes: `docs/ws-vs-http-leaderboards.md` (versioned HTTP parts + realtime pushes), `docs/vacuum-finished-events.md`.
- No `tests/` integration suite. `cargo test --lib` runs ~290 unit tests in `#[cfg(test)]` modules; Redis-backed tests skip themselves unless `HARUKI_COVERAGE_REDIS_URL` is set (CI sets it), and the PostgreSQL ones are `#[ignore]`d — run them with `HET_TEST_PG_URL=postgres://... cargo test --lib -- --ignored`. HTTP/DB behaviour is otherwise validated against staging. `examples/perf_bench.rs` and `examples/trace_columns_bench.rs` are benchmark-style example targets; `tools/bench-rank-edges.sh` benchmarks the rank-edge queries.

## Build & run

- MSRV: Rust 1.88 (edition 2024).
- Build: `cargo build --release --bin haruki-event-tracker`. The release profile already sets `lto = "thin"`, `codegen-units = 1`, `strip = true`, `opt-level = 3`; the global allocator is mimalloc (`main.rs`).
- Test: `cargo test --lib`.
- Lint: `cargo clippy --all-targets -- -D warnings` — keep clippy clean before committing (CI also runs `cargo fmt --check`).
- Run locally: reads `haruki-tracker-configs.yaml` from the working directory (override with `--config <uri>` or `HARUKI_CONFIG_URI`; config location and `master_data_dir` go through `storage.rs` (OpenDAL), so `file://`/`http(s)://`/`s3://` URIs work). Redis is required only when a tracker daemon is enabled — API-only mode (`tracker.enabled: false` everywhere) skips Redis, the Sekai API client, and the scheduler.
- Operator subcommands (same binary): `repair-time-ids`, `create-covering-indexes`, `vacuum-finished-events` (see "Per-server DBs" below; usage strings in `main.rs`).
- Docker: `docker build -t haruki-event-tracker .` (multi-stage: cargo-chef `lukemathwalker/cargo-chef:<ver>-rust-1.98-alpine` cooks the dependencies in their own cached layer, then an `alpine:3.24` runtime, non-root user, `WORKDIR /app`, `EXPOSE 8080`, ~29 MB; Dependabot bumps the builder tag). Mount the config file into `/app`. The version comes from `Cargo.toml`; there is no version build arg.
- Kubernetes: plain manifests under `deploy/kubernetes/` (`base` at one replica, `overlays/api-only` for API-only replicas; see its README). Do not scale tracker-enabled pods above one replica — use the cluster roles instead.
- Local Kubernetes smoke test: `scripts/smoke_k8s_orbstack.sh` (OrbStack; builds the image, runs API-only mode against a temp PostgreSQL, checks `/livez` + `/readyz`).
- `scripts/diff_go_vs_rust.sh` is the historical Go-vs-Rust parity sweep from the cutover; it targets the Go-era routes and is no longer useful against current builds.

## Configuration

`haruki-tracker-configs.example.yaml` is the commented template (Chinese comments). Top-level sections (`src/config.rs`): `redis`, `api_cache` (TTLs, pool, precompression), `api_query` (trace concurrency limits), `privacy.uid_anonymization.{enabled, salt}`, `toolbox` (private-lookup backend), `backend` (listen/TLS, log files, trusted proxies, `access_log_sample_rate`, `access_log_slow_threshold_ms`), `realtime` (push/online intervals, WS ping/idle), `cluster` (`role`, `token`, `writer_url`, `replica_wait_ms`, reconnect/ping), `cloud_api.tokens`, `sekai_api`, and `servers.<region>` (`enabled`, `master_data_dir`, `tracker`, `db`). Keep the example in sync when adding fields.

Gotcha: the example currently has **no `privacy` section** (and no `access_log_sample_rate` / `access_log_slow_threshold_ms`, which fall back to defaults). Anonymization defaults to off, and then every web route that returns player data (everything except the `.../status` heartbeat routes) answers 400 `web API requires privacy.uid_anonymization.enabled` and the `writer` role refuses to start. Add `privacy.uid_anonymization.enabled: true` with a non-empty `salt` (empty salt with `enabled: true` is a startup error) to any config built from the example that serves the web group.

## Architecture pointers

The process wires five long-lived subsystems together in `main.rs` → `app::build`:

1. **HTTP** (`src/api/`) — `axum` 0.8 (with `ws`) + `tower-http` + `axum-server` for unified HTTP / HTTPS (10 s graceful shutdown). JSON in/out goes through `api::json` (sonic-rs; `Json<T>`, `RawJson`, `EncodedJson` for pre-gzipped cache hits). Routes are built by `api::router::build_router`; the Go-era `/event/{server}/{event_id}/...` prefix no longer exists. Current surface:
   - `GET /livez` (process liveness) and `GET /readyz` (pings every DB, 503 on failure; also reports the cluster role, the reader's link status and replication lag).
   - `GET /ws-ticket` + `GET /ws` — WebSocket realtime (see API-side services).
   - **Cloud group** (bot clients; bearer token from `cloud_api.tokens` via `api::cloud_auth`, open when the list is empty): `/api/v2/cloud/events/{server}/{event_id}/leaderboards/total/sk/{query,check-room,line,speed,trace,status}` and `.../world-bloom/{character_id}/sk/{query,check-room,line,speed,trace}` → `handler::leaderboard::cloud` / `handler::status`. Always speaks **raw** upstream UIDs (`ApiAudience::Cloud`), whatever the anonymization switch says.
   - **Web group** (public website; always `unique_id`, `ApiAudience::Web`, requires `privacy.uid_anonymization.enabled`): `/api/v2/web/events/{server}/{event_id}/leaderboards/total/{overview,replay/overview,top100,borders,growth,status,details/rank/{rank},details/user/{user_id},users/search,check-room}` and the same set under `.../world-bloom/{character_id}/...` → `handler::leaderboard::web` / `handler::web`. `top100` / `borders` / `growth` / `status` are the overview split into separately cacheable parts. Overviews take `interval`/`at`; details take `interval`/`at`/`includeTrace`/`includePlayerTrace`/`includeProfile`/`cursor`/`limit`, and traces can be requested in columnar form with `traceFormat=columns` (`model::trace_columns`). `check-room?userId=<raw uid>` and `details/user/{raw}?idType=uid` are the only web entry points that accept a raw UID; they resolve it to `unique_id`, serve the cached anonymized detail, then reveal the raw UID for the subject only (`subject` block + the subject's own rows).
   - **Private sub-group** (raw-UID lookups): `.../total/private/details/user/{user_id}` and `.../world-bloom/{character_id}/private/details/user/{user_id}`, guarded by `handler::private::require_subject` (subject from the WS proxy extension or trusted-proxy Oathkeeper headers; 401 otherwise).
   - **Cluster stream** (writer role only): `GET /internal/updates` (`api::cluster`, bearer `cluster.token`) streams `hello` / `updated` / `ping` JSON frames from the `UpdateBus`. A writer mounts only health, this stream and the webhook below — no cloud/web/WS routes.
   - **Registry webhook** (any non-reader role, mounted only when `cluster.token` is set): `POST /internal/master-updated` `{server, dataVersion}` (the master registry's subscriber callback shape) drops the region's `EventDataParser` cache so the next tick re-reads `events.json` / `worldBlooms.json` immediately. Point `master_data_dir` at the registry's per-region `files/` URL — its `no-cache` + ETag answers are exactly the parser's stat fingerprint.

   Middleware, outermost first: catch-panic → access log (`access_log::log` with `ProxyTrust`) → web HTTP caching (`http_cache::web_cache_headers`, `/api/v2/web/` only) → compression (gzip + br + zstd at quality 1, bodies > 1 KiB; precompressed cache hits pass through). `api::extract::resolve_engine` parses `:server` against `AppState`'s per-server `Arc<DatabaseEngine>` map; an unknown server returns 400 via `api::error::ApiError::InvalidServer`.
2. **Per-server DBs** (`src/db/`) — one `DatabaseEngine` per enabled `cfg.servers` entry, sea-orm 2.0.0-rc (pinned `sea-query` 1.0.0-rc) with MySQL / PostgreSQL / SQLite drivers, dialect chosen from `DbConfig.dialect`. Tables are created dynamically per `(server, event_id)`: `db::schema::create_event_tables` bootstraps the four table kinds (`TableKind::{TimeId, EventUsers, Event, WorldBloom}` → `event_<id>_time_id`, `event_<id>_users`, `event_<id>`, `wl_<id>`) plus read indexes, and `db::table_name::intern(TableKind, event_id)` returns the `&'static str` name used in `sea-query` aliases — route new queries through `intern`, never hardcode names. Query modules (`db::query`): `batch` (write path), `ranking`, `lines`, `growth`, `heartbeat`, `user`, `world_bloom`, `web` (cursor-paginated web searches/traces), `score_samples` (per-player histories behind the cloud speed metrics), and the crate-internal `edge`, `keys`, `trace`. `db::privacy::ensure_user_table_extensions` lazily migrates pre-existing `_users` tables (profile columns, `unique_id` backfill + index); it runs from tracker init and is memoized from `AppState`.

   **Invariant: `event_<id>_time_id.time_id` order == `timestamp` order.** Readers rely on it — per-key first/last rows (`lines`, `growth`, `web` rank windows, top-player growth) are per-key `ORDER BY time_id {ASC|DESC} LIMIT 1` probes on the `(rank, time_id)` / `(user_id_key, time_id)` indexes (`db::query::edge`), time windows are translated to `time_id` bounds through the time table's unique `timestamp` index (so no query filters by joining every history row to the time table), single-row latest lookups are `ORDER BY time_id DESC`. The writer maintains it by assigning `time_id = timestamp` to every new row (`db::entity::time_id::time_id_for_timestamp`, used by the batch inserts and `write_heartbeat`; the column's identity sequence is left unused). Legacy rows carry sequence ids; where a historical merge left them out of order, `haruki-event-tracker repair-time-ids --region <r> --event <id> [--dry-run]` (`db::repair`) renumbers the event to `time_id = timestamp` (time table + `event_<id>` + `wl_<id>`, one transaction, idempotent; PostgreSQL/SQLite). Finished events are frozen by hand with `haruki-event-tracker vacuum-finished-events` (`db::maintenance`, PostgreSQL primary only, never scheduled; see `docs/vacuum-finished-events.md`). **Rollback trap:** a binary predating this rule resumes the identity sequence, so on PostgreSQL its new rows get small ids below existing timestamp-valued ones (fresh inversions; readers return stale "latest"), and on SQLite/MySQL the auto-increment continues from `max+1` and the next same-second row from a new binary collides on the primary key — after this rule is deployed, don't run an older writer against the same tables. Traces (`db::query::trace`) read the ranking table alone once an event's time table is known to have `time_id == timestamp` on every row (checked once per process, cached per `(region, event)`), and are index-only scans on the covering indexes `idx_<id>_{rank_time,user_time}_cov` / `idx_<id>_wl_char_{rank,user}_time_cov` (PostgreSQL `INCLUDE`), which new events get at bootstrap; tables with history get them only through `haruki-event-tracker create-covering-indexes --region <r> [--event <id>] [--dry-run]` (`db::schema::create_covering_indexes`: `CREATE INDEX CONCURRENTLY`, one at a time, skips valid ones, rebuilds invalid leftovers; the plain indexes stay). On PostgreSQL, key sets go in as one `bigint[]` parameter (`db::query::keys`: `= ANY($1)`, `unnest($1)`) so statement text does not vary with the key count.
3. **Tracker daemons** (`src/tracker/`) — one `HarukiEventTracker` per server with `tracker.enabled: true`, scheduled by `tokio_cron_scheduler` (cron expression from config; the `use_second_level_cron: false` 5-field form is padded with a leading `"0 "` for the crate's 6-field schedule — don't strip the pad). The job uses `try_lock` and **skips** a tick while the previous one is still running. Each tick:
   - `EventDataParser::get_current_event_status` reads `events.json` / `worldBlooms.json` from `master_data_dir` and produces an `EventStatus` for the current wallclock.
   - `HarukiEventTracker::track_ranking_data` reinitialises the inner `EventTrackerBase` when the event id advances, skips `aggregating` events, finalizes once on `ended`, then calls `record_ranking_data`. After an event ends, `refresh_after_end` keeps re-fetching every `tracker.post_end_user_refresh_interval_secs` (default 3600) to record post-end corrections (banned-account cleanups, final border settlement) as new trace points and refresh user profiles.
   - `EventTrackerBase::handle_ranking_data` calls `HarukiSekaiAPIClient::get_top100` + `get_border` (borders no more often than `border_fetch_interval_secs`, with a backoff after failures), hashes the border response (SHA-256) and skips the merge when it matches the last persisted hash (`tracker::cache::check_cache`; in-memory mirror first, Redis after a restart). The hash is committed with `store_cache` only after the merged rows landed. Hex output uses `{:02x}` to stay byte-compatible with the Go-era fingerprints.
   - Diffing is rank-based: `tracker::diff::diff_rank_based` compares each rank's `(user_id, score)` against `prev_rank_state` and keeps only rows that moved. State is mirrored to Redis under `haruki:tracker:<server>:<event>:{rank_state,ended}` — byte-compatible with the Go version and still holding live production state. (Go also wrote a `user_state` hash but never read it back; that key is intentionally not ported.)
   - Writes: diffed rows accumulate in `tracker::pending::PendingBuffer` (each keeps its sample timestamp) and are flushed per `flush_interval_secs` / `flush_max_rows` / `flush_hot_ranks` (`0` interval = every tick) through `db::query::batch::flush_batch` — users, main and World Bloom rows of whole samples in **one transaction per chunk** (on PostgreSQL a single simple-protocol message through `db::pg_session` when the time table is aligned, bounded by the engine's write timeout). A failed chunk is put back and retried; Redis `rank_state` and the border hash advance only after the buffer drained. Status-only heartbeat rows (`db::query::heartbeat::write_heartbeat`) are written on idle / API-error ticks, throttled by `idle_heartbeat_interval_secs`, so the status endpoint reports freshness. Pending rows are flushed on graceful shutdown.
   - Every flush is bracketed with API-cache invalidation through `tracker::invalidation::CacheInvalidation` (`begin_event_update` → write → `finish_event_update` epoch bump, or `abort_event_update` on error) and followed by a `RealtimeHub` `updated` broadcast carrying the new epoch as `version`.
4. **Cluster roles** (`src/cluster.rs`, `src/cluster/subscriber.rs`, `cluster` config) — `standalone` (default, everything in one process), `writer` (tracks, publishes each committed flush on the in-process `UpdateBus` with the primary's WAL LSN, serves only health + `/internal/*`; requires `cluster.token` and `privacy.uid_anonymization.enabled`), `reader` (never tracks — `tracker.enabled` is ignored; requires `cluster.token` + `cluster.writer_url`; `DatabaseEngine::is_read_only()` so `db::privacy` verifies columns instead of ALTERing, since the DB may be a streaming replica; `cluster::subscriber` dials the writer's `/internal/updates`, waits up to `replica_wait_ms` for the LSN to replay, bumps the local API-cache epoch and fires the realtime `updated`). `CacheInvalidation` is the seam: `LocalRedis` for standalone, `Cluster(bus)` for writers.
5. **Bootstrap & shutdown** (`src/app.rs`, `src/shutdown.rs`) — `app::build` validates the role invariants and returns an `AppContext { state, dbs, trackers, scheduler }`. `shutdown::signal()` resolves on SIGINT/SIGTERM (Ctrl+C on Windows); `shutdown::run` stops the scheduler, flushes each tracker's pending rows, then drops trackers, `AppState` and the DB engines (connections close on `Drop`).

### API-side services (`src/api/`)

- **`cache.rs`** — two-tier API cache: sharded in-process L1 (quick_cache) + Redis L2 (own connection pool, Lua read/write scripts, per-event epoch/dirty control keys, single-flight, optional gzip precompression). Configured by the `api_cache` section; `AppState` holds it as `Option<ApiCache>`. Redis keys live under `haruki:tracker:<server>:<event>:api_cache:*` — Rust-only keyspace, no Go-compat constraint, but it shares the tracker prefix.
- **`http_cache.rs`** — web-group GETs only: strong `ETag` over the exact bytes sent, `If-None-Match` → `304`, and `Cache-Control`: `public, max-age=86400, immutable` only when the request's `v=<epoch>` equals the `ServedEpoch` the cache tagged the body with, `private, no-store` for `/private/` routes and raw-UID lookups (`http_cache::private`), `public, max-age=1, stale-while-revalidate=5` otherwise. Only the epoch-pure overview parts (`top100` / `borders` / `growth`, windows anchored at the per-epoch rank cut via `build_overview_until`) set `ServedEpoch`; anything reading the wall clock or the heartbeat must not. On cluster readers the realtime `version` is pushed only after the replica replayed the writer's LSN. Design and rollout: `docs/ws-vs-http-leaderboards.md`.
- **`limiter.rs`** — `ApiQueryLimiter`: global + per-server semaphores bounding trace-query concurrency (`api_query` config). This is admission control inside handlers, **not** per-IP rate limiting (none exists).
- **`realtime.rs` / `ws.rs` / `ws_ticket.rs`** — `RealtimeHub` broadcast channel with per-`(server, event_id)` topics and online counters. `GET /ws-ticket` resolves a subject from trusted-proxy (Oathkeeper) headers and issues a single-use 45 s ticket; `GET /ws?ticket=...` upgrades, then proxies JSON frames (`subscribe`/`unsubscribe`/`ping`/request) into the web router via `tower::ServiceExt::oneshot`, injecting the socket's subject as a `PrivateSubject` extension — that's how private endpoints become reachable over WS. Pushes `ready` / `updated` / `online` events for subscribed topics; trackers feed it via `notify_realtime_update`.
- **`handler/private.rs` / `private_lookup.rs`** — `require_subject` middleware plus raw-UID detail handlers; `PrivateLookupVerifier` (from the `toolbox` config section) asks the Toolbox backend whether the subject owns a given `(server, user_id)` binding.
- **`cloud_auth.rs`** — bearer check for the cloud group against every configured token (constant-time compare; open, with a startup warning, when `cloud_api.tokens` is empty).
- **Privacy** (`src/privacy.rs` + `src/db/privacy.rs`) — `UidAnonymizer` (salted SHA-256) produces the public `unique_id` from `privacy.uid_anonymization.{enabled, salt}`. Public web APIs accept and return only `unique_id`; raw UID stays internal (see `WEB_API_CAPABILITIES.md`).
- **`access_log.rs`** — access-log middleware + `ProxyTrust` (trusted CIDRs, client-IP header, `access_log_sample_rate`, `access_log_slow_threshold_ms`); logs to `target = "access"`.
- **`stats.rs`** — global atomic cache/access/API counters with a periodic aggregation logger spawned from `main`.
- **`storage.rs`** (top-level) — OpenDAL wrapper (`StorageRoot` / `StorageFile`) behind config loading and `master_data_dir` reads.

### World Bloom specifics

World Bloom events have per-character chapters tracked in parallel. `EventTrackerBase` keeps `world_bloom_statuses` and `is_world_bloom_chapter_ended` maps; `HarukiEventTracker::handle_world_bloom` iterates *all* chapters each tick (overlap periods are intentional), and `handle_world_bloom_chapter` skips chapters that are `not_started`, `aggregating`, or already finalised. World Bloom rows are persisted in the separate `wl_<event_id>` table named by `intern(TableKind::WorldBloom, _)`.

### Models

`src/model/` holds *all* shared types — API request/response schemas (`api.rs`), the columnar trace encoding behind `traceFormat=columns` (`trace_columns.rs` — `TraceColumns` codec and the `TracePayload` detail field that splices either wire form), DB config (`db_config.rs`), domain enums (`enums.rs` — `SekaiServerRegion`, `SekaiEventType`, `SekaiEventStatus`, the `SEKAI_EVENT_RANKING_LINES_NORMAL` / `_WORLD_BLOOM` constants), event master data structs (`event.rs`), upstream Sekai API DTOs (`sekai.rs`), and tracker state structs (`tracker.rs`). `db` and `tracker` both depend on `model`; `model` depends on nothing internal — keep it that way to avoid cycles. The upstream client lives in `src/sekai_api/`.

## Conventions to follow when writing code

- **No `mod.rs`** — every module lives in `foo.rs` with optional siblings under `foo/`; `src/lib.rs` declares the top-level modules.
- **Comments are sparse** — only when the *why* is non-obvious (cross-language wire compat, lifetime workarounds, Go-version dead code intentionally skipped). Don't narrate.
- **Wire compatibility** with the Go version is load-bearing: Redis key suffixes, JSON field names, hex-encoded SHA-256 casing, `PlayerState/RankState` single-letter serde rename keys (`s` / `r` / `u`). Don't change without coordinating a hard cutover. The `api_cache:*` keyspace under the same `haruki:tracker:` prefix is Rust-only and versioned by epoch — evolve it via the epoch/control-key mechanism in `api::cache`, not by renaming ad hoc.
- **Server identifiers** are the lowercase `model::enums::SekaiServerRegion` strings (`jp` / `en` / `tw` / `kr` / `cn`) everywhere — routes, configs, table names, Redis keys, span fields.
- **Dynamic table inserts** must go through `sea-query` (`Query::insert_into(Alias::new(intern(...)))`); the SeaORM `ActiveModel` API doesn't work because Entity types carry a non-unit `table_name` field.
- **JSON** is sonic-rs everywhere (`sonic_rs::{from_str, from_slice, to_vec, to_string}`); `api::json::Json<T>` wraps it for handlers.
- **Privacy**: public web endpoints only accept/return the anonymized `unique_id` — never raw upstream UID or `twitterId` in web responses, logs, or cache keys. The two exceptions are explicit opt-ins by the caller (web `check-room?userId=`, `details/user/{id}?idType=uid`) and reveal only the queried player; raw-UID access for *other* players goes only through the private endpoints + Toolbox verification. Cloud handlers always use `PublicUserIdMode::Raw` via `ApiAudience::Cloud`; cloud trace cache keys hash the subject.
- **DSN form**: sqlx wants URL form (`mysql://user:pwd@host:port/db?charset=utf8mb4`). The Go-style `user:pwd@tcp(host:port)/db?...` is not accepted; `parseTime` and `loc` are GORM-only and must be dropped.
- **Errors** are typed per module with `thiserror`; don't add panics or `unwrap` outside tests and obviously infallible parsing.
- **TLS**: rustls 0.23 panics in `ServerConfig::builder()` when both the `ring` and `aws_lc_rs` providers are reachable in the dep graph (they are, transitively). `main` calls `rustls::crypto::aws_lc_rs::default_provider().install_default()` on the SSL branch — keep that line.
- **Logging** is `tracing` with the `HarukiFormat` formatter in `src/logger.rs`: `[YYYY-MM-DD HH:MM:SS.mmm][LEVEL][component] message`, where `component` is the first module after `haruki_event_tracker::` (`http` for `tower_http`, the first target segment for other crates), optionally followed by identity tags. `target = "access"` goes to `backend.access_log_path` (or into `backend.main_log_file` when that is empty); everything else goes to `backend.main_log_file`; stdout gets everything. File sinks strip ANSI and are non-blocking and lossy under back-pressure (dropped lines are counted in the periodic stats).

## Git commits

All commit subjects must follow:

```text
[Type] Short description starting with capital letter
```

Allowed types:

| Type      | Usage                                                 |
|-----------|-------------------------------------------------------|
| `[Feat]`  | New feature or capability                             |
| `[Fix]`   | Bug fix                                               |
| `[Chore]` | Maintenance, refactoring, dependency or build changes |
| `[Docs]`  | Documentation-only changes                            |

Rules:

- Description starts with a capital letter.
- Use imperative mood: `Add ...`, not `Added ...`.
- No trailing period.
- Keep the subject at or below roughly 70 characters.
- **Agent attribution uses the standard Git `Co-authored-by:` trailer in the commit body, not a free-form `Agent:` line.** This makes GitHub render the co-author avatar on the commit page. The trailer must be on its own line, separated from the subject by a blank line, in the form `Co-authored-by: <Display Name> <email>`. Suggested values per agent:
  - Claude (any model): `Co-authored-by: Claude Fable 5 <noreply@anthropic.com>` (substitute the actual model, e.g. `Claude Opus 4.7`, `Claude Sonnet 4.6`)
  - Codex: `Co-authored-by: Codex <noreply@openai.com>`
  - Copilot: `Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>`

Examples from this repo's history:

```text
[Feat] Add cloud native tracker runtime
[Fix] Address PR review feedback
[Chore] Update dependencies
[Docs] Mark cutover complete in REWRITE_PLAN
```

## GitHub Actions workflows

CI reuses the shared templates in
[`seiunx-dev/ci-templates`](https://github.com/seiunx-dev/ci-templates) at `@v1`.
The files in `.github/workflows` are thin callers:

- `ci.yml` (`CI`) runs on `main` pushes, pull requests targeting `main`, and manual
  dispatch: `rust-ci` (fmt, clippy `--all-targets -D warnings`, tests run once under
  `cargo llvm-cov` with a Redis service exposed as `HARUKI_COVERAGE_REDIS_URL`) →
  `sonar` (scans the uploaded coverage; skipped green on Dependabot/fork PRs), plus
  `docker` and `actionlint`.
- `docker` does not wait for the tests. PRs build only; on `main` it runs in parallel
  with `rust-ci` and pushes the immutable
  `ghcr.io/team-haruki/haruki-event-tracker:sha-<full sha>` and `:sha-<7 chars>` as soon
  as the build finishes. The `Docker tags` job (`docker-retag.yml`, after `CI OK`) then
  moves `:main` to that digest without rebuilding, so `:main` only follows commits whose
  `CI OK` passed and lags the `:sha-*` tags until then. The Dockerfile uses cargo-chef;
  the registry `:buildcache` keeps the cooked dependency layer.
- The aggregate job **`CI OK`** is the gate: `Docker tags` and the release gate wait on it, and it is meant to be the single required check. `main` has no branch protection at the moment, so GitHub does not enforce it — don't merge a PR while `CI OK` is red or pending.
- `release.yml` (`Release`): bump `version` in `Cargo.toml` in a PR → merge and wait for
  `CI OK` on `main` → push the tag `v<version>`. `release-gate` refuses a tag that
  differs from `Cargo.toml` and waits for `CI OK` on the tagged commit; then the
  binaries are built (tags only; `haruki-event-tracker-linux-x64.tar.gz`,
  `-macos-arm64.tar.gz`, `-windows-x64.zip`, binary at the archive root), the `main`
  image `:sha-<sha>` is promoted (re-tagged, not rebuilt) to `:<version>`,
  `:<major>.<minor>` and `:latest` (production pulls `:<version>`), and the GitHub
  Release is published with `SHA256SUMS-<tag>.txt`. Manual dispatch is a dry run: it
  builds the binaries and publishes nothing.

Workflow maintenance rules:

- Use the shared templates first. Add custom jobs or steps only when a template
  genuinely cannot meet the project's needs, keep them in the thin caller files, and
  add a comment explaining why.
- Template bugs and missing features are fixed upstream in `seiunx-dev/ci-templates`
  (new `v1.x.y` tag), not worked around here.
- Keep top-level `permissions: contents: read`; grant `packages: write` / `contents: write`
  only on the job that needs it.
- Do not suppress `githubactions:S7637` (full-SHA pins) in `sonar-project.properties`: the
  template's `sonar.yml` already ignores it for the `@v1` references.
- Third-party actions in caller-side custom steps are pinned to a full commit SHA with a
  `# vX.Y.Z` comment; Dependabot (`github-actions`) updates them and the template refs.
