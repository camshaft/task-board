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

use axum::response::{Html, IntoResponse};
use axum::Router;
use listenfd::ListenFd;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
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
    --seed-ui-elements <path>
                       One-shot: pin each UI element's props_schema from the given
                       ui-elements.json into the CAS and print the name->CID manifest to
                       stdout, then exit. Needs ipfs_api_url. Does not start the server.
    --build-ui-catalog <ui-elements.json> <ui-element-cids.json>
                       One-shot: join the ui-elements set with the name->CID manifest and
                       print the agent-facing ui-element catalog JSON to stdout, then exit
                       (task_820). Pure build-time join of the two committed files; needs no
                       CAS and no DB. Publish the output as the system/ui-elements document.
    -h, --help         Print this help.
";

/// Command-line inputs. Everything else lives in the TOML config file.
struct CliArgs {
    config: Option<String>,
    web_dir: Option<String>,
    dedup_projects: bool,
    seed_ui_elements: Option<String>,
    /// (ui-elements.json path, ui-element-cids.json path) for the one-shot catalog build (task_820).
    build_ui_catalog: Option<(String, String)>,
}

impl CliArgs {
    fn parse(args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        let mut config = None;
        let mut web_dir = None;
        let mut dedup_projects = false;
        let mut seed_ui_elements = None;
        let mut build_ui_catalog = None;
        let mut it = args;
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--config" => {
                    config = Some(
                        it.next()
                            .ok_or_else(|| anyhow::anyhow!("--config needs a path"))?,
                    );
                }
                "--web-dir" => {
                    web_dir = Some(
                        it.next()
                            .ok_or_else(|| anyhow::anyhow!("--web-dir needs a path"))?,
                    );
                }
                "--dedup-projects" => dedup_projects = true,
                "--seed-ui-elements" => {
                    seed_ui_elements = Some(
                        it.next()
                            .ok_or_else(|| anyhow::anyhow!("--seed-ui-elements needs a path"))?,
                    );
                }
                "--build-ui-catalog" => {
                    let elements = it.next().ok_or_else(|| {
                        anyhow::anyhow!(
                            "--build-ui-catalog needs <ui-elements.json> <ui-element-cids.json>"
                        )
                    })?;
                    let manifest = it.next().ok_or_else(|| {
                        anyhow::anyhow!(
                            "--build-ui-catalog needs <ui-elements.json> <ui-element-cids.json>"
                        )
                    })?;
                    build_ui_catalog = Some((elements, manifest));
                }
                "-h" | "--help" => {
                    print!("{USAGE}");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown argument `{other}`\n\n{USAGE}"),
            }
        }
        Ok(Self {
            config,
            web_dir,
            dedup_projects,
            seed_ui_elements,
            build_ui_catalog,
        })
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

    // One-shot: pin the UI element schemas into the CAS and print the name->CID manifest, then exit
    // without serving (task_755). The CID of each element's props_schema is its canonical build-time
    // identifier (doc_33 v16); redirect stdout to commit the manifest the web build + the CID-keyed
    // question model consume. Needs ipfs_api_url (the CAS to pin into); touches no DB.
    if let Some(path) = &args.seed_ui_elements {
        let Some(ipfs_url) = cfg.ipfs_api_url.as_deref() else {
            anyhow::bail!("--seed-ui-elements needs ipfs_api_url configured (the CAS to pin into)");
        };
        let json = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("reading ui-elements file {path}: {e}"))?;
        let set = core::parse_ui_element_set(&json)?;
        let manifest = core::seed_ui_elements(ipfs_url, &set).await?;
        println!("{}", serde_json::to_string_pretty(&manifest)?);
        return Ok(());
    }

    // One-shot: build the agent-facing ui-element catalog (task_820) by joining the committed
    // ui-elements.json with the name->CID manifest, and print it to stdout. Pure build-time join --
    // no CAS, no DB. The output is published as the system/ui-elements document that the MCP
    // ui-element resource serves.
    if let Some((elements_path, manifest_path)) = &args.build_ui_catalog {
        let elements = std::fs::read(elements_path)
            .map_err(|e| anyhow::anyhow!("reading ui-elements file {elements_path}: {e}"))?;
        let manifest = std::fs::read(manifest_path)
            .map_err(|e| anyhow::anyhow!("reading manifest file {manifest_path}: {e}"))?;
        let catalog = core::build_ui_element_catalog(&elements, &manifest)?;
        println!("{}", serde_json::to_string_pretty(&catalog)?);
        return Ok(());
    }

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
    let mut mcp_config =
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token());
    if cfg.mcp_allowed_hosts.iter().any(|h| h == "*") {
        tracing::warn!(
            "MCP Host validation disabled (mcp_allowed_hosts = [\"*\"]); any Host accepted"
        );
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
        // Shares cancellation with `ct`: the shutdown closure's `ct.cancel()` ends in-flight SSE
        // streams so graceful_shutdown drains promptly instead of waiting out the stop-timeout.
        shutdown: ct.clone(),
        db_path: cfg.db_path.clone(),
        db_snapshot: if cfg.db_snapshot_enabled {
            api::DbSnapshotCfg {
                enabled: true,
                user: cfg.db_snapshot_user.clone(),
                password: cfg.db_snapshot_password.clone(),
            }
        } else {
            api::DbSnapshotCfg::disabled()
        },
        host_auth: std::sync::Arc::new(
            cfg.hosts
                .iter()
                .map(|(name, h)| (name.to_ascii_lowercase(), h.auth_header.clone()))
                .collect(),
        ),
    });

    // Reverse tunnel for fleet hosts with no inbound path: they dial /tunnel/ws and the board
    // pushes wakes down the socket. Top-level (not under /api) — it's a WS upgrade, not REST.
    let tunnels = tunnel::registry();
    // Make the registry reachable from the event-emit path (events::emit) so a committed
    // notification can also push a best-effort wake down a live tunnel.
    tunnel::init_global(tunnels.clone());

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

    // Prefer a socket-activated listener: systemd passes the already-bound listening socket via
    // LISTEN_FDS (see the task-board.socket unit). Because systemd owns that socket, it stays open
    // across a service stop->start (e.g. a colmena redeploy), so mid-deploy connections queue in the
    // kernel backlog and are served the instant the new process accepts — no connection-refused
    // window, so no deploy-time 502 at whatever proxy sits in front. Fall back to binding the port
    // ourselves when not socket-activated (dev / non-systemd runs), preserving existing behavior.
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = match ListenFd::from_env().take_tcp_listener(0)? {
        Some(std_listener) => {
            std_listener.set_nonblocking(true)?;
            tracing::info!(
                "task-board listening on socket-activated fd (LISTEN_FDS)  (MCP: /mcp, API: /api)"
            );
            tokio::net::TcpListener::from_std(std_listener)?
        }
        None => {
            let l = tokio::net::TcpListener::bind(&addr).await?;
            tracing::info!("task-board listening on http://{addr}  (MCP: /mcp, API: /api)");
            l
        }
    };

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            ct.cancel();
        })
        .await?;
    Ok(())
}

/// Resolve when the process should begin a graceful shutdown: on SIGINT (ctrl_c, dev/foreground) OR
/// SIGTERM (what systemd / a `colmena switch` / `kill` send on a redeploy). Catching SIGTERM is the
/// point -- otherwise a managed restart terminates the process on the default signal disposition,
/// severing in-flight requests mid-response (the deploy-time "incomplete response" 502). Reaching
/// here instead lets axum drain in-flight requests and the caller cancel the SSE tailer first.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            // If we can't install the handler, never resolve on this arm (fall back to ctrl_c).
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Serve index.html with a `<base href>` injected so relative asset/API URLs resolve
/// under whatever path the app is mounted at. The mount is read from `X-Forwarded-Prefix`
/// (set by a sub-path reverse proxy); absent that, the base is `/` (origin root). Returns
/// 200 so client-side routing works on deep links.
async fn serve_index(
    index_path: &str,
    headers: &axum::http::HeaderMap,
) -> axum::response::Response {
    let html = match tokio::fs::read_to_string(index_path).await {
        Ok(h) => h,
        Err(_) => return (axum::http::StatusCode::NOT_FOUND, "index.html missing").into_response(),
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
