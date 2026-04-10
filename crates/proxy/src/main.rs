#![allow(clippy::doc_markdown)]
//! `platform-proxy` -- process wrapper with mTLS, log capture, and OTLP export.
//!
//! Usage:
//!   platform-proxy --wrap -- postgres -c max_connections=300
//!   platform-proxy --wrap --app-port 8080 -- my-app --listen :8080
//!   platform-proxy --wrap --tcp-ports 5432,6379 -- my-service
//!
//! Env vars:
//!   PLATFORM_API_URL          -- platform HTTP endpoint (for OTLP export + cert issuance)
//!   PLATFORM_API_TOKEN        -- bearer token for platform API auth
//!   PLATFORM_PROJECT_ID       -- UUID, set as platform.project_id resource attribute
//!   PLATFORM_SERVICE_NAME     -- service name (default: binary name of wrapped process)
//!   PLATFORM_SESSION_ID       -- optional session UUID
//!   PROXY_HEALTH_PORT         -- health endpoint port (default: 15020)
//!   PROXY_APP_PORT            -- app HTTP port for readiness check (default: auto-detect)
//!   PROXY_LOG_LEVEL           -- proxy's own log level (default: info)
//!   PROXY_METRICS_INTERVAL    -- process metrics scrape interval (default: 15s)
//!   PROXY_FLUSH_INTERVAL      -- OTLP batch flush interval (default: 5s)
//!   PROXY_BATCH_SIZE          -- max spans/logs per OTLP batch (default: 500)
//!   PROXY_TLS_PORT            -- mTLS inbound listener port (e.g. 8443)
//!   PROXY_OUTBOUND_PORT       -- mTLS outbound proxy port (e.g. 15001)
//!   PROXY_NAMESPACE           -- K8s namespace for SPIFFE identity (default: "default")
//!   PROXY_SCRAPE_TYPE         -- metrics scraper: "postgres", "redis"
//!   PROXY_SCRAPE_URL          -- Prometheus scrape URL
//!   PROXY_SCRAPE_POSTGRES_URL -- Postgres connection URL for stat queries
//!   PROXY_SCRAPE_REDIS_URL    -- Redis/Valkey connection URL for INFO scraping

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::{mpsc, watch};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use platform_proxy::proxy::{
    child, config::ProxyConfig, gateway, health, inbound, logs, metrics, otlp, outbound,
    process_metrics, scraper, tcp_proxy, tls, traces,
};

/// Start mTLS bootstrap with retry in the background.
///
/// Returns `SharedCerts` immediately. The certs are populated asynchronously
/// once the platform API becomes available. mTLS listeners use `ArcSwap` to
/// pick up the certs when they arrive — connections before that are plain TCP.
fn spawn_mtls_bootstrap(
    config: &ProxyConfig,
    shutdown_rx: &watch::Receiver<()>,
) -> Option<tls::SharedCerts> {
    let needs_mtls = config.tls_port.is_some()
        || config.outbound_port.is_some()
        || !config.tcp_ports.is_empty()
        || config.transparent;
    if !needs_mtls {
        return None;
    }

    // Create shared certs holder — starts empty, filled by background task
    let shared = Arc::new(ArcSwap::from_pointee(tls::ProxyCerts::empty()));

    let config = config.clone();
    let certs = shared.clone();
    let mut shutdown = shutdown_rx.clone();
    let renewal_shutdown = shutdown_rx.clone();

    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(2);
        let max_backoff = Duration::from_secs(30);

        loop {
            match tls::bootstrap_cert(&config).await {
                Ok(initial_certs) => {
                    tracing::info!(
                        not_after = %initial_certs.not_after,
                        "mTLS certificates bootstrapped"
                    );
                    certs.store(Arc::new(initial_certs));
                    // Start renewal loop
                    tls::cert_renewal_loop(config, certs, renewal_shutdown).await;
                    return;
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        retry_secs = backoff.as_secs(),
                        "cert bootstrap failed, retrying"
                    );
                    tokio::select! {
                        () = tokio::time::sleep(backoff) => {}
                        _ = shutdown.changed() => return,
                    }
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    });

    Some(shared)
}

/// Start mesh (mTLS listeners, TCP proxies) and scraper components.
#[allow(clippy::too_many_arguments)]
fn start_mesh_components(
    config: &ProxyConfig,
    certs: Option<&tls::SharedCerts>,
    span_tx: &mpsc::Sender<traces::SpanRecord>,
    active_spans: &traces::SharedActiveSpans,
    red_metrics: &Arc<metrics::RedMetrics>,
    metric_tx: &mpsc::Sender<metrics::MetricRecord>,
    shutdown_rx: &watch::Receiver<()>,
) {
    // Transparent mode -- replaces explicit inbound/outbound when enabled
    if config.transparent {
        if let Some(certs) = certs {
            start_transparent_listeners(
                config,
                certs,
                span_tx,
                active_spans,
                red_metrics,
                metric_tx,
                shutdown_rx,
            );
        }
        return;
    }

    // Inbound mTLS listener
    if let (Some(tls_port), Some(app_port), Some(certs)) = (config.tls_port, config.app_port, certs)
    {
        let params = inbound::InboundParams {
            tls_port,
            app_port,
            service_name: config.service_name.clone(),
            certs: certs.clone(),
            span_tx: span_tx.clone(),
            active_spans: active_spans.clone(),
            red_metrics: red_metrics.clone(),
        };
        tokio::spawn(inbound::run_inbound_listener(params, shutdown_rx.clone()));
        tokio::spawn(metrics::flush_red_metrics(
            red_metrics.clone(),
            config.service_name.clone(),
            metric_tx.clone(),
            config.metrics_interval,
            shutdown_rx.clone(),
        ));
    }

    // Outbound mTLS proxy
    if let (Some(outbound_port), Some(certs)) = (config.outbound_port, certs) {
        let params = outbound::OutboundParams {
            listen_port: outbound_port,
            service_name: config.service_name.clone(),
            certs: certs.clone(),
            span_tx: span_tx.clone(),
        };
        tokio::spawn(outbound::run_outbound_proxy(params, shutdown_rx.clone()));
    }

    // TCP proxies for non-HTTP ports
    if let Some(certs) = certs {
        for &port in &config.tcp_ports {
            let tls_port = port + 10_000;
            tokio::spawn(tcp_proxy::run_tcp_proxy(
                tls_port,
                port,
                config.service_name.clone(),
                certs.clone(),
                span_tx.clone(),
                shutdown_rx.clone(),
            ));
        }
    }

    // Metrics scraper
    if config.scrape_type.is_some() || config.scrape_url.is_some() {
        let scraper_config = scraper::ScraperConfig {
            scrape_type: config.scrape_type.clone(),
            scrape_url: config.scrape_url.clone(),
            postgres_url: config.scrape_postgres_url.clone(),
            redis_url: config.scrape_redis_url.clone(),
            tls_insecure: config.scrape_tls_insecure,
            service_name: config.service_name.clone(),
        };
        tokio::spawn(scraper::run_metrics_scraper(
            scraper_config,
            metric_tx.clone(),
            config.metrics_interval,
            shutdown_rx.clone(),
        ));
    }
}

/// Spawn transparent inbound + outbound listeners.
#[allow(clippy::too_many_arguments)]
fn start_transparent_listeners(
    config: &ProxyConfig,
    certs: &tls::SharedCerts,
    span_tx: &mpsc::Sender<traces::SpanRecord>,
    active_spans: &traces::SharedActiveSpans,
    red_metrics: &Arc<metrics::RedMetrics>,
    metric_tx: &mpsc::Sender<metrics::MetricRecord>,
    shutdown_rx: &watch::Receiver<()>,
) {
    let inbound_params = inbound::TransparentInboundParams {
        inbound_port: config.inbound_port,
        mtls_mode: config.mtls_mode,
        node_cidrs: config.node_cidrs.clone(),
        bypass_port_range: config.bypass_port_range,
        passthrough_ports: config.tcp_ports.clone(),
        service_name: config.service_name.clone(),
        certs: certs.clone(),
        span_tx: span_tx.clone(),
        active_spans: active_spans.clone(),
        red_metrics: red_metrics.clone(),
    };
    tokio::spawn(inbound::run_transparent_inbound(
        inbound_params,
        shutdown_rx.clone(),
    ));
    tokio::spawn(metrics::flush_red_metrics(
        red_metrics.clone(),
        config.service_name.clone(),
        metric_tx.clone(),
        config.metrics_interval,
        shutdown_rx.clone(),
    ));

    let outbound_port = config.outbound_port.unwrap_or(15001);
    let outbound_params = outbound::TransparentOutboundParams {
        outbound_port,
        bypass_port_range: config.bypass_port_range,
        passthrough_ports: config.tcp_ports.clone(),
        internal_cidrs: config.internal_cidrs.clone(),
        service_name: config.service_name.clone(),
        certs: certs.clone(),
        span_tx: span_tx.clone(),
    };
    tokio::spawn(outbound::run_transparent_outbound(
        outbound_params,
        shutdown_rx.clone(),
    ));
}

/// Initialize tracing and run the gateway mode.
async fn init_gateway_and_run(args: Vec<String>) {
    let gw_config = gateway::GatewayConfig::from_env();
    let filter = tracing_subscriber::EnvFilter::try_new(&gw_config.log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false).json())
        .init();

    gateway::run(args).await;
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() {
    // Install rustls crypto provider
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Check for gateway mode
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--gateway") {
        init_gateway_and_run(args).await;
        return;
    }

    // Parse config from env + CLI args (wrap/sidecar mode)
    let (config, child_args) = ProxyConfig::from_env_and_args(&args);

    // Initialize tracing
    let filter = EnvFilter::try_new(&config.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false).json())
        .init();

    tracing::info!(
        service = %config.service_name,
        transparent = config.transparent,
        tls_port = ?config.tls_port,
        outbound_port = ?config.outbound_port,
        tcp_ports = ?config.tcp_ports,
        scrape_type = ?config.scrape_type,
        "platform-proxy starting"
    );

    // Shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(());

    // OTLP channels
    let (span_tx, span_rx) = mpsc::channel::<traces::SpanRecord>(10_000);
    let (log_tx, log_rx) = mpsc::channel::<logs::LogRecord>(10_000);
    let (metric_tx, metric_rx) = mpsc::channel::<metrics::MetricRecord>(10_000);

    // Shared active spans for log correlation
    let active_spans = traces::SharedActiveSpans::default();

    // RED metrics for HTTP proxy traffic
    let red_metrics = Arc::new(metrics::RedMetrics::new());

    // Start OTLP exporter — uses dedicated OTLP token (observe:write scope)
    let exporter = otlp::OtlpExporter::new(
        config.api_url.clone(),
        config.otlp_token.clone(),
        config.project_id.clone(),
        config.service_name.clone(),
        config.session_id.clone(),
    );
    tokio::spawn(otlp::run_exporter(
        exporter,
        span_rx,
        log_rx,
        metric_rx,
        config.flush_interval,
        config.batch_size,
        shutdown_rx.clone(),
    ));

    // Start health server
    tokio::spawn(health::run_health_server(
        config.health_port,
        config.app_port,
        shutdown_rx.clone(),
    ));

    // Start process metrics (cgroup CPU/MEM)
    tokio::spawn(process_metrics::flush_process_metrics(
        config.service_name.clone(),
        metric_tx.clone(),
        config.metrics_interval,
        shutdown_rx.clone(),
    ));

    // Bootstrap mTLS in background (retries until platform API is available)
    let certs = spawn_mtls_bootstrap(&config, &shutdown_rx);

    // Start mTLS components + scraper (listeners use ArcSwap, pick up certs when ready)
    start_mesh_components(
        &config,
        certs.as_ref(),
        &span_tx,
        &active_spans,
        &red_metrics,
        &metric_tx,
        &shutdown_rx,
    );

    // Spawn child process (if any) — starts immediately, doesn't wait for mTLS
    if child_args.is_empty() {
        tracing::info!("no child command specified, running in proxy-only mode");
        // Wait for shutdown signal
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received SIGINT, shutting down");
            }
        }
        let _ = shutdown_tx.send(());
    } else {
        let command = &child_args[0];
        let args = &child_args[1..];

        match child::spawn(command, args) {
            Ok(spawned) => {
                let child::SpawnedChild {
                    mut child,
                    stdout,
                    stderr,
                } = spawned;

                // Start zombie reaper (we're PID 1)
                tokio::spawn(child::reap_zombies(shutdown_rx.clone()));

                // Start log pipeline
                tokio::spawn(logs::run_log_pipeline(
                    stdout,
                    stderr,
                    log_tx,
                    active_spans,
                    shutdown_rx.clone(),
                ));

                // Wait for child to exit
                let exit_code = child::wait_for_exit(&mut child, shutdown_tx).await;

                // Give a brief moment for final flush
                tokio::time::sleep(Duration::from_millis(500)).await;

                std::process::exit(exit_code);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to spawn child process");
                let _ = shutdown_tx.send(());
                std::process::exit(1);
            }
        }
    }
}
