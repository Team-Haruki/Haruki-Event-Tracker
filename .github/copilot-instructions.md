# Copilot instructions

GitHub Copilot guidance for Haruki Event Tracker. Read this before suggesting edits.

## What this is

Rust service that scrapes ranking data from the Haruki Sekai API for *Project Sekai* (プロジェクトセカイ), persists it to a per-server SQL database, and exposes query APIs (cloud/bot leaderboard queries under `/api/v2/cloud/...`, public web leaderboard APIs under `/api/v2/web/...`, WebSocket realtime updates, heartbeat status) for downstream clients such as HarukiBot and the public website.

## Project state

The repo was rewritten from Go on `rewrite/rust`. The Rust port has been live in production since **2026-04-28 05:01:54Z** (5 servers — jp / en / tw / kr / cn). The project is now on the v4 line (`Cargo.toml` carries the latest released version): `/api/v2/{cloud,web}` routes, two-tier API cache, WebSocket realtime push, UID anonymization for public web APIs, private raw-UID endpoints behind Toolbox ownership checks. The rewrite record lives in `REWRITE_PLAN.md` (historical). Companion docs: `CLAUDE.md` (Claude Code, most detailed), `AGENTS.md` (cross-agent overview), `WEB_API_CAPABILITIES.md` (web API surface).

## Stack

- Rust 1.88, edition 2024.
- HTTP: `axum` 0.8 (with `ws`) + `tower-http` + `axum-server` (HTTP/HTTPS via the same handle, `aws_lc_rs` rustls provider).
- DB: `sea-orm` 2.0.0-rc + pinned `sea-query` 1.0.0-rc (MySQL / PostgreSQL / SQLite).
- JSON: **sonic-rs everywhere** (`api::json::Json<T>` wraps it for handlers).
- Async runtime: `tokio` 1.x with `tokio-cron-scheduler` for tracker ticks.
- Cache / state: `redis` 1.x with `ConnectionManager`; two-tier API cache (in-process L1 + Redis L2) in `api/cache.rs`.
- Storage: `opendal` for config / master-data locations (`file://`, `http(s)://`, `s3://`).
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
- Cross-version API parity sweep (historical, used during the Go cutover): `bash scripts/diff_go_vs_rust.sh`

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
