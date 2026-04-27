use std::net::SocketAddr;
use std::process::ExitCode;

use haruki_event_tracker::{api, app, config, logger, shutdown};

#[tokio::main]
async fn main() -> ExitCode {
    let cfg = match config::load_from_file(config::DEFAULT_CONFIG_FILE) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("failed to load {}: {err}", config::DEFAULT_CONFIG_FILE);
            return ExitCode::from(1);
        }
    };

    let log_file = (!cfg.backend.main_log_file.is_empty()).then(|| cfg.backend.main_log_file.clone());
    if let Err(err) = logger::init(&cfg.backend.log_level, log_file.as_deref()) {
        eprintln!("failed to init logger: {err}");
        return ExitCode::from(1);
    }

    tracing::info!(
        "========================= Haruki Event Tracker {} =========================",
        env!("CARGO_PKG_VERSION")
    );
    tracing::info!("Powered by Haruki Dev Team");

    let ctx = match app::build(&cfg).await {
        Ok(ctx) => ctx,
        Err(err) => {
            tracing::error!(%err, "bootstrap failed");
            return ExitCode::from(1);
        }
    };

    if cfg.backend.ssl {
        tracing::warn!(
            "SSL is enabled in config but not yet supported by the Rust build; \
             starting plain HTTP — terminate TLS at a reverse proxy for now"
        );
    }

    let router = api::router::build_router(ctx.state.clone());
    let bind_target = format!("{}:{}", cfg.backend.host, cfg.backend.port);
    let listener = match tokio::net::TcpListener::bind(&bind_target).await {
        Ok(l) => l,
        Err(err) => {
            tracing::error!(%err, target = %bind_target, "failed to bind");
            return ExitCode::from(1);
        }
    };
    tracing::info!(target = %bind_target, "HTTP server listening");

    let serve = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown::signal());

    if let Err(err) = serve.await {
        tracing::error!(%err, "axum server error");
    }

    shutdown::run(ctx.scheduler, ctx.trackers, ctx.dbs).await;
    tracing::info!("bye");
    ExitCode::SUCCESS
}
