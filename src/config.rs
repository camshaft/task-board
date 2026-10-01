//! Task-board settings, loaded from a TOML config file.
//!
//! Everything a deployment cares about lives in one documented file (see `Settings`
//! below and `config.example.toml` at the repo root). `web_dir` is intentionally *not*
//! a setting — it's the location of the bundled UI assets, decided by packaging, and is
//! passed on the command line (`--web-dir`) by the Nix wrapper.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

/// Reference vocabulary. Not hard-enforced (agents may use others), but these are the
/// blessed values the UI understands.
pub const TASK_STATUSES: &[&str] = &["todo", "in_progress", "blocked", "done", "cancelled"];
pub const PROJECT_STATUSES: &[&str] = &["active", "archived"];
pub const AGENT_STATUSES: &[&str] = &["online", "busy", "away", "offline"];

/// The on-disk settings, deserialized from TOML. Every field has a default so a partial
/// (or absent) file still works; the defaults match the documented example.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Path to the SQLite database file. Its parent directory is created on startup.
    pub db_path: String,
    /// Address to bind (e.g. "0.0.0.0" for all interfaces, "127.0.0.1" for local-only).
    pub host: String,
    /// Port to listen on for MCP (/mcp), the REST API (/api), and the web UI (/).
    pub port: u16,
    /// Timeout, in seconds, for best-effort webhook POSTs to agents that registered one.
    pub webhook_timeout_secs: f64,
    /// Host authorities the MCP endpoint (`/mcp`) accepts in the inbound `Host` header.
    ///
    /// rmcp guards streamable-HTTP against DNS-rebinding by only accepting loopback hosts
    /// by default. An empty list keeps that safe default (loopback only). When the service
    /// is LAN-exposed or fronted by a reverse proxy that does not rewrite `Host`, list the
    /// authorities clients actually send (e.g. "green-machine.lan:8079", "board.example.com").
    /// A single `"*"` disables the check entirely — any `Host` is accepted (NOT recommended
    /// for public deployments; only for closed networks).
    pub mcp_allowed_hosts: Vec<String>,
    /// Optional IPFS HTTP API base URL (e.g. "http://127.0.0.1:5001"). When set, the board
    /// can content-address raw document `content` server-side (pin via `/api/v0/add` and
    /// store the returned CID) — so a client with no local IPFS can still author a document.
    /// When unset (the default), the board stays CID-only: callers supply a precomputed CID.
    pub ipfs_api_url: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            db_path: "/data/task-board/board.db".to_string(),
            host: "0.0.0.0".to_string(),
            port: 8079,
            webhook_timeout_secs: 5.0,
            mcp_allowed_hosts: Vec::new(),
            ipfs_api_url: None,
        }
    }
}

/// Runtime configuration: the parsed settings plus the process-level `web_dir` (from the
/// `--web-dir` flag / packaging), which is not part of the TOML.
#[derive(Clone, Debug)]
pub struct Config {
    pub db_path: String,
    pub host: String,
    pub port: u16,
    /// Best-effort HTTP push to agents that registered a webhook_url (fire-and-forget).
    pub webhook_timeout: Duration,
    /// Directory of built web UI assets to serve at `/`, if present.
    pub web_dir: Option<String>,
    /// `Host` authorities accepted by the MCP endpoint. Empty == rmcp's loopback-only
    /// default; `["*"]` == accept any host. See `Settings::mcp_allowed_hosts`.
    pub mcp_allowed_hosts: Vec<String>,
    /// Optional IPFS HTTP API base URL for server-side content-addressing. See
    /// `Settings::ipfs_api_url`. `None` keeps the board CID-only.
    pub ipfs_api_url: Option<String>,
}

impl Config {
    /// Load settings from a TOML file, then attach the (non-TOML) `web_dir`.
    pub fn load(path: &Path, web_dir: Option<String>) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading config file {}: {e}", path.display()))?;
        let settings: Settings = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing config file {}: {e}", path.display()))?;
        Ok(Self::from_settings(settings, web_dir))
    }

    /// Build a config from built-in defaults (used when no `--config` is given).
    pub fn defaults(web_dir: Option<String>) -> Self {
        Self::from_settings(Settings::default(), web_dir)
    }

    fn from_settings(s: Settings, web_dir: Option<String>) -> Self {
        Self {
            db_path: s.db_path,
            host: s.host,
            port: s.port,
            webhook_timeout: Duration::from_secs_f64(s.webhook_timeout_secs),
            web_dir: web_dir.filter(|s| !s.is_empty()),
            mcp_allowed_hosts: s.mcp_allowed_hosts,
            ipfs_api_url: s.ipfs_api_url.filter(|s| !s.is_empty()),
        }
    }
}
