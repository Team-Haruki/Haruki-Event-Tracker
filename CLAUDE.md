# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Haruki Event Tracker is a Rust service that periodically scrapes ranking data from the Haruki Sekai API for the *Project Sekai* (プロジェクトセカイ) mobile game, persists it to a per-server SQL database, and exposes query endpoints (cloud/bot leaderboard queries, public web leaderboard APIs, WebSocket realtime updates, heartbeat status) for downstream clients such as HarukiBot and the public website.

The repo was rewritten from Go on `rewrite/rust`; `REWRITE_PLAN.md` is the frozen historical record of that rewrite (per-phase decisions, cutover verification, Go behaviour intentionally not ported). The Rust port took over production traffic on **2026-04-28 05:01:54Z** and all cutover follow-ups (GHCR image via `v2.0.0` tag, config migration) are long done.

**Status**: the project is now on the v4 line (`Cargo.toml` carries the latest released version, currently `4.2.0`; bump it before tagging). The v3 work added the `/api/v2` route surface (cloud + web), a two-tier API cache, WebSocket realtime push, UID anonymization for public web APIs, private (raw-UID) endpoints behind Toolbox ownership checks, and OpenDAL-backed config/master-data locations.

## Build & Run

- MSRV: Rust 1.88 (edition 2024).
- Build: `cargo build --release --bin haruki-event-tracker`. Release profile already enables `lto = "thin"`, `codegen-units = 1`, `strip = true`, `opt-level = 3`.
- Run: reads `haruki-tracker-configs.yaml` from the working directory by default; override with `--config <uri>` or `HARUKI_CONFIG_URI`. Config location and `master_data_dir` go through `storage.rs` (OpenDAL), so `file://`, `http(s)://`, and `s3://` URIs all work. Redis is required only when at least one tracker daemon is enabled — API-only deployments (`tracker.enabled: false` everywhere) skip Redis, the upstream Sekai API client, and the cron scheduler.
- Tests: `cargo test --lib` — ~100 unit tests in `#[cfg(test)]` modules across `api` (cache, access_log, ws_ticket, leaderboard services), `db` (query/web, schema, privacy), `tracker` (diff, parser, base), `storage`, and `privacy`. There is no `tests/` integration suite; HTTP/DB behaviour is validated against staging. `examples/perf_bench.rs` is a benchmark-style example target.
- Lint: `cargo clippy --all-targets -- -D warnings`. Keep clippy clean before committing — new warnings are treated as build failures.
- Docker: `docker build -t haruki-event-tracker .` (multi-stage: cargo-chef `lukemathwalker/cargo-chef:<ver>-rust-1.98-alpine` cooks the dependencies in their own cached layer, then `alpine:3.24` runtime, non-root user, ~29 MB; dependabot bumps the builder tag). The image expects the config file mounted into `/app`. The version comes from `Cargo.toml`; there is no version build arg.
- Tagged releases: bump `Cargo.toml`, merge, then push `v<version>`; `.github/workflows/release.yml` builds linux-x64 / macos-arm64 / windows-x64 and promotes the `main` image (see "GitHub Actions workflows").
- Local Kubernetes smoke test: `scripts/smoke_k8s_orbstack.sh` (OrbStack; builds the image, runs API-only mode against a temp PostgreSQL, checks `/livez` + `/readyz`).

## Architecture

The process wires four long-lived subsystems together in `main.rs` → `app::build`:

1. **HTTP layer** (`src/api/`): `axum` 0.8 (with `ws`) + `tower-http` + `axum-server` for unified HTTP / HTTPS via `Handle::graceful_shutdown(10s)`. JSON in/out goes through `api::json` (sonic-rs; `Json<T>`, `RawJson`, `EncodedJson` for pre-gzipped cache hits). All routes are GET, built by `api::router::build_router`; the legacy `/event/{server}/{event_id}/...` prefix from the Go era is **gone**. Current surface:
   - `GET /livez` (process liveness) and `GET /readyz` (pings every DB, 503 on failure).
   - `GET /ws-ticket` + `GET /ws` — WebSocket realtime (see API-side services below).
   - **Cloud group** (bot clients; bearer token from `cloud_api.tokens` via `api::cloud_auth`, open when the list is empty): `/api/v2/cloud/events/{server}/{event_id}/leaderboards/total/sk/{query,check-room,line,speed,trace,status}` and `.../world-bloom/{character_id}/sk/{query,check-room,line,speed,trace}` → `handler::leaderboard::cloud` / `handler::status`. Always speaks **raw** upstream UIDs (`ApiAudience::Cloud`), whatever the anonymization switch says.
   - **Web group** (public website; always `unique_id`, `ApiAudience::Web`, requires `privacy.uid_anonymization.enabled`): `/api/v2/web/events/{server}/{event_id}/leaderboards/total/{overview,replay/overview,details/rank/{rank},details/user/{user_id},users/search,check-room}` and the same set under `.../world-bloom/{character_id}/...` → `handler::leaderboard::web`. Query params: overviews take `interval`/`at`, details take `interval`/`at`/`includeTrace`/`includePlayerTrace`/`includeProfile`/`cursor`/`limit`. `check-room?userId=<raw uid>` and `details/user/{raw}?idType=uid` are the only web entry points that accept a raw UID; they resolve it to `unique_id`, serve the cached anonymized detail, then reveal the raw UID for the subject only (`subject` block + the subject's own rows).
   - **Cluster stream** (writer role only): `GET /internal/updates` (`api::cluster`, bearer `cluster.token`) streams `hello` / `updated` / `ping` JSON frames from the `UpdateBus`.
   - **Registry webhook** (any role that runs tracker daemons, when `cluster.token` is set): `POST /internal/master-updated` `{server, dataVersion}` (the master registry's `registry.subscribers` shape) drops the region's `EventDataParser` cache so the next tick re-reads `events.json` / `worldBlooms.json` immediately. Point `master_data_dir` at the registry's `files/` URL (`http://haruki-master-registry:9998/v1/master/<region>/files/`) — its `no-cache` + ETag answers are exactly the parser's stat fingerprint.
   - **Private sub-group** (raw-UID lookups): `.../total/private/details/user/{user_id}` and `.../world-bloom/{character_id}/private/details/user/{user_id}`, guarded by `private::require_subject` (subject from the WS proxy extension or trusted-proxy Oathkeeper headers; 401 otherwise).
   - Middleware, outermost first: catch-panic → access log (`access_log::log` with `ProxyTrust`) → web HTTP caching (`http_cache::web_cache_headers`, `/api/v2/web/` GETs only) → compression (gzip/br, level 4, bodies ≥1 KiB).
   `api::extract::resolve_engine` parses `:server` against `AppState`'s per-server `Arc<DatabaseEngine>` map; an unknown server returns 400 via `api::error::ApiError::InvalidServer`.
2. **Per-server database engines** (`src/db/`): one `DatabaseEngine` per enabled `cfg.servers` entry. Backed by `sea-orm` 2.0.0-rc (pinned `sea-query` 1.0.0-rc) with MySQL / PostgreSQL / SQLite drivers, dialect chosen from `DbConfig.dialect`. **Tables are created dynamically per `(server, event_id)`** — `db::schema::create_event_tables` bootstraps the four table kinds (`TableKind::{TimeId, EventUsers, Event, WorldBloom}` → `event_<id>_time_id`, `event_<id>_users`, `event_<id>`, `wl_<id>`) plus web-read indexes, and `db::table_name::intern(TableKind, event_id)` returns the `&'static str` name used in `sea-query` aliases. When adding queries, route through `intern` rather than hardcoding names. Query modules: `batch` (write path; on PostgreSQL one flush runs on a single owned connection through `db::pg_session` under the writer's `write_timeout`), `ranking`, `lines`, `growth`, `heartbeat`, `user`, `world_bloom`, and `web` (cursor-paginated web searches/traces). `db::privacy::ensure_user_table_extensions` lazily migrates pre-existing `_users` tables (profile columns, `unique_id` backfill + index); it runs from tracker init and memoized from `AppState`.
3. **Tracker daemons** (`src/tracker/`): one `HarukiEventTracker` per server with `tracker.enabled: true`, scheduled by `tokio_cron_scheduler` (cron expression from config). The `use_second_level_cron: false` (5-field) form is auto-padded with a leading `"0 "` to match the crate's required 6-field schedule. The cron job uses `try_lock` and **skips** a tick if the previous one is still running. Each tick:
   - `EventDataParser::get_current_event_status` reads `events.json` / `worldBlooms.json` from `master_data_dir` and produces an `EventStatus` for the current wallclock.
   - `HarukiEventTracker::track_ranking_data` reinitialises the inner `EventTrackerBase` when the event id advances, short-circuits if the event is `aggregating` / `ended`, then calls `record_ranking_data`. After an event ends, `refresh_after_end` keeps re-fetching on `tracker.post_end_user_refresh_interval_secs` (default 3600) to record post-end corrections (banned-account cleanups, final border settlement) as new trace points and refresh user profiles.
   - `EventTrackerBase::handle_ranking_data` calls `HarukiSekaiAPIClient::get_top100` + `get_border`, hashes the border response (SHA-256), and uses `tracker::cache::detect_cache` (Redis hex-encoded match) to skip the merge step when nothing changed. Hex output uses `format!("{:02x}")` to stay byte-compatible with the Go-era fingerprints.
   - Diffing is **rank-based**: `tracker::diff::diff_rank_based` compares each rank's `(user_id, score)` against `prev_rank_state` and only persists rows that moved. State is mirrored to Redis under `haruki:tracker:<server>:<event>:{rank_state,ended}`. (Go also wrote a `user_state` hash but never read it back; that key is intentionally not ported.)
   - Writes: `db::query::batch::batch_upsert_event_users` runs *before* the transaction (keeps row locks short); `batch_insert_event_rankings` / `batch_insert_world_bloom_rankings` run inside it. On API failure or no-change ticks a heartbeat row is still written via `db::query::heartbeat::write_heartbeat` so the status endpoint reports freshness.
   - Every write is bracketed with API-cache invalidation (`begin_event_update` → work → `finish_event_update` epoch bump, or `abort_event_update` on error) and followed by a `RealtimeHub` `updated` broadcast.
4. **Cluster roles** (`src/cluster.rs`, `cluster` config): `standalone` (default, everything in one process), `writer` (tracks, publishes each committed flush on the in-process `UpdateBus` with the primary's WAL LSN, serves only health + `/internal/updates`), `reader` (never tracks, `DatabaseEngine::is_read_only()` so `db::privacy` verifies columns instead of ALTERing — the DB may be a streaming replica — and `cluster::subscriber` dials the writer, waits for the LSN to replay (`replica_wait_ms`), bumps the local API-cache epoch and fires the realtime `updated`). `tracker::invalidation::CacheInvalidation` is the seam: `LocalRedis` for standalone, `Cluster(bus)` for writers. `/readyz` reports `role`, the reader's link status, and replication lag.
5. **Bootstrap & shutdown** (`src/app.rs`, `src/shutdown.rs`): `app::build` returns an `AppContext { state, dbs, trackers, scheduler }`. `shutdown::signal()` resolves on SIGINT/SIGTERM (Ctrl+C on Windows); `shutdown::run` stops the scheduler, drops the trackers (which closes the shared Redis `ConnectionManager` handle), and `Arc::try_unwrap` + closes each `DatabaseEngine`.

### API-side services (`src/api/`)

- **`cache.rs`** — two-tier API cache: sharded in-process L1 + Redis L2 (own connection pool, Lua read script, per-event epoch/dirty control keys, single-flight, optional gzip precompression). Configured by the `api_cache` section; `AppState` holds it as `Option<ApiCache>`. Redis keys live under `haruki:tracker:<server>:<event>:api_cache:*` — Rust-only keyspace, no Go-compat constraint, but it shares the tracker prefix.
- **`limiter.rs`** — `ApiQueryLimiter`: global + per-server semaphores bounding trace-query concurrency (`api_query` config). This is admission control inside handlers, **not** per-IP rate limiting (none exists).
- **`realtime.rs` / `ws.rs` / `ws_ticket.rs`** — `RealtimeHub` broadcast channel with per-`(server, event_id)` topics and online counters. `GET /ws-ticket` resolves a subject from trusted-proxy (Oathkeeper) headers and issues a single-use 45 s ticket; `GET /ws?ticket=...` upgrades, then proxies JSON frames (`subscribe`/`unsubscribe`/`ping`/request) into the web router via `tower::ServiceExt::oneshot`, injecting the socket's subject as a `PrivateSubject` extension — that's how private endpoints become reachable over WS. Pushes `ready` / `updated` / `online` events for subscribed topics; trackers feed it via `notify_realtime_update`.
- **`private.rs` / `private_lookup.rs`** — `require_subject` middleware plus raw-UID detail handlers; `PrivateLookupVerifier` (from the `toolbox` config section) asks the Toolbox backend whether the subject owns a given `(server, user_id)` binding.
- **Privacy** (`src/privacy.rs` + `src/db/privacy.rs`) — `UidAnonymizer` (salted SHA-256) produces the public `unique_id`; `privacy.uid_anonymization.{enabled, salt}` in config (startup error if enabled with an empty salt). Public web APIs accept and return only `unique_id`; raw UID stays internal (see `WEB_API_CAPABILITIES.md`).
- **`http_cache.rs`** — `ETag` (SHA-256 of the bytes sent) / `If-None-Match` → `304` / `Cache-Control` for the web group: `immutable` only when `v=<epoch>` equals the `ServedEpoch` the cache tagged the body with, `private, no-store` for `/private/` routes and raw-UID lookups (`http_cache::private`), short `public` lifetime otherwise. Only the epoch-pure overview parts (`top100`/`borders`/`growth`, windows anchored at #80's per-epoch rank cut via `build_overview_until`) set `ServedEpoch`; anything reading the wall clock or the heartbeat must not. Realtime `updated` pushes carry the epoch as `version` — on cluster readers only after the replica replayed the writer's LSN. Design and rollout: `docs/ws-vs-http-leaderboards.md`.
- **`access_log.rs`** — access-log middleware + `ProxyTrust` (trusted CIDRs, client-IP header, `access_log_sample_rate`, `access_log_slow_threshold_ms`); logs to `target = "access"`.
- **`stats.rs`** — global atomic cache/access/API counters with a periodic aggregation logger spawned from `main`.
- **`storage.rs`** (top-level) — OpenDAL wrapper (`StorageRoot` / `StorageFile`) behind config loading and `master_data_dir` reads.

### World Bloom specifics

World Bloom events have per-character chapters tracked in parallel. `EventTrackerBase` keeps `world_bloom_statuses` and `is_world_bloom_chapter_ended` maps; `HarukiEventTracker::handle_world_bloom` iterates *all* chapters each tick (overlap periods are intentional), and `handle_world_bloom_chapter` skips chapters that are `not_started`, `aggregating`, or already finalised. World Bloom rows are persisted via the separate `wl_<event_id>` table built by `intern(TableKind::WorldBloom, _)`.

### Models package

`src/model/` holds *all* shared types — API request/response schemas (`api.rs`), the columnar trace encoding behind `traceFormat=columns` (`trace_columns.rs` — `TraceColumns` codec and the `TracePayload` detail field that splices either wire form), DB config (`db_config.rs`), domain enums (`enums.rs` — `SekaiServerRegion`, `SekaiEventType`, `SekaiEventStatus`, the `SEKAI_EVENT_RANKING_LINES_NORMAL` / `_WORLD_BLOOM` constants), event master data structs (`event.rs`), upstream Sekai API DTOs (`sekai.rs`), and tracker state structs (`tracker.rs`). `db` and `tracker` both depend on `model`; `model` depends on nothing internal — keep it that way to avoid cycles.

## Conventions

- **No `mod.rs`**: every module lives in `foo.rs` with optional siblings under `foo/`. `src/lib.rs` declares the top-level modules.
- **Redis key compat**: tracker keys under `haruki:tracker:<server>:<event>:{rank_state,ended}` are byte-compatible with the Go version and still hold live production state. Don't change suffixes, JSON field names, or hex casing. The `api_cache:*` keyspace under the same prefix is Rust-only and versioned by epoch — evolve it via the epoch/`control` mechanism in `api::cache`, not by renaming ad hoc.
- **PlayerState/RankState** use serde rename to single-letter keys (`s`/`r`/`u`) for the same Go wire-compat reason.
- **sonic-rs everywhere**: `sonic_rs::{from_str, from_slice, to_vec, to_string}`. `api::json` wraps it for handlers.
- Server identifiers in routes, configs, table names, Redis keys, and span fields are always the lowercase `model::enums::SekaiServerRegion` strings (`jp`/`en`/`tw`/`kr`/`cn`).
- **Dynamic table inserts** must go through `sea-query` (`Query::insert_into(Alias::new(intern(...)))`); SeaORM `ActiveModel` API can't be used because the Entity types carry a non-unit `table_name` field.
- **Privacy**: public web endpoints must only accept/return `unique_id`; never expose raw upstream UID or `twitterId` in web responses, logs, or cache keys. The two exceptions are explicit opt-ins by the caller (web `check-room?userId=`, `details/user/{id}?idType=uid`) and reveal only the queried player; raw-UID access for *other* players goes through the private endpoints + Toolbox verification only. Cloud handlers always use `PublicUserIdMode::Raw` via `ApiAudience::Cloud`; cloud trace cache keys hash the subject.
- **Comments are sparse** — only when the *why* is non-obvious (cross-language wire compat, lifetime workarounds, Go-version dead code that's intentionally skipped). Don't add narrating comments.
- **TLS**: rustls 0.23 panics in `ServerConfig::builder()` when both `ring` and `aws_lc_rs` providers are reachable in the dep graph (they are, transitively). `main` calls `aws_lc_rs::default_provider().install_default()` once on the SSL branch — keep that line.
- Logging is `tracing` with the `GoStyleFormat` formatter (`[YYYY-MM-DD HH:MM:SS.mmm][LEVEL][target] message`). `target = "access"` is routed to `access_log_path`; everything else goes to `main_log_file` and stdout. File sinks strip ANSI; stdout keeps it.
- DSN form: sea-orm/sqlx wants URL form (`mysql://user:pwd@host:port/db?charset=utf8mb4`). The Go-style `user:pwd@tcp(host:port)/db?...` is **not** parsed. `parseTime` and `loc` are GORM-only and must be dropped.
- Config sections beyond the v2-era basics: `api_cache` (TTLs, pool, precompression), `api_query` (trace concurrency limits), `privacy.uid_anonymization`, `toolbox` (private-lookup backend), `cluster` (role, token, writer_url, replica_wait_ms), `cloud_api.tokens`, and the `backend` access-log knobs (`access_log_sample_rate`, `access_log_slow_threshold_ms`). Keep `haruki-tracker-configs.example.yaml` in sync when adding fields.

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
- The aggregate job **`CI OK`** is the only required status check.
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
