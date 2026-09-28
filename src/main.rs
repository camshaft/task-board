//! task-board: a scrappy SQLite-backed coordination board for agents, exposed over both
//! MCP (streamable-HTTP, for agents) and a REST API (for humans + the web UI).

mod api;
mod config;
mod core;
mod db;
mod events;
mod mcp;

use std::sync::OnceLock;
use std::time::Duration;

use axum::Router;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use axum::response::{Html, IntoResponse};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

/// Webhook timeout, set once at startup and read by the events layer. Avoids threading
/// the config through every core signature.
pub static WEBHOOK_TIMEOUT: OnceLock<Duration> = OnceLock::new();

const USAGE: &str = "\
task-board — agent coordination board (MCP + REST + UI)

USAGE:
    task-board [--config <path>] [--web-dir <path>]

OPTIONS:
    --config <path>    TOML config file (see config.example.toml). Omit for defaults.
    --web-dir <path>   Directory of built UI assets to serve at /. Usually set by
                       packaging; falls back to the TB_WEB_DIR env var.
    -h, --help         Print this help.
";

/// The two command-line inputs. Everything else lives in the TOML config file.
struct CliArgs {
    config: Option<String>,
    web_dir: Option<String>,
}

impl CliArgs {
    fn parse(args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut config = None;
        let mut web_dir = None;
        let mut it = args;
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--config" => {
                    config = Some(it.next().ok_or_else(|| anyhow::anyhow!("--config needs a path"))?);
                }
                "--web-dir" => {
                    web_dir = Some(it.next().ok_or_else(|| anyhow::anyhow!("--web-dir needs a path"))?);
                }
                "-h" | "--help" => {
                    print!("{USAGE}");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown argument `{other}`\n\n{USAGE}"),
            }
        }
        Ok(Self { config, web_dir })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,task_board=debug".into()),
        )
        .init();

    let args = CliArgs::parse(std::env::args().skip(1))?;
    // web_dir isn't a config-file setting: it's where the bundled UI assets live, set by
    // packaging (the Nix wrapper passes --web-dir). Fall back to TB_WEB_DIR for dev.
    let web_dir = args
        .web_dir
        .or_else(|| std::env::var("TB_WEB_DIR").ok())
        .filter(|s| !s.is_empty());
    let cfg = match &args.config {
        Some(path) => config::Config::load(std::path::Path::new(path), web_dir)?,
        None => config::Config::defaults(web_dir),
    };
    let _ = WEBHOOK_TIMEOUT.set(cfg.webhook_timeout);

    let pool = db::init(&cfg.db_path).await?;
    tracing::info!("db ready at {}", cfg.db_path);

    // MCP over streamable-HTTP at /mcp (fresh Board handle per session).
    let ct = tokio_util::sync::CancellationToken::new();
    let mcp_pool = pool.clone();
    // rmcp defaults to a loopback-only Host allowlist (DNS-rebinding protection). Apply the
    // deployment's configured hosts: empty keeps the safe default, ["*"] disables the check,
    // otherwise use the explicit allowlist. See config::Settings::mcp_allowed_hosts.
    let mut mcp_config = StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token());
    if cfg.mcp_allowed_hosts.iter().any(|h| h == "*") {
        tracing::warn!("MCP Host validation disabled (mcp_allowed_hosts = [\"*\"]); any Host accepted");
        mcp_config = mcp_config.disable_allowed_hosts();
    } else if !cfg.mcp_allowed_hosts.is_empty() {
        tracing::info!("MCP allowed hosts: {:?}", cfg.mcp_allowed_hosts);
        mcp_config = mcp_config.with_allowed_hosts(cfg.mcp_allowed_hosts.clone());
    }
    let mcp_service = StreamableHttpService::new(
        move || Ok(mcp::Board::new(mcp_pool.clone())),
        LocalSessionManager::default().into(),
        mcp_config,
    );

    // REST API at /api.
    let api_router = api::router(api::AppState { pool: pool.clone() });

    let mut router = Router::new()
        .nest_service("/mcp", mcp_service)
        .nest("/api", api_router)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    // Optionally serve the built web UI at / (static assets, SPA fallback to index.html
    // with a 200 so client-side routing works).
    if let Some(dir) = &cfg.web_dir {
        let index_path = format!("{dir}/index.html");
        let serve = ServeDir::new(dir).fallback(axum::routing::get(move || {
            let index_path = index_path.clone();
            async move {
                match tokio::fs::read_to_string(&index_path).await {
                    Ok(html) => Html(html).into_response(),
                    Err(_) => (axum::http::StatusCode::NOT_FOUND, "index.html missing")
                        .into_response(),
                }
            }
        }));
        router = router.fallback_service(serve);
        tracing::info!("serving web UI from {dir}");
    }

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("task-board listening on http://{addr}  (MCP: /mcp, API: /api)");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            ct.cancel();
        })
        .await?;
    Ok(())
}
