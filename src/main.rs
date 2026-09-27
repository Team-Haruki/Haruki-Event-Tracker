use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use axum_server::Handle;
use axum_server::accept::NoDelayAcceptor;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};

use haruki_event_tracker::db::engine::DatabaseEngine;
use haruki_event_tracker::db::repair::repair_time_ids;
use haruki_event_tracker::db::schema::{CoveringIndexAction, create_covering_indexes};
use haruki_event_tracker::model::enums::SekaiServerRegion;
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
    DatabaseEngine::connect(&server_cfg.db)
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
