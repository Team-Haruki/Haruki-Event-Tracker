# Haruki Event Tracker

**Haruki Event Tracker** is a companion project for [HarukiBot](https://github.com/Team-Haruki), designed to track and record in-game ranking data and provide query APIs for clients.

## Requirements
+ `MySQL`, `SQLite`, `PostgreSQL` (Depending on your database choice)
+ `Redis` (Only required when tracker daemons are enabled)
+ Rust 1.88+ (only for building from source — releases ship pre-built binaries)

## How to Use
1. Go to release page to download `HarukiEventTracker`
2. Rename `haruki-tracker-configs.example.yaml` to `haruki-tracker-configs.yaml` and then edit it. For more details, see the `haruki-tracker-configs.example.yaml` comments.
3. Make a new directory or use an exists directory
4. Put `HarukiEventTracker` and `haruki-tracker-configs.yaml` in the same directory
5. Open Terminal, and `cd` to the directory
6. Run `HarukiEventTracker`

The Rust build reads `haruki-tracker-configs.yaml` from the current directory by
default. You can override it with `HARUKI_CONFIG_URI=/path/to/config.yaml` or
`./HarukiEventTracker --config /path/to/config.yaml`. Config and
`master_data_dir` can also point at OpenDAL locations such as `file:///app/config`
or `s3://bucket/path/master?region=ap-northeast-1`.

For API-only deployments, keep each enabled server configured with its database
and set `servers.<region>.tracker.enabled: false`. In that mode the process
skips Redis, the upstream Sekai API client, and the cron scheduler.

Health endpoints:

- `GET /livez` — process liveness.
- `GET /readyz` — pings all configured databases and returns 503 if any ping fails.

For a local OrbStack Kubernetes smoke test, run
`scripts/smoke_k8s_orbstack.sh`. It builds `haruki-event-tracker:local`, starts
a temporary PostgreSQL deployment, runs API-only mode, and checks `/livez` plus
`/readyz`.

## License

This project is licensed under the MIT License.
