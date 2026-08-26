# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Haruki Event Tracker is a Rust service that periodically scrapes ranking data from the Haruki Sekai API for the *Project Sekai* (プロジェクトセカイ) mobile game, persists it to a per-server SQL database, and exposes query endpoints (cloud/bot leaderboard queries, public web leaderboard APIs, WebSocket realtime updates, heartbeat status) for downstream clients such as HarukiBot and the public website.

The repo was rewritten from Go on `rewrite/rust`; `REWRITE_PLAN.md` is the frozen historical record of that rewrite (per-phase decisions, cutover verification, Go behaviour intentionally not ported). The Rust port took over production traffic on **2026-04-28 05:01:54Z** and all cutover follow-ups (GHCR image via `v2.0.0` tag, config migration) are long done.

**Status**: the project is now on the v3 line (latest tag `v3.3.0`; `Cargo.toml` carries the `3.0.0-dev` dev-version convention — real release versions come from tags). The v3 work added the `/api/v2` route surface (cloud + web), a two-tier API cache, WebSocket realtime push, UID anonymization for public web APIs, private (raw-UID) endpoints behind Toolbox ownership checks, and OpenDAL-backed config/master-data locations.

## Build & Run

- MSRV: Rust 1.88 (edition 2024).
- Build: `cargo build --release --bin haruki-event-tracker`. Release profile already enables `lto = "thin"`, `codegen-units = 1`, `strip = true`, `opt-level = 3`.
- Run: reads `haruki-tracker-configs.yaml` from the working directory by default; override with `--config <uri>` or `HARUKI_CONFIG_URI`. Config location and `master_data_dir` go through `storage.rs` (OpenDAL), so `file://`, `http(s)://`, and `s3://` URIs all work. Redis is required only when at least one tracker daemon is enabled — API-only deployments (`tracker.enabled: false` everywhere) skip Redis, the upstream Sekai API client, and the cron scheduler.
- Tests: `cargo test --lib` — ~100 unit tests in `#[cfg(test)]` modules across `api` (cache, access_log, ws_ticket, leaderboard services), `db` (query/web, schema, privacy), `tracker` (diff, parser, base), `storage`, and `privacy`. There is no `tests/` integration suite; HTTP/DB behaviour is validated against staging. `examples/perf_bench.rs` is a benchmark-style example target.
- Lint: `cargo clippy --all-targets -- -D warnings`. Keep clippy clean before committing — new warnings are treated as build failures.
- Docker: `docker build --build-arg VERSION=<ver> -t haruki-event-tracker .` (multi-stage `rust:1.98-alpine` builder → `alpine:3.24` runtime, non-root user, ~29 MB; dependabot bumps the builder tag). The image expects the config file mounted into `/app`. The builder pre-builds deps from a dummy `src/main.rs`; keep the `find src -name '*.rs' -exec touch {} +` line — Docker `COPY` preserves host mtimes and otherwise cargo skips the real rebuild.
- Tagged releases: pushing `v*` tags triggers `.github/workflows/release.yml` (targets: linux-x64, macos-arm64, windows-x64) and `.github/workflows/docker.yml` (GHCR push).
- Local Kubernetes smoke test: `scripts/smoke_k8s_orbstack.sh` (OrbStack; builds the image, runs API-only mode against a temp PostgreSQL, checks `/livez` + `/readyz`).

## Architecture

The process wires four long-lived subsystems together in `main.rs` → `app::build`:

1. **HTTP layer** (`src/api/`): `axum` 0.8 (with `ws`) + `tower-http` + `axum-server` for unified HTTP / HTTPS via `Handle::graceful_shutdown(10s)`. JSON in/out goes through `api::json` (sonic-rs; `Json<T>`, `RawJson`, `EncodedJson` for pre-gzipped cache hits). All routes are GET, built by `api::router::build_router`; the legacy `/event/{server}/{event_id}/...` prefix from the Go era is **gone**. Current surface:
   - `GET /livez` (process liveness) and `GET /readyz` (pings every DB, 503 on failure).
   - `GET /ws-ticket` + `GET /ws` — WebSocket realtime (see API-side services below).
   - **Cloud group** (bot clients, no auth): `/api/v2/cloud/events/{server}/{event_id}/leaderboards/total/sk/{query,check-room,line,speed,trace,status}` and `.../world-bloom/{character_id}/sk/{query,check-room,line,speed,trace}` → `handler::leaderboard::cloud` / `handler::status`.
   - **Web group** (public website): `/api/v2/web/events/{server}/{event_id}/leaderboards/total/{overview,replay/overview,details/rank/{rank},details/user/{user_id},users/search}` and the same set under `.../world-bloom/{character_id}/...` → `handler::leaderboard::web`. Query params: overviews take `interval`/`at`, details take `interval`/`at`/`includeTrace`/`includePlayerTrace`/`includeProfile`/`cursor`/`limit`.
   - **Private sub-group** (raw-UID lookups): `.../total/private/details/user/{user_id}` and `.../world-bloom/{character_id}/private/details/user/{user_id}`, guarded by `private::require_subject` (subject from the WS proxy extension or trusted-proxy Oathkeeper headers; 401 otherwise).
   - Middleware, outermost first: access log (`access_log::log` with `ProxyTrust`) → compression (gzip/br, level 4, bodies ≥1 KiB) → catch-panic.
   `api::extract::resolve_engine` parses `:server` against `AppState`'s per-server `Arc<DatabaseEngine>` map; an unknown server returns 400 via `api::error::ApiError::InvalidServer`.
2. **Per-server database engines** (`src/db/`): one `DatabaseEngine` per enabled `cfg.servers` entry. Backed by `sea-orm` 2.0.0-rc (pinned `sea-query` 1.0.0-rc) with MySQL / PostgreSQL / SQLite drivers, dialect chosen from `DbConfig.dialect`. **Tables are created dynamically per `(server, event_id)`** — `db::schema::create_event_tables` bootstraps the four table kinds (`TableKind::{TimeId, EventUsers, Event, WorldBloom}` → `event_<id>_time_id`, `event_<id>_users`, `event_<id>`, `wl_<id>`) plus web-read indexes, and `db::table_name::intern(TableKind, event_id)` returns the `&'static str` name used in `sea-query` aliases. When adding queries, route through `intern` rather than hardcoding names. Query modules: `batch` (write path), `ranking`, `lines`, `growth`, `heartbeat`, `user`, `world_bloom`, and `web` (cursor-paginated web searches/traces). `db::privacy::ensure_user_table_extensions` lazily migrates pre-existing `_users` tables (profile columns, `unique_id` backfill + index); it runs from tracker init and memoized from `AppState`.
3. **Tracker daemons** (`src/tracker/`): one `HarukiEventTracker` per server with `tracker.enabled: true`, scheduled by `tokio_cron_scheduler` (cron expression from config). The `use_second_level_cron: false` (5-field) form is auto-padded with a leading `"0 "` to match the crate's required 6-field schedule. The cron job uses `try_lock` and **skips** a tick if the previous one is still running. Each tick:
   - `EventDataParser::get_current_event_status` reads `events.json` / `worldBlooms.json` from `master_data_dir` and produces an `EventStatus` for the current wallclock.
   - `HarukiEventTracker::track_ranking_data` reinitialises the inner `EventTrackerBase` when the event id advances, short-circuits if the event is `aggregating` / `ended`, then calls `record_ranking_data`. After an event ends, `refresh_after_end` keeps re-fetching on `tracker.post_end_user_refresh_interval_secs` (default 3600) to record post-end corrections (banned-account cleanups, final border settlement) as new trace points and refresh user profiles.
   - `EventTrackerBase::handle_ranking_data` calls `HarukiSekaiAPIClient::get_top100` + `get_border`, hashes the border response (SHA-256), and uses `tracker::cache::detect_cache` (Redis hex-encoded match) to skip the merge step when nothing changed. Hex output uses `format!("{:02x}")` to stay byte-compatible with the Go-era fingerprints.
   - Diffing is **rank-based**: `tracker::diff::diff_rank_based` compares each rank's `(user_id, score)` against `prev_rank_state` and only persists rows that moved. State is mirrored to Redis under `haruki:tracker:<server>:<event>:{rank_state,ended}`. (Go also wrote a `user_state` hash but never read it back; that key is intentionally not ported.)
   - Writes: `db::query::batch::batch_upsert_event_users` runs *before* the transaction (keeps row locks short); `batch_insert_event_rankings` / `batch_insert_world_bloom_rankings` run inside it. On API failure or no-change ticks a heartbeat row is still written via `db::query::heartbeat::write_heartbeat` so the status endpoint reports freshness.
   - Every write is bracketed with API-cache invalidation (`begin_event_update` → work → `finish_event_update` epoch bump, or `abort_event_update` on error) and followed by a `RealtimeHub` `updated` broadcast.
4. **Bootstrap & shutdown** (`src/app.rs`, `src/shutdown.rs`): `app::build` returns an `AppContext { state, dbs, trackers, scheduler }`. `shutdown::signal()` resolves on SIGINT/SIGTERM (Ctrl+C on Windows); `shutdown::run` stops the scheduler, drops the trackers (which closes the shared Redis `ConnectionManager` handle), and `Arc::try_unwrap` + closes each `DatabaseEngine`.

### API-side services (`src/api/`)

- **`cache.rs`** — two-tier API cache: sharded in-process L1 + Redis L2 (own connection pool, Lua read script, per-event epoch/dirty control keys, single-flight, optional gzip precompression). Configured by the `api_cache` section; `AppState` holds it as `Option<ApiCache>`. Redis keys live under `haruki:tracker:<server>:<event>:api_cache:*` — Rust-only keyspace, no Go-compat constraint, but it shares the tracker prefix.
- **`limiter.rs`** — `ApiQueryLimiter`: global + per-server semaphores bounding trace-query concurrency (`api_query` config). This is admission control inside handlers, **not** per-IP rate limiting (none exists).
- **`realtime.rs` / `ws.rs` / `ws_ticket.rs`** — `RealtimeHub` broadcast channel with per-`(server, event_id)` topics and online counters. `GET /ws-ticket` resolves a subject from trusted-proxy (Oathkeeper) headers and issues a single-use 45 s ticket; `GET /ws?ticket=...` upgrades, then proxies JSON frames (`subscribe`/`unsubscribe`/`ping`/request) into the web router via `tower::ServiceExt::oneshot`, injecting the socket's subject as a `PrivateSubject` extension — that's how private endpoints become reachable over WS. Pushes `ready` / `updated` / `online` events for subscribed topics; trackers feed it via `notify_realtime_update`.
- **`private.rs` / `private_lookup.rs`** — `require_subject` middleware plus raw-UID detail handlers; `PrivateLookupVerifier` (from the `toolbox` config section) asks the Toolbox backend whether the subject owns a given `(server, user_id)` binding.
- **Privacy** (`src/privacy.rs` + `src/db/privacy.rs`) — `UidAnonymizer` (salted SHA-256) produces the public `unique_id`; `privacy.uid_anonymization.{enabled, salt}` in config (startup error if enabled with an empty salt). Public web APIs accept and return only `unique_id`; raw UID stays internal (see `WEB_API_CAPABILITIES.md`).
- **`access_log.rs`** — access-log middleware + `ProxyTrust` (trusted CIDRs, client-IP header, `access_log_sample_rate`, `access_log_slow_threshold_ms`); logs to `target = "access"`.
- **`stats.rs`** — global atomic cache/access/API counters with a periodic aggregation logger spawned from `main`.
- **`storage.rs`** (top-level) — OpenDAL wrapper (`StorageRoot` / `StorageFile`) behind config loading and `master_data_dir` reads.

### World Bloom specifics

World Bloom events have per-character chapters tracked in parallel. `EventTrackerBase` keeps `world_bloom_statuses` and `is_world_bloom_chapter_ended` maps; `HarukiEventTracker::handle_world_bloom` iterates *all* chapters each tick (overlap periods are intentional), and `handle_world_bloom_chapter` skips chapters that are `not_started`, `aggregating`, or already finalised. World Bloom rows are persisted via the separate `wl_<event_id>` table built by `intern(TableKind::WorldBloom, _)`.

### Models package

`src/model/` holds *all* shared types — API request/response schemas (`api.rs`), DB config (`db_config.rs`), domain enums (`enums.rs` — `SekaiServerRegion`, `SekaiEventType`, `SekaiEventStatus`, the `SEKAI_EVENT_RANKING_LINES_NORMAL` / `_WORLD_BLOOM` constants), event master data structs (`event.rs`), upstream Sekai API DTOs (`sekai.rs`), and tracker state structs (`tracker.rs`). `db` and `tracker` both depend on `model`; `model` depends on nothing internal — keep it that way to avoid cycles.

## Conventions

- **No `mod.rs`**: every module lives in `foo.rs` with optional siblings under `foo/`. `src/lib.rs` declares the top-level modules.
- **Redis key compat**: tracker keys under `haruki:tracker:<server>:<event>:{rank_state,ended}` are byte-compatible with the Go version and still hold live production state. Don't change suffixes, JSON field names, or hex casing. The `api_cache:*` keyspace under the same prefix is Rust-only and versioned by epoch — evolve it via the epoch/`control` mechanism in `api::cache`, not by renaming ad hoc.
- **PlayerState/RankState** use serde rename to single-letter keys (`s`/`r`/`u`) for the same Go wire-compat reason.
- **sonic-rs everywhere**: `sonic_rs::{from_str, from_slice, to_vec, to_string}`. `api::json` wraps it for handlers.
- Server identifiers in routes, configs, table names, Redis keys, and span fields are always the lowercase `model::enums::SekaiServerRegion` strings (`jp`/`en`/`tw`/`kr`/`cn`).
- **Dynamic table inserts** must go through `sea-query` (`Query::insert_into(Alias::new(intern(...)))`); SeaORM `ActiveModel` API can't be used because the Entity types carry a non-unit `table_name` field.
- **Privacy**: public web endpoints must only accept/return `unique_id`; never expose raw upstream UID or `twitterId` in web responses, logs, or cache keys. Raw-UID access goes through the private endpoints + Toolbox verification only.
- **Comments are sparse** — only when the *why* is non-obvious (cross-language wire compat, lifetime workarounds, Go-version dead code that's intentionally skipped). Don't add narrating comments.
- **TLS**: rustls 0.23 panics in `ServerConfig::builder()` when both `ring` and `aws_lc_rs` providers are reachable in the dep graph (they are, transitively). `main` calls `aws_lc_rs::default_provider().install_default()` once on the SSL branch — keep that line.
- Logging is `tracing` with the `GoStyleFormat` formatter (`[YYYY-MM-DD HH:MM:SS.mmm][LEVEL][target] message`). `target = "access"` is routed to `access_log_path`; everything else goes to `main_log_file` and stdout. File sinks strip ANSI; stdout keeps it.
- DSN form: sea-orm/sqlx wants URL form (`mysql://user:pwd@host:port/db?charset=utf8mb4`). The Go-style `user:pwd@tcp(host:port)/db?...` is **not** parsed. `parseTime` and `loc` are GORM-only and must be dropped.
- Config sections beyond the v2-era basics: `api_cache` (TTLs, pool, precompression), `api_query` (trace concurrency limits), `privacy.uid_anonymization`, `toolbox` (private-lookup backend), and the `backend` access-log knobs (`access_log_sample_rate`, `access_log_slow_threshold_ms`). Keep `haruki-tracker-configs.example.yaml` in sync when adding fields.

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
- `release.yml` is the standard release build entrypoint. It runs on `v*` tags and manual dispatch, builds release artifacts (linux-x64, macos-arm64, windows-x64), uploads them with `actions/upload-artifact`, and publishes GitHub Release assets on tag pushes.
- `docker.yml` is the standard Docker entrypoint. It runs on `main` pushes, `v*` tags, PRs that touch Docker/build inputs, and manual dispatch. PRs build only; non-PR runs push GHCR images with lowercase image names and Docker metadata tags.

Workflow maintenance rules:

- Keep workflow filenames and top-level names aligned: `CI`, `Release`, `Docker`, and optional package-specific names.
- Use `actions/checkout@v7`, `actions/upload-artifact@v7`, `actions/download-artifact@v8`, `softprops/action-gh-release@v3`, and current Docker actions (`setup-buildx@v4`, `login@v4`, `metadata@v6`, `build-push@v7`).
- Keep `permissions` minimal: `contents: read` for CI/Docker build-only work, `contents: write` for release publishing, and `packages: write` only when pushing container images.
- Use workflow `concurrency` keyed by workflow name and ref, with release jobs using `release-${{ github.ref_name }}` and `cancel-in-progress: false`.
- Do not reintroduce legacy workflow names such as `rust-ci.yml`, `build.yml`, `release-build.yml`, `docker-build.yml`, or `docker-release.yml` unless a package-specific workflow already exists and is intentionally preserved.
