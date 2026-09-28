//! Task-board settings, overridable via TB_* env vars.

use std::time::Duration;

/// Reference vocabulary. Not hard-enforced (agents may use others), but these are the
/// blessed values the UI understands.
pub const TASK_STATUSES: &[&str] = &["todo", "in_progress", "blocked", "done", "cancelled"];
pub const PROJECT_STATUSES: &[&str] = &["active", "archived"];
pub const AGENT_STATUSES: &[&str] = &["online", "busy", "away", "offline"];

#[derive(Clone, Debug)]
pub struct Config {
    pub db_path: String,
    pub host: String,
    pub port: u16,
    /// Best-effort HTTP push to agents that registered a webhook_url (fire-and-forget).
    pub webhook_timeout: Duration,
    /// Directory of built web UI assets to serve at `/`, if present.
    pub web_dir: Option<String>,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

impl Config {
    pub fn from_env() -> Self {
        let web_dir = std::env::var("TB_WEB_DIR").ok().filter(|s| !s.is_empty());
        Self {
            db_path: env_or("TB_DB_PATH", "/data/task-board/board.db"),
            host: env_or("TB_MCP_HOST", "0.0.0.0"),
            port: env_or("TB_MCP_PORT", "8079").parse().unwrap_or(8079),
            webhook_timeout: Duration::from_secs_f64(
                env_or("TB_WEBHOOK_TIMEOUT", "5").parse().unwrap_or(5.0),
            ),
            web_dir,
        }
    }
}
