# AGENTS.md

Cross-agent guidance for Haruki Event Tracker. This file is the entry point for any AI coding agent (Codex, Cursor, Copilot, Claude Code, etc.) working in this repository. Claude Code has its own deeper file at `CLAUDE.md`; both files share the same conventions.

## What this is

Haruki Event Tracker scrapes ranking data from the Haruki Sekai API for *Project Sekai* (プロジェクトセカイ), persists it to a per-server SQL database, and exposes query APIs (cloud/bot leaderboard queries, public web leaderboard APIs, WebSocket realtime updates, heartbeat status) for downstream clients such as HarukiBot and the public website.

## Project state

- Active branch: `main`. The repo was rewritten from Go on `rewrite/rust` and **the Rust port took over production traffic at 2026-04-28 05:01:54Z**; `REWRITE_PLAN.md` is the frozen historical record of that rewrite (all phases `[x]`, cutover verification, rollback handle). All cutover follow-ups (GHCR image via `v2.0.0` tag, config migration) are done.
- The project is now on the v4 line (`Cargo.toml` carries the latest released version, currently `4.2.0`; bump it before tagging). v3 added the `/api/v2/{cloud,web}` route surface, a two-tier API cache, WebSocket realtime push, UID anonymization for public web APIs, private (raw-UID) endpoints behind Toolbox ownership checks, and OpenDAL-backed config/master-data locations. Web API surface details: `WEB_API_CAPABILITIES.md`.
- No `tests/` integration suite. `cargo test --lib` runs ~100 unit tests in `#[cfg(test)]` modules; HTTP/DB behaviour is validated against staging.

## Build & run

- MSRV: Rust 1.88 (edition 2024).
- Build: `cargo build --release --bin haruki-event-tracker`.
- Test: `cargo test --lib`.
- Lint: `cargo clippy --all-targets -- -D warnings` — keep clippy clean before committing.
- Run locally: reads `haruki-tracker-configs.yaml` from the working directory (override with `--config <uri>` or `HARUKI_CONFIG_URI`; OpenDAL `file://`/`http(s)://`/`s3://` URIs work). Redis is required only when a tracker daemon is enabled — API-only mode skips Redis, the Sekai API client, and the scheduler.
- Docker: `docker build -t haruki-event-tracker .` (cargo-chef on `rust:1.98-alpine` → `alpine:3.24` runtime, ~29 MB image). The version comes from `Cargo.toml`; there is no version build arg.

## Architecture pointers

The process wires four long-lived subsystems together in `main.rs` → `app::build`:

1. **HTTP** (`src/api/`) — `axum` 0.8 (with `ws`) + `tower-http`, JSON via sonic-rs. All routes are GET: `/livez`, `/readyz`, `/ws-ticket`, `/ws`, plus the cloud group `GET /api/v2/cloud/events/{server}/{event_id}/leaderboards/...` (bot clients) and the web group `GET /api/v2/web/events/{server}/{event_id}/leaderboards/...` (public website, `unique_id` only; private raw-UID sub-routes guarded by `private::require_subject`). The Go-era `/event/{server}/{event_id}/...` prefix no longer exists. Supporting services: two-tier API cache (`api/cache.rs`), web HTTP caching headers — ETag/304/Cache-Control keyed to the cache epoch (`api/http_cache.rs`), trace-query concurrency limiter (`api/limiter.rs`), realtime hub + WebSocket proxy (`api/{realtime,ws,ws_ticket}.rs`), UID anonymizer (`src/privacy.rs`), access log with proxy trust (`api/access_log.rs`).
2. **Per-server DBs** (`src/db/`) — one `DatabaseEngine` per enabled server, sea-orm 2.0-rc with MySQL / Postgres / SQLite drivers. Tables are created dynamically per `(server, event_id)` and named through `db::table_name::intern(TableKind, event_id)` — never hardcode names. **Invariant: `event_<id>_time_id.time_id` order == `timestamp` order.** Readers rely on it — per-key first/last rows (`lines`, `growth`, `web` rank windows, top-player growth) are per-key `ORDER BY time_id {ASC|DESC} LIMIT 1` probes on the `(rank, time_id)` / `(user_id_key, time_id)` indexes (`db::query::edge`), time windows are translated to `time_id` bounds through the time table's unique `timestamp` index (so no query filters by joining every history row to the time table), single-row latest lookups are `ORDER BY time_id DESC`. The writer maintains it by assigning `time_id = timestamp` to every new row (`db::entity::time_id::time_id_for_timestamp`, used by the batch inserts and `write_heartbeat`; the column's identity sequence is left unused). Legacy rows carry sequence ids; where a historical merge left them out of order, `haruki-event-tracker repair-time-ids --region <r> --event <id> [--dry-run]` (`db::repair`) renumbers the event to `time_id = timestamp` (time table + `event_<id>` + `wl_<id>`, one transaction, idempotent; PostgreSQL/SQLite). Finished events are frozen by hand with `haruki-event-tracker vacuum-finished-events` (`db::maintenance`, PostgreSQL primary only, never scheduled; see `docs/vacuum-finished-events.md`). **Rollback trap:** a binary predating this rule resumes the identity sequence, so on PostgreSQL its new rows get small ids below existing timestamp-valued ones (fresh inversions; readers return stale "latest"), and on SQLite/MySQL the auto-increment continues from `max+1` and the next same-second row from a new binary collides on the primary key — after this rule is deployed, don't run an older writer against the same tables. Traces (`db::query::trace`) read the ranking table alone once an event's time table is known to have `time_id == timestamp` on every row (checked once per process, cached per `(region, event)`), and are index-only scans on the covering indexes `idx_<id>_{rank_time,user_time}_cov` / `idx_<id>_wl_char_{rank,user}_time_cov` (PostgreSQL `INCLUDE`), which new events get at bootstrap; tables with history get them only through `haruki-event-tracker create-covering-indexes --region <r> [--event <id>] [--dry-run]` (`db::schema::create_covering_indexes`: `CREATE INDEX CONCURRENTLY`, one at a time, skips valid ones, rebuilds invalid leftovers; the plain indexes stay). On PostgreSQL, key sets go in as one `bigint[]` parameter (`db::query::keys`: `= ANY($1)`, `unnest($1)`) so statement text does not vary with the key count.
3. **Tracker daemons** (`src/tracker/`) — one per server, scheduled by `tokio_cron_scheduler`. Diffing is rank-based; only ranks whose `(user_id, score)` changed are persisted. State lives in Redis keys `haruki:tracker:<server>:<event>:{rank_state,ended}` — these are byte-compatible with the Go version and still hold live production state. After an event ends the tracker keeps refreshing on an interval to record post-end ranking corrections.
4. **Bootstrap & shutdown** (`src/app.rs`, `src/shutdown.rs`).

For the full picture (route table, API-side services, World Bloom specifics, model layout, conventions on TLS, sonic-rs, dynamic table inserts), read `CLAUDE.md`.

## Conventions to follow when writing code

- **No `mod.rs`** — every module lives in `foo.rs` with optional siblings under `foo/`.
- **Comments are sparse** — only when the *why* is non-obvious (cross-language wire compat, lifetime workarounds, Go-version dead code intentionally skipped). Don't narrate.
- **Wire compatibility** with the Go version is load-bearing: Redis key suffixes, JSON field names, hex-encoded SHA-256 casing, `PlayerState/RankState` single-letter serde rename keys (`s` / `r` / `u`). Don't change without coordinating a hard cutover.
- **Server identifiers** are the lowercase `model::enums::SekaiServerRegion` strings (`jp` / `en` / `tw` / `kr` / `cn`) everywhere — routes, configs, table names, Redis keys, span fields.
- **Dynamic table inserts** must go through `sea-query` (`Query::insert_into(Alias::new(intern(...)))`); the SeaORM `ActiveModel` API doesn't work because Entity types carry a non-unit `table_name` field.
- **JSON** is sonic-rs everywhere (`sonic_rs::{from_str, from_slice, to_vec, to_string}`); `api::json::Json<T>` wraps it for handlers.
- **Privacy**: public web endpoints only accept/return the anonymized `unique_id` — never raw upstream UID or `twitterId` in web responses, logs, or cache keys. Raw-UID access goes through the private endpoints + Toolbox verification.
- **DSN form**: sqlx wants URL form (`mysql://user:pwd@host:port/db?charset=utf8mb4`). The Go-style `user:pwd@tcp(host:port)/db?...` is not accepted; `parseTime` and `loc` are GORM-only and must be dropped.

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
