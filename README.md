# Haruki Event Tracker

**Haruki Event Tracker** is a companion project for [HarukiBot](https://github.com/Team-Haruki), designed to track and record in-game ranking data and provide query APIs for clients.

## Requirements
+ `MySQL`, `SQLite`, `PostgreSQL` (Depending on your database choice)
+ `Redis` (Only required when tracker daemons are enabled)
+ Rust 1.88+ (only for building from source — releases ship pre-built binaries)

## How to Use
1. Go to the release page and download the archive for your platform (`haruki-event-tracker-linux-x64.tar.gz`, `haruki-event-tracker-macos-arm64.tar.gz` or `haruki-event-tracker-windows-x64.zip`); it contains the `haruki-event-tracker` binary
2. Rename `haruki-tracker-configs.example.yaml` to `haruki-tracker-configs.yaml` and then edit it. For more details, see the `haruki-tracker-configs.example.yaml` comments.
   The example ships `privacy.uid_anonymization.enabled: false`. The public web API (`/api/v2/web/...`) and the cluster
   `writer` role need `enabled: true` plus a long, random, secret `salt` (an empty salt with `enabled: true` fails at startup).
3. Make a new directory or use an exists directory
4. Put `haruki-event-tracker` and `haruki-tracker-configs.yaml` in the same directory
5. Open Terminal, and `cd` to the directory
6. Run `./haruki-event-tracker` (`haruki-event-tracker.exe` on Windows)

The Rust build reads `haruki-tracker-configs.yaml` from the current directory by
default. You can override it with `HARUKI_CONFIG_URI=/path/to/config.yaml` or
`./haruki-event-tracker --config /path/to/config.yaml`. Config and
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
