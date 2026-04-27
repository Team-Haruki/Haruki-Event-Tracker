use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;

use haruki_event_tracker::{api, app, config, logger, shutdown};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> ExitCode {
    let cfg = match config::load_from_file(config::DEFAULT_CONFIG_FILE) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("failed to load {}: {err}", config::DEFAULT_CONFIG_FILE);
            return ExitCode::from(1);
        }
    };

    let log_file =
        (!cfg.backend.main_log_file.is_empty()).then(|| cfg.backend.main_log_file.clone());
    let access_log_file =
        (!cfg.backend.access_log_path.is_empty()).then(|| cfg.backend.access_log_path.clone());
    if let Err(err) = logger::init(
        &cfg.backend.log_level,
        log_file.as_deref(),
        access_log_file.as_deref(),
    ) {
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

    let (trust, bad_cidrs) = haruki_event_tracker::api::access_log::ProxyTrust::from_config(
        cfg.backend.enable_trust_proxy,
        &cfg.backend.trusted_proxies,
        &cfg.backend.proxy_header,
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
            tracing::info!(grace_secs = SHUTDOWN_GRACE.as_secs(), "starting graceful shutdown");
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

        let tls = match RustlsConfig::from_pem_file(&cfg.backend.ssl_cert, &cfg.backend.ssl_key)
            .await
        {
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
        axum_server::bind_rustls(addr, tls)
            .handle(handle)
            .serve(make_service)
            .await
    } else {
        tracing::info!(addr = %addr, "HTTP server listening");
        axum_server::bind(addr).handle(handle).serve(make_service).await
    };

    if let Err(err) = serve_result {
        tracing::error!(%err, "server error");
    }

    shutdown::run(ctx.scheduler, ctx.trackers, ctx.dbs, ctx.state).await;
    tracing::info!("bye");
    ExitCode::SUCCESS
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
