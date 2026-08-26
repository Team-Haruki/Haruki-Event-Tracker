# AGENTS.md

Cross-agent guidance for Haruki Event Tracker. This file is the entry point for any AI coding agent (Codex, Cursor, Copilot, Claude Code, etc.) working in this repository. Claude Code has its own deeper file at `CLAUDE.md`; both files share the same conventions.

## What this is

Haruki Event Tracker scrapes ranking data from the Haruki Sekai API for *Project Sekai* (プロジェクトセカイ), persists it to a per-server SQL database, and exposes query APIs (cloud/bot leaderboard queries, public web leaderboard APIs, WebSocket realtime updates, heartbeat status) for downstream clients such as HarukiBot and the public website.

## Project state

- Active branch: `main`. The repo was rewritten from Go on `rewrite/rust` and **the Rust port took over production traffic at 2026-04-28 05:01:54Z**; `REWRITE_PLAN.md` is the frozen historical record of that rewrite (all phases `[x]`, cutover verification, rollback handle). All cutover follow-ups (GHCR image via `v2.0.0` tag, config migration) are done.
- The project is now on the v3 line (latest tag `v3.3.0`; `Cargo.toml` carries `3.0.0-dev` as the dev-version convention). v3 added the `/api/v2/{cloud,web}` route surface, a two-tier API cache, WebSocket realtime push, UID anonymization for public web APIs, private (raw-UID) endpoints behind Toolbox ownership checks, and OpenDAL-backed config/master-data locations. Web API surface details: `WEB_API_CAPABILITIES.md`.
- No `tests/` integration suite. `cargo test --lib` runs ~100 unit tests in `#[cfg(test)]` modules; HTTP/DB behaviour is validated against staging.

## Build & run

- MSRV: Rust 1.88 (edition 2024).
- Build: `cargo build --release --bin haruki-event-tracker`.
- Test: `cargo test --lib`.
- Lint: `cargo clippy --all-targets -- -D warnings` — keep clippy clean before committing.
- Run locally: reads `haruki-tracker-configs.yaml` from the working directory (override with `--config <uri>` or `HARUKI_CONFIG_URI`; OpenDAL `file://`/`http(s)://`/`s3://` URIs work). Redis is required only when a tracker daemon is enabled — API-only mode skips Redis, the Sekai API client, and the scheduler.
- Docker: `docker build --build-arg VERSION=<ver> -t haruki-event-tracker .` (`rust:1.98-alpine` builder → `alpine:3.24` runtime, ~29 MB image).

## Architecture pointers

The process wires four long-lived subsystems together in `main.rs` → `app::build`:

1. **HTTP** (`src/api/`) — `axum` 0.8 (with `ws`) + `tower-http`, JSON via sonic-rs. All routes are GET: `/livez`, `/readyz`, `/ws-ticket`, `/ws`, plus the cloud group `GET /api/v2/cloud/events/{server}/{event_id}/leaderboards/...` (bot clients) and the web group `GET /api/v2/web/events/{server}/{event_id}/leaderboards/...` (public website, `unique_id` only; private raw-UID sub-routes guarded by `private::require_subject`). The Go-era `/event/{server}/{event_id}/...` prefix no longer exists. Supporting services: two-tier API cache (`api/cache.rs`), trace-query concurrency limiter (`api/limiter.rs`), realtime hub + WebSocket proxy (`api/{realtime,ws,ws_ticket}.rs`), UID anonymizer (`src/privacy.rs`), access log with proxy trust (`api/access_log.rs`).
2. **Per-server DBs** (`src/db/`) — one `DatabaseEngine` per enabled server, sea-orm 2.0-rc with MySQL / Postgres / SQLite drivers. Tables are created dynamically per `(server, event_id)` and named through `db::table_name::intern(TableKind, event_id)` — never hardcode names.
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
