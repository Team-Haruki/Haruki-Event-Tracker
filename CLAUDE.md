# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Haruki Event Tracker is a Go service that periodically scrapes ranking data from the Haruki Sekai API for the *Project Sekai* (プロジェクトセカイ) mobile game, persists it to a per-server SQL database, and exposes query endpoints (latest rank, trace history, ranking lines, score-growth deltas, heartbeat status) for downstream clients such as HarukiBot.

## Build & Run

- Build: `go build -o haruki-event-tracker .` (release build adds `-ldflags "-s -w -X haruki-tracker/config.Version=<ver>" -trimpath -tags netgo`).
- Run: needs `haruki-tracker-configs.yaml` in the working directory (copy from `haruki-tracker-configs.example.yaml`); `config.init()` calls `os.Exit(1)` if it is missing or unparseable. Redis must be reachable before startup. Start with `./haruki-event-tracker` (or `go run .`).
- Docker: `docker build --build-arg VERSION=<ver> -t haruki-event-tracker .` — the image expects the config file to be mounted into `/app`.
- There is no test suite, no linter config, and no `go generate`. CI only builds release artifacts on `v*` tags (`.github/workflows/release.yml`, `docker.yml`).

## Architecture

The process wires four long-lived subsystems together in `main.go` → `api.InitAPIUtils`:

1. **HTTP layer** (`api/`): Fiber v3 app with sonic JSON, recovery and compression middleware. All routes are registered under `/event/:server/:event_id/...` by `api.RegisterRoutes`. `parseCommonParams` resolves `:server` to a `*gorm.DatabaseEngine` from the global `sekaiDBs` map keyed by `model.SekaiServerRegion`; an unknown server returns 400.
2. **Per-server database engines** (`utils/gorm/`): one `DatabaseEngine` per enabled server in `cfg.Servers`. Backed by GORM with MySQL / PostgreSQL / SQLite drivers selected by `GormConfig.Dialect`. **Tables are created dynamically per `(server, event_id)`** — `CreateEventTables` calls `AutoMigrate` against names produced by `GetTimeIDTableModel`, `GetEventUsersTableModel`, `GetEventTableModel`, `GetWorldBloomTableModel` (see `utils/gorm/tables.go`, which keeps a per-server cache of `Dynamic*Table` wrappers). When adding queries, route through these `Get*TableModel` helpers rather than hardcoding names.
3. **Tracker daemons** (`tracker/`): one `HarukiEventTracker` per server with `tracker.enabled: true`, scheduled by `gocron` (cron expression from config; second-level cron opt-in via `use_second_level_cron`). Each tick:
   - `EventDataParser.GetCurrentEventStatus()` reads local Sekai master data (`master_data_dir`) to determine the current event id, type and (for World Bloom) chapter statuses.
   - `HarukiEventTracker.TrackRankingData` reinitialises the inner `EventTrackerBase` when the event id advances, short-circuits if the event is `aggregating` / `ended`, then calls `RecordRankingData`.
   - `EventTrackerBase.handleRankingData` calls `HarukiSekaiAPIClient.GetTop100` + `GetBorder`, hashes the border response, and uses `detectCache` (Redis key per `(server,event,segment)`) to skip writes when nothing changed.
   - Diffing is **rank-based**: `diffRankBased` compares each rank's `(user_id, score)` against `prevRankState` and only persists rows that moved. State is mirrored to Redis under `haruki:tracker:<server>:<event>:{rank_state,user_state,ended}` so daemons survive restarts mid-event.
   - Writes go through `gorm.BatchInsertEventRankings` / `BatchInsertWorldBloomRankings`; if the API failed or no rows changed, a heartbeat row is still written via `gorm.WriteHeartbeat` so `/status` can report freshness.
4. **Shared singletons** (`api/utils.go`): `sekaiAPIClient`, `sekaiRedis`, `sekaiDBs`, `sekaiTrackerDaemons`, `sekaiScheduler`. `Shutdown` (called from `main` on SIGINT/SIGTERM after `app.ShutdownWithTimeout`) tears them down in order; add new resources to both functions.

### World Bloom specifics

World Bloom events have per-character chapters tracked in parallel. `EventTrackerBase` keeps `worldBloomStatuses` and `isWorldBloomChapterEnded` maps; `handleWorldBloom` iterates *all* chapters each tick (overlap periods are intentional), and `processWorldBloomChapter` skips chapters that are `not_started`, `aggregating`, or already finalised. World Bloom rows are written into the separate `WorldBloomTable` per event.

### Models package

`utils/model/` holds *all* shared types — API request/response schemas (`api.go`, `callapi.go`), GORM config (`gorm.go`), domain enums (`enums.go` — `SekaiServerRegion`, `SekaiEventType`, `SekaiEventStatus`, the `SekaiEventRankingLines{Normal,WorldBloom}` line lists used by `/ranking-lines`), and tracker state structs (`tracker.go`). The `gorm` and `tracker` packages both depend on `model`; `model` depends on nothing internal — keep it that way to avoid import cycles.

## Conventions

- Version string: injected at build time via `-X haruki-tracker/config.Version=...`; the source default is `2.0.0-dev`.
- Logging: always use `utils/logger.NewLogger(name, level, writer)` (wraps zap-style output with a per-component name). `nil` writer falls back to stdout. Tracker components name themselves `HarukiEventTracker<SERVER>...` — match that pattern for new daemons.
- Server identifiers in routes, configs, table names, Redis keys, and logger names are always the lowercase `model.SekaiServerRegion` constants (`jp`/`en`/`tw`/`kr`/`cn`). Don't introduce ad-hoc strings.
- Comments in code are sparse and the repo is bilingual (English code, Chinese in the example config and a few inline notes). Match the surrounding style rather than translating.
