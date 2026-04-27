# Copilot instructions

GitHub Copilot guidance for Haruki Event Tracker. Read this before suggesting edits.

## What this is

Rust service that scrapes ranking data from the Haruki Sekai API for *Project Sekai* (プロジェクトセカイ), persists it to a per-server SQL database, and exposes a query API (latest rank, trace, ranking lines, score growth, heartbeat) for downstream clients such as HarukiBot.

## Project state

The repo was rewritten from Go on `rewrite/rust`. The Rust port has been live in production since **2026-04-28 05:01:54Z** (5 servers — jp / en / tw / kr / cn — cut over simultaneously, Redis state read-through verified). Per-phase decisions and the cutover record live in `REWRITE_PLAN.md`. Companion docs: `CLAUDE.md` (Claude Code), `AGENTS.md` (cross-agent overview).

## Stack

- Rust 1.85, edition 2024.
- HTTP: `axum` 0.8 + `tower-http` + `axum-server` (HTTP/HTTPS via the same handle, `aws_lc_rs` rustls provider).
- DB: `sea-orm` 1.1 + `sea-query` 0.32 (MySQL / PostgreSQL / SQLite).
- JSON: **sonic-rs everywhere** (`api::json::Json<T>` wraps it for handlers).
- Async runtime: `tokio` 1.x with `tokio-cron-scheduler` for tracker ticks.
- Cache / state: `redis` 0.27 with `ConnectionManager`.
- Logging: `tracing` + a custom `GoStyleFormat` (`[YYYY-MM-DD HH:MM:SS.mmm][LEVEL][target] message`).

## Conventions

- **Module layout**: no `mod.rs`. Every module is `foo.rs` with optional siblings under `foo/`. `src/lib.rs` declares the top-level modules.
- **Comments are sparse**. Only document the *why* when it is non-obvious (cross-language wire compat, lifetime workarounds, Go-version dead code intentionally skipped). Do not narrate what the code does — names should suffice.
- **Wire compatibility with Go is load-bearing.** Redis keys (`haruki:tracker:<server>:<event>:{rank_state,ended}`), table names (`event_<id>`, `event_<id>_users`, `event_<id>_time_id`, `wl_<id>`), JSON field names, `PlayerState`/`RankState` single-letter serde rename keys (`s` / `r` / `u`), and lower-hex SHA-256 cache fingerprints all match the Go version byte-for-byte. Do not change them.
- **Server identifiers** are the lowercase `SekaiServerRegion` strings: `jp` / `en` / `tw` / `kr` / `cn`. Used uniformly in routes, configs, table names, Redis keys, span fields.
- **Dynamic table inserts** must go through `sea-query` (`Query::insert_into(Alias::new(intern(TableKind::*, event_id)))`). The SeaORM `ActiveModel` API does not work here — Entity types carry a non-unit `table_name` field.
- **Table naming** lives in `db::table_name::intern`. Never hardcode `event_<id>` or similar — route through `intern`.
- **DSN form** is sqlx URL: `mysql://user:pwd@host:port/db?charset=utf8mb4`. The Go-style `user:pwd@tcp(host:port)/db?...` form is **not** parsed. `parseTime` and `loc` are GORM-only and must be dropped.
- **TLS gotcha**: rustls 0.23 panics in `ServerConfig::builder()` when both `ring` and `aws_lc_rs` are reachable in the dep graph (they are, transitively). `main` calls `aws_lc_rs::default_provider().install_default()` exactly once on the SSL branch — keep that line.
- **Cron**: `use_second_level_cron: false` (5-field) is auto-padded with a leading `"0 "` to satisfy `tokio-cron-scheduler`'s 6-field requirement. Don't strip the pad.
- **Errors**: `thiserror` for typed errors at module boundaries, `anyhow` only at the top of `main` / handlers when nothing downstream cares. Don't add panics or `unwrap` outside of tests and obviously-infallible parsers.

## Build & test

- Build release: `cargo build --release --bin haruki-event-tracker`
- Unit tests: `cargo test --lib`
- Lint: `cargo clippy --all-targets -- -D warnings` (warnings treated as errors)
- Cross-version API parity sweep: `bash scripts/diff_go_vs_rust.sh` (needs the live Go and Rust endpoints reachable)

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
- **Agent attribution uses the standard Git `Co-authored-by:` trailer in the commit body, not a free-form `Agent:` line.** This makes GitHub render the co-author avatar on the commit page. The trailer must be on its own line, separated from the subject by a blank line, in the form `Co-authored-by: <Display Name> <email>`. For Copilot use:

  ```text
  Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>
  ```

  Other agents at work in this repo use `Co-authored-by: Claude Opus 4.7 <noreply@anthropic.com>` (substitute the model name) or `Co-authored-by: Codex <noreply@openai.com>`.

Examples from this repo's history:

```text
[Feat] Trust-proxy aware client IP in access log
[Fix] Master data parse + MySQL on-conflict syntax
[Chore] Add Go-vs-Rust API response diff harness
[Docs] Mark cutover complete in REWRITE_PLAN
```
