use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use axum_server::Handle;
use axum_server::accept::NoDelayAcceptor;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};

use haruki_event_tracker::db::engine::{DatabaseEngine, EngineRole};
use haruki_event_tracker::db::maintenance;
use haruki_event_tracker::db::repair::repair_time_ids;
use haruki_event_tracker::db::schema::{CoveringIndexAction, create_covering_indexes};
use haruki_event_tracker::model::enums::SekaiServerRegion;
use haruki_event_tracker::tracker::parser::EventDataParser;
use haruki_event_tracker::{api, app, config, logger, shutdown};

/// The release image is a musl build, whose malloc serialises allocations
/// across threads: under concurrent load, decoding and encoding large cached
/// payloads spent most of its CPU contending on the allocator lock.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some(REPAIR_TIME_IDS) => return repair_time_ids_cli(std::env::args().skip(2)).await,
        Some(VACUUM_FINISHED_EVENTS) => {
            return vacuum_finished_events_cli(std::env::args().skip(2)).await;
        }
        Some(CREATE_COVERING_INDEXES) => {
            return create_covering_indexes_cli(std::env::args().skip(2)).await;
        }
        _ => {}
    }
    let cfg_location = config::config_location_from_args_env();
    let cfg = match config::load_from_location(&cfg_location).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("failed to load {cfg_location}: {err}");
            return ExitCode::from(1);
        }
    };

    let log_file =
        (!cfg.backend.main_log_file.is_empty()).then(|| cfg.backend.main_log_file.clone());
    let access_log_file =
        (!cfg.backend.access_log_path.is_empty()).then(|| cfg.backend.access_log_path.clone());
    // Keeps the non-blocking log worker threads alive; dropped on exit so
    // buffered lines flush.
    let _log_guards = match logger::init(
        &cfg.backend.log_level,
        log_file.as_deref(),
        access_log_file.as_deref(),
    ) {
        Ok(guards) => guards,
        Err(err) => {
            eprintln!("failed to init logger: {err}");
            return ExitCode::from(1);
        }
    };

    tracing::info!(
        "========================= Haruki Event Tracker v{} =========================",
        env!("CARGO_PKG_VERSION")
    );
    tracing::info!("Powered by Haruki Dev Team");
    log_open_file_limit();
    haruki_event_tracker::api::stats::spawn_aggregation_logger();

    let ctx = match app::build(&cfg).await {
        Ok(ctx) => ctx,
        Err(err) => {
            tracing::error!(%err, "bootstrap failed");
            return ExitCode::from(1);
        }
    };

    let (trust, bad_cidrs) = haruki_event_tracker::api::access_log::ProxyTrust::from_config(
        cfg.backend.enable_trust_proxy,
        &cfg.backend.trusted_proxies,
        &cfg.backend.proxy_header,
        cfg.backend.access_log_sample_rate,
        cfg.backend.access_log_slow_threshold_ms,
    );
    for raw in &bad_cidrs {
        tracing::warn!(cidr = %raw, "ignored unparseable trusted_proxies entry");
    }
    if cfg.backend.enable_trust_proxy {
        tracing::info!(
            entries = trust.trusted.len(),
            header = %trust.primary_header,
            "trust proxy enabled"
        );
    }
    let router = api::router::build_router(ctx.state.clone(), std::sync::Arc::new(trust));
    let bind_target = format!("{}:{}", cfg.backend.host, cfg.backend.port);
    let addr = match resolve_addr(&bind_target).await {
        Ok(a) => a,
        Err(()) => return ExitCode::from(1),
    };

    let handle = Handle::new();
    tokio::spawn({
        let handle = handle.clone();
        async move {
            shutdown::signal().await;
            tracing::info!(
                grace_secs = SHUTDOWN_GRACE.as_secs(),
                "starting graceful shutdown"
            );
            handle.graceful_shutdown(Some(SHUTDOWN_GRACE));
        }
    });

    let make_service = router.into_make_service_with_connect_info::<SocketAddr>();
    let serve_result = if cfg.backend.ssl {
        // rustls 0.23 panics in `ServerConfig::builder()` when both `ring`
        // and `aws_lc_rs` are present in the dep graph (they are, transitively)
        // unless a default provider is explicitly installed. `install_default`
        // returns Err if one was already set — harmless in either case.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let tls =
            match RustlsConfig::from_pem_file(&cfg.backend.ssl_cert, &cfg.backend.ssl_key).await {
                Ok(c) => c,
                Err(err) => {
                    tracing::error!(
                        %err,
                        cert = %cfg.backend.ssl_cert,
                        key = %cfg.backend.ssl_key,
                        "failed to load TLS cert/key"
                    );
                    return ExitCode::from(1);
                }
            };
        tracing::info!(addr = %addr, cert = %cfg.backend.ssl_cert, "HTTPS server listening");
        axum_server::bind(addr)
            .acceptor(RustlsAcceptor::new(tls).acceptor(NoDelayAcceptor::new()))
            .handle(handle)
            .serve(make_service)
            .await
    } else {
        tracing::info!(addr = %addr, "HTTP server listening");
        axum_server::bind(addr)
            .acceptor(NoDelayAcceptor::new())
            .handle(handle)
            .serve(make_service)
            .await
    };

    if let Err(err) = serve_result {
        tracing::error!(%err, "server error");
    }

    shutdown::run(ctx.scheduler, ctx.trackers, ctx.dbs, ctx.state).await;
    tracing::info!("bye");
    ExitCode::SUCCESS
}

const REPAIR_TIME_IDS: &str = "repair-time-ids";
const REPAIR_USAGE: &str = "usage: haruki-event-tracker repair-time-ids --region <jp|en|tw|kr|cn> --event <id> [--dry-run] [--config <uri>]";
const CREATE_COVERING_INDEXES: &str = "create-covering-indexes";
const COVERING_USAGE: &str = "usage: haruki-event-tracker create-covering-indexes --region <jp|en|tw|kr|cn> [--event <id>] [--dry-run] [--config <uri>]";

/// The arguments the maintenance subcommands share.
struct OpsArgs {
    region: SekaiServerRegion,
    event_id: Option<i64>,
    dry_run: bool,
    cfg_location: String,
}

fn parse_ops_args(args: impl Iterator<Item = String>, usage: &str) -> Result<OpsArgs, ExitCode> {
    let mut region = None;
    let mut event_id = None;
    let mut dry_run = false;
    let mut cfg_location = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) => (k.to_owned(), Some(v.to_owned())),
            None => (arg, None),
        };
        let mut value = || inline.clone().or_else(|| args.next());
        match key.as_str() {
            "--region" => region = value().and_then(|v| SekaiServerRegion::parse(&v)),
            "--event" => event_id = value().and_then(|v| v.parse::<i64>().ok()),
            "--config" => cfg_location = value(),
            "--dry-run" => dry_run = true,
            _ => {
                eprintln!("unknown argument {key}\n{usage}");
                return Err(ExitCode::from(2));
            }
        }
    }
    let Some(region) = region else {
        eprintln!("{usage}");
        return Err(ExitCode::from(2));
    };
    Ok(OpsArgs {
        region,
        event_id,
        dry_run,
        cfg_location: cfg_location.unwrap_or_else(config::config_location_from_env),
    })
}

/// The region's configured database, connected.
async fn connect_region(
    region: SekaiServerRegion,
    cfg_location: &str,
) -> Result<DatabaseEngine, ExitCode> {
    let cfg = match config::load_from_location(cfg_location).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("failed to load {cfg_location}: {err}");
            return Err(ExitCode::from(1));
        }
    };
    let Some(server_cfg) = cfg.servers.get(&region) else {
        eprintln!("region {region} is not configured in {cfg_location}");
        return Err(ExitCode::from(1));
    };
    DatabaseEngine::connect(&server_cfg.db, EngineRole::Serving)
        .await
        .map_err(|err| {
            eprintln!("failed to connect {region} database: {err}");
            ExitCode::from(1)
        })
}

/// `repair-time-ids`: restore `time_id` order == `timestamp` order for one
/// event (see `db::repair`). Runs against the region's configured DB, in
/// one transaction; `--dry-run` only reports.
async fn repair_time_ids_cli(args: impl Iterator<Item = String>) -> ExitCode {
    let args = match parse_ops_args(args, REPAIR_USAGE) {
        Ok(args) => args,
        Err(code) => return code,
    };
    let (region, dry_run) = (args.region, args.dry_run);
    let Some(event_id) = args.event_id else {
        eprintln!("{REPAIR_USAGE}");
        return ExitCode::from(2);
    };
    let engine = match connect_region(region, &args.cfg_location).await {
        Ok(engine) => engine,
        Err(code) => return code,
    };
    let result = repair_time_ids(&engine, event_id, dry_run).await;
    let _ = engine.close().await;
    match result {
        Ok(report) => {
            println!(
                "{region} event {event_id}: inversions={} drifted_time_rows={} \
                 orphan_ranking_rows={} orphan_world_bloom_rows={}",
                report.inversions,
                report.drifted_time_rows,
                report.orphan_ranking_rows,
                report.orphan_world_bloom_rows
            );
            if report.applied {
                println!(
                    "renumbered: time_rows={} ranking_rows={} world_bloom_rows={}",
                    report.renumbered_time_rows,
                    report.renumbered_ranking_rows,
                    report.renumbered_world_bloom_rows
                );
            } else if dry_run && report.inversions > 0 {
                println!("dry run: nothing changed");
            } else {
                println!("no inversions: nothing to do");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("repair failed: {err}");
            ExitCode::from(1)
        }
    }
}

const VACUUM_FINISHED_EVENTS: &str = "vacuum-finished-events";
const VACUUM_USAGE: &str = "usage: haruki-event-tracker vacuum-finished-events [--region <jp|en|tw|kr|cn>] \
[--event <id>] [--dry-run] [--max-events <n>] [--min-idle-days <days>] [--pause-secs <secs>] \
[--min-xid-age <n>] [--idle-only] [--config <uri>]";

/// `vacuum-finished-events`: `VACUUM (FREEZE, ANALYZE)` every table of
/// events that are over, one event at a time (see `db::maintenance`).
/// PostgreSQL primaries only; never scheduled by the daemon.
async fn vacuum_finished_events_cli(args: impl Iterator<Item = String>) -> ExitCode {
    let mut region = None;
    let mut only_event = None;
    let mut dry_run = false;
    let mut max_events = usize::MAX;
    let mut min_idle_days = 3u64;
    let mut pause = Duration::from_secs(5);
    let mut min_xid_age = 0i64;
    let mut idle_only = false;
    let mut cfg_location = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) => (k.to_owned(), Some(v.to_owned())),
            None => (arg, None),
        };
        let mut value = || inline.clone().or_else(|| args.next());
        let parsed = match key.as_str() {
            "--region" => {
                region = value().and_then(|v| SekaiServerRegion::parse(&v));
                region.is_some()
            }
            "--event" => {
                only_event = value().and_then(|v| v.parse::<i64>().ok());
                only_event.is_some()
            }
            "--max-events" => match value().and_then(|v| v.parse::<usize>().ok()) {
                Some(n) if n > 0 => {
                    max_events = n;
                    true
                }
                _ => false,
            },
            "--min-idle-days" => match value().and_then(|v| v.parse::<u64>().ok()) {
                Some(d) => {
                    min_idle_days = d;
                    true
                }
                None => false,
            },
            "--pause-secs" => match value().and_then(|v| v.parse::<u64>().ok()) {
                Some(s) => {
                    pause = Duration::from_secs(s);
                    true
                }
                None => false,
            },
            "--min-xid-age" => match value().and_then(|v| v.parse::<i64>().ok()) {
                Some(n) => {
                    min_xid_age = n;
                    true
                }
                None => false,
            },
            "--config" => {
                cfg_location = value();
                cfg_location.is_some()
            }
            "--dry-run" => {
                dry_run = true;
                true
            }
            "--idle-only" => {
                idle_only = true;
                true
            }
            _ => {
                eprintln!("unknown argument {key}\n{VACUUM_USAGE}");
                return ExitCode::from(2);
            }
        };
        if !parsed {
            eprintln!("bad value for {key}\n{VACUUM_USAGE}");
            return ExitCode::from(2);
        }
    }
    let cfg_location = cfg_location.unwrap_or_else(config::config_location_from_env);
    let cfg = match config::load_from_location(&cfg_location).await {
        Ok(c) => c,
        Err(err) => {
            eprintln!("failed to load {cfg_location}: {err}");
            return ExitCode::from(1);
        }
    };
    let mut regions: Vec<SekaiServerRegion> = match region {
        Some(region) => vec![region],
        None => cfg
            .servers
            .iter()
            .filter(|(_, server)| server.enabled)
            .map(|(region, _)| *region)
            .collect(),
    };
    regions.sort_by_key(|region| region.to_string());
    if regions.is_empty() {
        eprintln!("no enabled region in {cfg_location}");
        return ExitCode::from(1);
    }

    let min_idle = Duration::from_secs(min_idle_days * 86_400);
    let mut budget = max_events;
    let mut failed = false;
    for region in regions {
        let Some(server_cfg) = cfg.servers.get(&region) else {
            eprintln!("region {region} is not configured in {cfg_location}");
            return ExitCode::from(1);
        };
        if budget == 0 {
            println!("{region}: --max-events reached, skipping");
            continue;
        }
        let closed_at_by_event = if idle_only {
            None
        } else {
            let parser = EventDataParser::new(region, &server_cfg.master_data_dir)
                .map_err(|err| err.to_string());
            let events = match parser {
                Ok(parser) => parser
                    .load_event_data()
                    .await
                    .map_err(|err| err.to_string()),
                Err(err) => Err(err),
            };
            match events {
                Ok(events) => Some(
                    events
                        .iter()
                        .map(|event| (event.id, event.closed_at))
                        .collect::<HashMap<i64, i64>>(),
                ),
                Err(err) => {
                    eprintln!(
                        "{region}: cannot read master data ({err}); pass --idle-only to select \
                         events by table idleness alone"
                    );
                    return ExitCode::from(1);
                }
            }
        };
        let engine = match DatabaseEngine::connect(&server_cfg.db, EngineRole::Serving).await {
            Ok(engine) => engine,
            Err(err) => {
                eprintln!("{region}: failed to connect database: {err}");
                return ExitCode::from(1);
            }
        };
        let outcome = vacuum_region(
            region,
            &engine,
            closed_at_by_event.as_ref(),
            only_event,
            dry_run,
            &mut budget,
            min_idle,
            pause,
            min_xid_age,
        )
        .await;
        let _ = engine.close().await;
        match outcome {
            Ok(()) => {}
            Err(err) if engine_is_not_postgres(&err) => println!("{region}: {err}"),
            Err(err) => {
                eprintln!("{region}: {err}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn engine_is_not_postgres(err: &sea_orm::DbErr) -> bool {
    err.to_string().contains("PostgreSQL only")
}

#[allow(clippy::too_many_arguments)]
async fn vacuum_region(
    region: SekaiServerRegion,
    engine: &DatabaseEngine,
    closed_at_by_event: Option<&HashMap<i64, i64>>,
    only_event: Option<i64>,
    dry_run: bool,
    budget: &mut usize,
    min_idle: Duration,
    pause: Duration,
    min_xid_age: i64,
) -> Result<(), sea_orm::DbErr> {
    maintenance::check_vacuum_target(engine).await?;
    let now = chrono::Utc::now().timestamp();
    let mut candidates = Vec::new();
    let mut skipped_unknown = 0usize;
    for event_id in maintenance::list_event_ids(engine).await? {
        if only_event.is_some_and(|only| only != event_id) {
            continue;
        }
        let closed_at_ms = match closed_at_by_event {
            Some(map) => match map.get(&event_id) {
                Some(closed_at) => Some(*closed_at),
                None => {
                    skipped_unknown += 1;
                    continue;
                }
            },
            None => None,
        };
        let last_sample_at = maintenance::last_sample_at(engine, event_id).await?;
        let Some(reason) =
            maintenance::finished_reason(now, closed_at_ms, last_sample_at, min_idle)
        else {
            continue;
        };
        candidates.push(maintenance::FinishedEvent {
            event_id,
            reason,
            last_sample_at,
            tables: maintenance::event_tables(engine, event_id).await?,
        });
    }
    println!(
        "{region}: {} finished event(s) selected, {skipped_unknown} without a master entry skipped{}",
        candidates.len(),
        if dry_run { " (dry run)" } else { "" }
    );
    for event in candidates {
        if *budget == 0 {
            println!("{region}: --max-events reached, stopping");
            break;
        }
        *budget -= 1;
        let reason = match event.reason {
            maintenance::FinishedReason::ClosedAt(at) => format!("closed_at={at}"),
            maintenance::FinishedReason::Idle => "idle".to_owned(),
        };
        println!(
            "{region} event {}: {reason} last_sample_at={} tables={}",
            event.event_id,
            event
                .last_sample_at
                .map_or_else(|| "none".to_owned(), |ts| ts.to_string()),
            event.tables.join(",")
        );
        for (i, table) in event.tables.iter().enumerate() {
            let before = maintenance::table_stats(engine, table).await?;
            if before.xid_age < min_xid_age {
                println!(
                    "  {table}: xid_age={} < {min_xid_age}, skipped",
                    before.xid_age
                );
                continue;
            }
            if dry_run {
                println!(
                    "  {table}: xid_age={} size={} (would VACUUM (FREEZE, ANALYZE))",
                    before.xid_age,
                    human_bytes(before.total_bytes)
                );
                continue;
            }
            if i > 0 && !pause.is_zero() {
                tokio::time::sleep(pause).await;
            }
            let done = maintenance::vacuum_freeze_analyze(engine, table).await?;
            println!(
                "  {table}: xid_age {}->{} size {}->{} in {:.1?}",
                done.before.xid_age,
                done.after.xid_age,
                human_bytes(done.before.total_bytes),
                human_bytes(done.after.total_bytes),
                done.elapsed
            );
        }
        if !dry_run && *budget > 0 && !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
    }
    Ok(())
}

fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// `create-covering-indexes`: build the trace covering indexes
/// (`db::schema::covering_indexes`) of one event, or of every event in the
/// region's database, with `CREATE INDEX CONCURRENTLY`, one at a time.
/// Existing valid indexes are skipped; `--dry-run` only reports.
/// PostgreSQL only.
async fn create_covering_indexes_cli(args: impl Iterator<Item = String>) -> ExitCode {
    let args = match parse_ops_args(args, COVERING_USAGE) {
        Ok(args) => args,
        Err(code) => return code,
    };
    let engine = match connect_region(args.region, &args.cfg_location).await {
        Ok(engine) => engine,
        Err(code) => return code,
    };
    let result = create_covering_indexes(&engine, args.event_id, args.dry_run).await;
    let _ = engine.close().await;
    match result {
        Ok(reports) => {
            for report in &reports {
                let action = match (args.dry_run, report.action) {
                    (_, CoveringIndexAction::Skipped) => "exists, skipped",
                    (true, CoveringIndexAction::Created) => "would create",
                    (true, CoveringIndexAction::Rebuilt) => "would drop invalid and rebuild",
                    (false, CoveringIndexAction::Created) => "created",
                    (false, CoveringIndexAction::Rebuilt) => "dropped invalid and rebuilt",
                };
                let size = report
                    .size_bytes
                    .map(|bytes| format!(" {:.1} MB", bytes as f64 / 1_048_576.0))
                    .unwrap_or_default();
                println!(
                    "{} {} ON {}: {action}{size} ({:.1} s)",
                    args.region,
                    report.index.name,
                    report.index.table,
                    report.elapsed.as_secs_f64()
                );
            }
            if reports.is_empty() {
                println!("{}: no event tables found", args.region);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("create-covering-indexes failed: {err}");
            ExitCode::from(1)
        }
    }
}

async fn resolve_addr(target: &str) -> Result<SocketAddr, ()> {
    match tokio::net::lookup_host(target).await {
        Ok(mut iter) => match iter.next() {
            Some(a) => Ok(a),
            None => {
                tracing::error!(%target, "no address resolved");
                Err(())
            }
        },
        Err(err) => {
            tracing::error!(%err, %target, "DNS lookup failed");
            Err(())
        }
    }
}

#[cfg(target_os = "linux")]
fn log_open_file_limit() {
    match std::fs::read_to_string("/proc/self/limits") {
        Ok(limits) => {
            if let Some(line) = limits
                .lines()
                .find(|line| line.starts_with("Max open files"))
            {
                tracing::info!(limit = %line, "process open file limit");
            }
        }
        Err(err) => tracing::warn!(%err, "failed to read process open file limit"),
    }
}

#[cfg(not(target_os = "linux"))]
fn log_open_file_limit() {}
