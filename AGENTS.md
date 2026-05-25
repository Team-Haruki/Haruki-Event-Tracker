# AGENTS.md

Cross-agent guidance for Haruki Event Tracker. This file is the entry point for any AI coding agent (Codex, Cursor, Copilot, Claude Code, etc.) working in this repository. Claude Code has its own deeper file at `CLAUDE.md`; both files share the same conventions.

## What this is

Haruki Event Tracker scrapes ranking data from the Haruki Sekai API for *Project Sekai* (プロジェクトセカイ), persists it to a per-server SQL database, and exposes a query API (latest rank, trace history, ranking lines, score-growth deltas, heartbeat status) for downstream clients such as HarukiBot.

## Project state

- Active branch: `rewrite/rust`. The repo was rewritten from Go on this branch and **the Rust port took over production traffic at 2026-04-28 05:01:54Z** (5 servers cut over simultaneously, Redis state read-through verified, no rollback triggered).
- Phase plan & per-item verification record: `REWRITE_PLAN.md` (Phase 0–8 all `[x]`, §6 行为对照清单 all `[x]`, §7 records the cutover artefacts and rollback handle).
- Open follow-ups (operational, non-code): tag `v2.0.0` so CI publishes the GHCR image, swap the prod compose back to the official tag, retire the legacy Go-style `haruki-tracker-configs.yaml`, drop the backup compose snapshot.
- No live integration test suite. `cargo test --lib` covers pure-function helpers; HTTP/DB parity is validated against staging via `scripts/diff_go_vs_rust.sh`.

## Build & run

- MSRV: Rust 1.88 (edition 2024).
- Build: `cargo build --release --bin haruki-event-tracker`.
- Test: `cargo test --lib`.
- Lint: `cargo clippy --all-targets -- -D warnings` — keep clippy clean before committing.
- Run locally: needs `haruki-tracker-configs.yaml` next to the binary, plus a reachable Redis. `cargo run --release` once the config is in place.
- Docker: `docker build --build-arg VERSION=<ver> -t haruki-event-tracker .` (Alpine-based, ~29 MB image).

## Architecture pointers

The process wires four long-lived subsystems together in `main.rs` → `app::build`:

1. **HTTP** (`src/api/`) — `axum` 0.8 + `tower-http`, JSON via sonic-rs. All routes are `GET /event/{server}/{event_id}/...`.
2. **Per-server DBs** (`src/db/`) — one `DatabaseEngine` per enabled server, sea-orm with MySQL / Postgres / SQLite drivers. Tables are created dynamically per `(server, event_id)` and named through `db::table_name::intern(TableKind, event_id)` — never hardcode names.
3. **Tracker daemons** (`src/tracker/`) — one per server, scheduled by `tokio_cron_scheduler`. Diffing is rank-based; only ranks whose `(user_id, score)` changed are persisted. State lives in Redis keys `haruki:tracker:<server>:<event>:{rank_state,ended}` — these are byte-compatible with the Go version.
4. **Bootstrap & shutdown** (`src/app.rs`, `src/shutdown.rs`).

For the full picture (World Bloom specifics, model layout, conventions on TLS, sonic-rs, dynamic table inserts), read `CLAUDE.md`.

## Conventions to follow when writing code

- **No `mod.rs`** — every module lives in `foo.rs` with optional siblings under `foo/`.
- **Comments are sparse** — only when the *why* is non-obvious (cross-language wire compat, lifetime workarounds, Go-version dead code intentionally skipped). Don't narrate.
- **Wire compatibility** with the Go version is load-bearing: Redis key suffixes, JSON field names, hex-encoded SHA-256 casing, `PlayerState/RankState` single-letter serde rename keys (`s` / `r` / `u`). Don't change without coordinating a hard cutover.
- **Server identifiers** are the lowercase `model::enums::SekaiServerRegion` strings (`jp` / `en` / `tw` / `kr` / `cn`) everywhere — routes, configs, table names, Redis keys, span fields.
- **Dynamic table inserts** must go through `sea-query` (`Query::insert_into(Alias::new(intern(...)))`); the SeaORM `ActiveModel` API doesn't work because Entity types carry a non-unit `table_name` field.
- **JSON** is sonic-rs everywhere (`sonic_rs::{from_str, from_slice, to_vec, to_string}`); `api::json::Json<T>` wraps it for handlers.
- **DSN form**: sqlx wants URL form (`mysql://user:pwd@host:port/db?charset=utf8mb4`). The Go-style `user:pwd@tcp(host:port)/db?...` is not accepted; `parseTime` and `loc` are GORM-only and must be dropped at cutover time.

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
  - Claude (any 4.x): `Co-authored-by: Claude Opus 4.7 <noreply@anthropic.com>` (substitute the actual model, e.g. `Claude Sonnet 4.6`, `Claude Haiku 4.5`)
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
- Use `actions/checkout@v6`, `actions/setup-go@v6`, `actions/upload-artifact@v7`, `actions/download-artifact@v8`, `softprops/action-gh-release@v3`, and current Docker actions (`setup-buildx@v4`, `login@v4`, `metadata@v6`, `build-push@v7`).
- Keep `permissions` minimal: `contents: read` for CI/Docker build-only work, `contents: write` for release publishing, and `packages: write` only when pushing container images.
- Use workflow `concurrency` keyed by workflow name and ref, with release jobs using `release-${{ github.ref_name }}` and `cancel-in-progress: false`.
- Do not reintroduce legacy workflow names such as `rust-ci.yml`, `build.yml`, `release-build.yml`, `docker-build.yml`, or `docker-release.yml` unless a package-specific workflow already exists and is intentionally preserved.
