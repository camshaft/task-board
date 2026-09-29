//! task-board: a scrappy SQLite-backed coordination board for agents, exposed over both
//! MCP (streamable-HTTP, for agents) and a REST API (for humans + the web UI).

mod api;
mod config;
mod core;
mod db;
mod events;
mod ipfs;
mod mcp;
mod sse;
mod tunnel;

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
    task-board --dedup-projects [--config <path>]

OPTIONS:
    --config <path>    TOML config file (see config.example.toml). Omit for defaults.
    --web-dir <path>   Directory of built UI assets to serve at /. Usually set by
                       packaging; falls back to the TB_WEB_DIR env var.
    --dedup-projects   One-shot maintenance: merge case-insensitive duplicate projects
                       (keep the earliest, repoint tasks/subs/events), then exit. Back up
                       the DB first. Does not start the server.
    -h, --help         Print this help.
";

/// Command-line inputs. Everything else lives in the TOML config file.
struct CliArgs {
    config: Option<String>,
    web_dir: Option<String>,
    dedup_projects: bool,
}

impl CliArgs {
    fn parse(args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut config = None;
        let mut web_dir = None;
        let mut dedup_projects = false;
        let mut it = args;
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--config" => {
                    config = Some(it.next().ok_or_else(|| anyhow::anyhow!("--config needs a path"))?);
                }
                "--web-dir" => {
                    web_dir = Some(it.next().ok_or_else(|| anyhow::anyhow!("--web-dir needs a path"))?);
                }
                "--dedup-projects" => dedup_projects = true,
                "-h" | "--help" => {
                    print!("{USAGE}");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown argument `{other}`\n\n{USAGE}"),
            }
        }
        Ok(Self { config, web_dir, dedup_projects })
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

    // One-shot maintenance: merge duplicate projects, report, and exit without serving.
    if args.dedup_projects {
        let report = core::merge_duplicate_projects(&pool).await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

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
    let mcp_ipfs = cfg.ipfs_api_url.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(mcp::Board::new(mcp_pool.clone(), mcp_ipfs.clone())),
        LocalSessionManager::default().into(),
        mcp_config,
    );

    // Live activity bus + the background tailer that feeds it from the committed event log.
    // The sender lives in AppState so `GET /api/stream` can subscribe per connection.
    let events_tx = sse::channel();
    sse::spawn_tailer(pool.clone(), events_tx.clone());

    // REST API at /api.
    let api_router = api::router(api::AppState {
        pool: pool.clone(),
        events_tx,
        ipfs_api_url: cfg.ipfs_api_url.clone(),
    });

    // Reverse tunnel for fleet hosts with no inbound path: they dial /tunnel/ws and the board
    // pushes wakes down the socket. Top-level (not under /api) — it's a WS upgrade, not REST.
    let tunnels = tunnel::registry();

    let mut router = Router::new()
        .nest_service("/mcp", mcp_service)
        .nest("/api", api_router)
        .merge(tunnel::ws_router(tunnels.clone()))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    // Optionally serve the built web UI at /. Assets come from ServeDir; every request
    // for index.html (the directory index at `/` and the SPA fallback) goes through
    // `serve_index`, which injects a <base href> so the app works at the origin root or
    // under a reverse-proxy sub-path — driven entirely by the X-Forwarded-Prefix header,
    // nothing baked in at build time. `append_index_html_on_directories(false)` makes `/`
    // miss in ServeDir and fall through to that handler instead of the raw file.
    if let Some(dir) = &cfg.web_dir {
        let index_path: std::sync::Arc<str> = format!("{dir}/index.html").into();
        let serve = ServeDir::new(dir)
            .append_index_html_on_directories(false)
            .fallback(axum::routing::get(move |headers: axum::http::HeaderMap| {
                let index_path = index_path.clone();
                async move { serve_index(&index_path, &headers).await }
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

/// Serve index.html with a `<base href>` injected so relative asset/API URLs resolve
/// under whatever path the app is mounted at. The mount is read from `X-Forwarded-Prefix`
/// (set by a sub-path reverse proxy); absent that, the base is `/` (origin root). Returns
/// 200 so client-side routing works on deep links.
async fn serve_index(index_path: &str, headers: &axum::http::HeaderMap) -> axum::response::Response {
    let html = match tokio::fs::read_to_string(index_path).await {
        Ok(h) => h,
        Err(_) => {
            return (axum::http::StatusCode::NOT_FOUND, "index.html missing").into_response()
        }
    };
    // Normalize the forwarded prefix to exactly one leading and one trailing slash, e.g.
    // "/board" or "board/" -> "/board/", empty/unset -> "/". A trailing slash is required
    // for <base href> to resolve "./assets/x" as "{prefix}/assets/x".
    let prefix = headers
        .get("x-forwarded-prefix")
        .and_then(|v| v.to_str().ok())
        .map(|p| p.trim().trim_matches('/'))
        .filter(|p| !p.is_empty())
        .map(|p| format!("/{p}/"))
        .unwrap_or_else(|| "/".to_string());
    // Inject right after <head> so it precedes every asset reference in the document.
    let injected = format!("<base href=\"{prefix}\">");
    let html = match html.split_once("<head>") {
        Some((head, rest)) => format!("{head}<head>{injected}{rest}"),
        None => format!("{injected}{html}"),
    };
    Html(html).into_response()
}
