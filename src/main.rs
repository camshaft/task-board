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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,task_board=debug".into()),
        )
        .init();

    let cfg = config::Config::from_env();
    let _ = WEBHOOK_TIMEOUT.set(cfg.webhook_timeout);

    let pool = db::init(&cfg.db_path).await?;
    tracing::info!("db ready at {}", cfg.db_path);

    // MCP over streamable-HTTP at /mcp (fresh Board handle per session).
    let ct = tokio_util::sync::CancellationToken::new();
    let mcp_pool = pool.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(mcp::Board::new(mcp_pool.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
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
