# AGENTS.md

Cross-agent guidance for Haruki Event Tracker. This file is the entry point for any AI coding agent (Codex, Cursor, Copilot, Claude Code, etc.) working in this repository.

## What this is

Haruki Event Tracker scrapes ranking data from the Haruki Sekai API for *Project Sekai* (プロジェクトセカイ), persists it to a per-server SQL database, and exposes query APIs (cloud/bot leaderboard queries, public web leaderboard APIs, WebSocket realtime updates, heartbeat status) for downstream clients such as HarukiBot and the public website.

## Project state

- Active branch: `main`. The repo was rewritten from Go on `rewrite/rust` and **the Rust port took over production traffic at 2026-04-28 05:01:54Z**; `REWRITE_PLAN.md` is the frozen historical record of that rewrite (all phases `[x]`, cutover verification, rollback handle). All cutover follow-ups (GHCR image via `v2.0.0` tag, config migration) are done.
- The project is now on the v3 line (latest tag `v3.3.0`; `Cargo.toml` carries `3.0.0-dev` as the dev-version convention). v3 added the `/api/v2/{cloud,web}` route surface, a two-tier API cache, WebSocket realtime push, UID anonymization for public web APIs, private (raw-UID) endpoints behind Toolbox ownership checks, and OpenDAL-backed config/master-data locations. Web API surface details: `WEB_API_CAPABILITIES.md`.
- No `tests/` integration suite. `cargo test --lib` runs ~100 unit tests in `#[cfg(test)]` modules; HTTP/DB behaviour is validated against staging.

## Build & run

- MSRV: Rust 1.88 (edition 2024).
- Build: `cargo build --release --bin haruki-event-tracker`. The release profile already sets `lto = "thin"`, `codegen-units = 1`, `strip = true`, `opt-level = 3`.
- Test: `cargo test --lib` — unit tests sit in `#[cfg(test)]` modules across `api` (cache, access_log, ws_ticket, leaderboard services), `db` (query/web, schema, privacy), `tracker` (diff, parser, base), `storage`, and `privacy`. `examples/perf_bench.rs` is a benchmark-style example target.
- Lint: `cargo clippy --all-targets -- -D warnings` — keep clippy clean before committing.
- Run locally: reads `haruki-tracker-configs.yaml` from the working directory (override with `--config <uri>` or `HARUKI_CONFIG_URI`; OpenDAL `file://`/`http(s)://`/`s3://` URIs work). Redis is required only when a tracker daemon is enabled — API-only mode skips Redis, the Sekai API client, and the scheduler.
- Docker: `docker build --build-arg VERSION=<ver> -t haruki-event-tracker .` (`rust:1.98-alpine` builder → `alpine:3.24` runtime, non-root user, ~29 MB image; dependabot bumps the builder tag). The image expects the config file mounted into `/app`. The builder pre-builds deps from a dummy `src/main.rs`; keep the `find src -name '*.rs' -exec touch {} +` line — Docker `COPY` preserves host mtimes and cargo otherwise skips the real rebuild.
- Releases: pushing a `v*` tag triggers `.github/workflows/release.yml` (linux-x64, macos-arm64, windows-x64) and `.github/workflows/docker.yml` (GHCR push).
- Local Kubernetes smoke test: `scripts/smoke_k8s_orbstack.sh` (OrbStack; builds the image, runs API-only mode against a temp PostgreSQL, checks `/livez` + `/readyz`).

## Architecture pointers

The process wires five long-lived subsystems together in `main.rs` → `app::build`:

1. **HTTP** (`src/api/`) — `axum` 0.8 (with `ws`) + `tower-http` + `axum-server` for unified HTTP / HTTPS via `Handle::graceful_shutdown(10s)`. JSON in/out goes through `api::json` (sonic-rs; `Json<T>`, `RawJson`, `EncodedJson` for pre-gzipped cache hits). All routes are GET and are built by `api::router::build_router`; the Go-era `/event/{server}/{event_id}/...` prefix no longer exists. Current surface:
   - `GET /livez` (process liveness) and `GET /readyz` (pings every DB, 503 on failure; also reports cluster role, the reader's link status and replication lag).
   - `GET /ws-ticket` + `GET /ws` — WebSocket realtime.
   - **Cloud group** (bot clients; bearer token from `cloud_api.tokens` via `api::cloud_auth`, open when the list is empty): `/api/v2/cloud/events/{server}/{event_id}/leaderboards/total/sk/{query,check-room,line,speed,trace,status}` and `.../world-bloom/{character_id}/sk/{query,check-room,line,speed,trace}` → `handler::leaderboard::cloud` / `handler::status`. Always speaks **raw** upstream UIDs (`ApiAudience::Cloud`), whatever the anonymization switch says.
   - **Web group** (public website; always `unique_id`, `ApiAudience::Web`, requires `privacy.uid_anonymization.enabled`): `/api/v2/web/events/{server}/{event_id}/leaderboards/total/{overview,replay/overview,details/rank/{rank},details/user/{user_id},users/search,check-room}` and the same set under `.../world-bloom/{character_id}/...` → `handler::leaderboard::web`. Overviews take `interval`/`at`; details take `interval`/`at`/`includeTrace`/`includePlayerTrace`/`includeProfile`/`cursor`/`limit`. `check-room?userId=<raw uid>` and `details/user/{raw}?idType=uid` are the only web entry points that accept a raw UID; they resolve it to `unique_id`, serve the cached anonymized detail, then reveal the raw UID for the subject only (`subject` block + the subject's own rows).
   - **Private sub-group** (raw-UID lookups): `.../total/private/details/user/{user_id}` and `.../world-bloom/{character_id}/private/details/user/{user_id}`, guarded by `private::require_subject` (subject from the WS proxy extension or trusted-proxy Oathkeeper headers; 401 otherwise).
   - **Cluster stream** (writer role only): `GET /internal/updates` (`api::cluster`, bearer `cluster.token`) streams `hello` / `updated` / `ping` JSON frames from the `UpdateBus`.
   - **Registry webhook** (any role that runs tracker daemons, when `cluster.token` is set): `POST /internal/master-updated` `{server, dataVersion}` (the master registry's `registry.subscribers` shape) drops the region's `EventDataParser` cache so the next tick re-reads `events.json` / `worldBlooms.json` immediately. Point `master_data_dir` at the registry's `files/` URL (`http://haruki-master-registry:9998/v1/master/<region>/files/`) — its `no-cache` + ETag answers are exactly the parser's stat fingerprint.

   Middleware, outermost first: access log (`access_log::log` with `ProxyTrust`) → compression (gzip/br, level 4, bodies ≥1 KiB) → catch-panic. `api::extract::resolve_engine` parses `:server` against `AppState`'s per-server `Arc<DatabaseEngine>` map; an unknown server returns 400 via `api::error::ApiError::InvalidServer`.
2. **Per-server DBs** (`src/db/`) — one `DatabaseEngine` per enabled `cfg.servers` entry, sea-orm 2.0.0-rc (pinned `sea-query` 1.0.0-rc) with MySQL / PostgreSQL / SQLite drivers, dialect chosen from `DbConfig.dialect`. Tables are created dynamically per `(server, event_id)`: `db::schema::create_event_tables` bootstraps the four table kinds (`TableKind::{TimeId, EventUsers, Event, WorldBloom}` → `event_<id>_time_id`, `event_<id>_users`, `event_<id>`, `wl_<id>`) plus web-read indexes, and `db::table_name::intern(TableKind, event_id)` returns the `&'static str` name used in `sea-query` aliases — route new queries through `intern`, never hardcode names. Query modules: `batch` (write path), `ranking`, `lines`, `growth`, `heartbeat`, `user`, `world_bloom`, and `web` (cursor-paginated web searches/traces). `db::privacy::ensure_user_table_extensions` lazily migrates pre-existing `_users` tables (profile columns, `unique_id` backfill + index); it runs from tracker init and memoized from `AppState`.
3. **Tracker daemons** (`src/tracker/`) — one `HarukiEventTracker` per server with `tracker.enabled: true`, scheduled by `tokio_cron_scheduler` (cron expression from config; the `use_second_level_cron: false` 5-field form is auto-padded with a leading `"0 "` for the crate's required 6-field schedule). The job uses `try_lock` and **skips** a tick if the previous one is still running. Each tick:
   - `EventDataParser::get_current_event_status` reads `events.json` / `worldBlooms.json` from `master_data_dir` and produces an `EventStatus` for the current wallclock.
   - `HarukiEventTracker::track_ranking_data` reinitialises the inner `EventTrackerBase` when the event id advances, short-circuits if the event is `aggregating` / `ended`, then calls `record_ranking_data`. After an event ends, `refresh_after_end` keeps re-fetching on `tracker.post_end_user_refresh_interval_secs` (default 3600) to record post-end corrections (banned-account cleanups, final border settlement) as new trace points and refresh user profiles.
   - `EventTrackerBase::handle_ranking_data` calls `HarukiSekaiAPIClient::get_top100` + `get_border`, hashes the border response (SHA-256) and uses `tracker::cache::detect_cache` (Redis hex-encoded match) to skip the merge when nothing changed. Hex output uses `format!("{:02x}")` to stay byte-compatible with the Go-era fingerprints.
   - Diffing is rank-based: `tracker::diff::diff_rank_based` compares each rank's `(user_id, score)` against `prev_rank_state` and persists only rows that moved. State is mirrored to Redis under `haruki:tracker:<server>:<event>:{rank_state,ended}` — byte-compatible with the Go version and still holding live production state. (Go also wrote a `user_state` hash but never read it back; that key is intentionally not ported.)
   - Writes: `db::query::batch::batch_upsert_event_users` runs *before* the transaction (keeps row locks short); `batch_insert_event_rankings` / `batch_insert_world_bloom_rankings` run inside it. On API failure or no-change ticks a heartbeat row is still written via `db::query::heartbeat::write_heartbeat` so the status endpoint reports freshness.
   - Every write is bracketed with API-cache invalidation (`begin_event_update` → work → `finish_event_update` epoch bump, or `abort_event_update` on error) and followed by a `RealtimeHub` `updated` broadcast.
4. **Cluster roles** (`src/cluster.rs`, `cluster` config) — `standalone` (default, everything in one process), `writer` (tracks, publishes each committed flush on the in-process `UpdateBus` with the primary's WAL LSN, serves only health + `/internal/updates`), `reader` (never tracks, `DatabaseEngine::is_read_only()` so `db::privacy` verifies columns instead of ALTERing — the DB may be a streaming replica — and `cluster::subscriber` dials the writer, waits for the LSN to replay (`replica_wait_ms`), bumps the local API-cache epoch and fires the realtime `updated`). `tracker::invalidation::CacheInvalidation` is the seam: `LocalRedis` for standalone, `Cluster(bus)` for writers.
5. **Bootstrap & shutdown** (`src/app.rs`, `src/shutdown.rs`) — `app::build` returns an `AppContext { state, dbs, trackers, scheduler }`. `shutdown::signal()` resolves on SIGINT/SIGTERM (Ctrl+C on Windows); `shutdown::run` stops the scheduler, drops the trackers (closing the shared Redis `ConnectionManager` handle), then `Arc::try_unwrap` + closes each `DatabaseEngine`.

### API-side services (`src/api/`)

- **`cache.rs`** — two-tier API cache: sharded in-process L1 + Redis L2 (own connection pool, Lua read script, per-event epoch/dirty control keys, single-flight, optional gzip precompression). Configured by the `api_cache` section; `AppState` holds it as `Option<ApiCache>`. Redis keys live under `haruki:tracker:<server>:<event>:api_cache:*`.
- **`limiter.rs`** — `ApiQueryLimiter`: global + per-server semaphores bounding trace-query concurrency (`api_query` config). This is admission control inside handlers, **not** per-IP rate limiting (none exists).
- **`realtime.rs` / `ws.rs` / `ws_ticket.rs`** — `RealtimeHub` broadcast channel with per-`(server, event_id)` topics and online counters. `GET /ws-ticket` resolves a subject from trusted-proxy (Oathkeeper) headers and issues a single-use 45 s ticket; `GET /ws?ticket=...` upgrades, then proxies JSON frames (`subscribe`/`unsubscribe`/`ping`/request) into the web router via `tower::ServiceExt::oneshot`, injecting the socket's subject as a `PrivateSubject` extension — that's how private endpoints become reachable over WS. Pushes `ready` / `updated` / `online` events for subscribed topics; trackers feed it via `notify_realtime_update`.
- **`private.rs` / `private_lookup.rs`** — `require_subject` middleware plus raw-UID detail handlers; `PrivateLookupVerifier` (from the `toolbox` config section) asks the Toolbox backend whether the subject owns a given `(server, user_id)` binding.
- **Privacy** (`src/privacy.rs` + `src/db/privacy.rs`) — `UidAnonymizer` (salted SHA-256) produces the public `unique_id`; `privacy.uid_anonymization.{enabled, salt}` in config (startup error if enabled with an empty salt).
- **`access_log.rs`** — access-log middleware + `ProxyTrust` (trusted CIDRs, client-IP header, `access_log_sample_rate`, `access_log_slow_threshold_ms`); logs to `target = "access"`.
- **`stats.rs`** — global atomic cache/access/API counters with a periodic aggregation logger spawned from `main`.
- **`storage.rs`** (top-level) — OpenDAL wrapper (`StorageRoot` / `StorageFile`) behind config loading and `master_data_dir` reads.

### World Bloom specifics

World Bloom events have per-character chapters tracked in parallel. `EventTrackerBase` keeps `world_bloom_statuses` and `is_world_bloom_chapter_ended` maps; `HarukiEventTracker::handle_world_bloom` iterates *all* chapters each tick (overlap periods are intentional), and `handle_world_bloom_chapter` skips chapters that are `not_started`, `aggregating`, or already finalised. World Bloom rows are persisted via the separate `wl_<event_id>` table built by `intern(TableKind::WorldBloom, _)`.

### Models package

`src/model/` holds *all* shared types — API request/response schemas (`api.rs`), DB config (`db_config.rs`), domain enums (`enums.rs` — `SekaiServerRegion`, `SekaiEventType`, `SekaiEventStatus`, the `SEKAI_EVENT_RANKING_LINES_NORMAL` / `_WORLD_BLOOM` constants), event master data structs (`event.rs`), upstream Sekai API DTOs (`sekai.rs`), and tracker state structs (`tracker.rs`). `db` and `tracker` both depend on `model`; `model` depends on nothing internal — keep it that way to avoid cycles.

## Conventions to follow when writing code

- **No `mod.rs`** — every module lives in `foo.rs` with optional siblings under `foo/`.
- **Comments are sparse** — only when the *why* is non-obvious (cross-language wire compat, lifetime workarounds, Go-version dead code intentionally skipped). Don't narrate.
- **Wire compatibility** with the Go version is load-bearing: Redis key suffixes, JSON field names, hex-encoded SHA-256 casing, `PlayerState/RankState` single-letter serde rename keys (`s` / `r` / `u`). Don't change without coordinating a hard cutover. The `api_cache:*` keyspace under the same `haruki:tracker:` prefix is Rust-only and versioned by epoch — evolve it via the epoch/`control` mechanism in `api::cache`, not by renaming ad hoc.
- **Server identifiers** are the lowercase `model::enums::SekaiServerRegion` strings (`jp` / `en` / `tw` / `kr` / `cn`) everywhere — routes, configs, table names, Redis keys, span fields.
- **Dynamic table inserts** must go through `sea-query` (`Query::insert_into(Alias::new(intern(...)))`); the SeaORM `ActiveModel` API doesn't work because Entity types carry a non-unit `table_name` field.
- **JSON** is sonic-rs everywhere (`sonic_rs::{from_str, from_slice, to_vec, to_string}`); `api::json::Json<T>` wraps it for handlers.
- **Privacy**: public web endpoints only accept/return the anonymized `unique_id` — never raw upstream UID or `twitterId` in web responses, logs, or cache keys. Raw-UID access goes through the private endpoints + Toolbox verification.
- **Privacy, cloud side**: cloud handlers always use `PublicUserIdMode::Raw` via `ApiAudience::Cloud`, and cloud trace cache keys hash the subject.
- **DSN form**: sqlx wants URL form (`mysql://user:pwd@host:port/db?charset=utf8mb4`). The Go-style `user:pwd@tcp(host:port)/db?...` is not accepted; `parseTime` and `loc` are GORM-only and must be dropped.
- **TLS**: rustls 0.23 panics in `ServerConfig::builder()` when both the `ring` and `aws_lc_rs` providers are reachable in the dep graph (they are, transitively). `main` calls `aws_lc_rs::default_provider().install_default()` once on the SSL branch — keep that line.
- **Logging** is `tracing` with the `GoStyleFormat` formatter (`[YYYY-MM-DD HH:MM:SS.mmm][LEVEL][target] message`). `target = "access"` is routed to `access_log_path`; everything else goes to `main_log_file` and stdout. File sinks strip ANSI; stdout keeps it.
- **Config sections** beyond the v2-era basics: `api_cache` (TTLs, pool, precompression), `api_query` (trace concurrency limits), `privacy.uid_anonymization`, `toolbox` (private-lookup backend), `cluster` (role, token, writer_url, replica_wait_ms), `cloud_api.tokens`, and the `backend` access-log knobs (`access_log_sample_rate`, `access_log_slow_threshold_ms`). Keep `haruki-tracker-configs.example.yaml` in sync when adding fields.

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

Use the standardized workflow layout in `.github/workflows`:

- `ci.yml` runs on `main` pushes, pull requests targeting `main`, and manual dispatch.
- Rust CI order: `cargo fmt --all -- --check`, `cargo check --locked --all-targets`, `cargo clippy --locked --all-targets -- -D warnings`, then `cargo test --locked`.
- `release.yml` is the standard release build entrypoint. It runs on `v*` tags and manual dispatch, builds release artifacts, uploads them with `actions/upload-artifact`, and publishes GitHub Release assets on tag pushes.
- `docker.yml` is the standard Docker entrypoint. It runs on `main` pushes, `v*` tags, PRs that touch Docker/build inputs, and manual dispatch. PRs build only; non-PR runs push GHCR images with lowercase image names and Docker metadata tags.

Workflow maintenance rules:

- Keep workflow filenames and top-level names aligned: `CI`, `Release`, `Docker`, and optional package-specific names.
- Use `actions/checkout@v7`, `actions/upload-artifact@v7`, `actions/download-artifact@v8`, `softprops/action-gh-release@v3`, and current Docker actions (`setup-buildx@v4`, `login@v4`, `metadata@v6`, `build-push@v7`).
- Keep `permissions` minimal: `contents: read` for CI/Docker build-only work, `contents: write` for release publishing, and `packages: write` only when pushing container images.
- Use workflow `concurrency` keyed by workflow name and ref, with release jobs using `release-${{ github.ref_name }}` and `cancel-in-progress: false`.
- Do not reintroduce legacy workflow names such as `rust-ci.yml`, `build.yml`, `release-build.yml`, `docker-build.yml`, or `docker-release.yml` unless a package-specific workflow already exists and is intentionally preserved.
