//! Task-board settings, loaded from a TOML config file.
//!
//! Everything a deployment cares about lives in one documented file (see `Settings`
//! below and `config.example.toml` at the repo root). `web_dir` is intentionally *not*
//! a setting — it's the location of the bundled UI assets, decided by packaging, and is
//! passed on the command line (`--web-dir`) by the Nix wrapper.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Reference vocabulary. Not hard-enforced (agents may use others), but these are the
/// blessed values the UI understands.
pub const TASK_STATUSES: &[&str] = &["todo", "in_progress", "blocked", "done", "cancelled"];
pub const PROJECT_STATUSES: &[&str] = &["active", "archived"];
pub const AGENT_STATUSES: &[&str] = &["online", "busy", "away", "offline"];

/// Built-in default webhook_allowed_cidrs (task_1492 incident correction): loopback + the three
/// RFC1918 private ranges. The board is private-only and CO-RESIDENT with the fleet workers, so a
/// legitimate webhook_url is only ever a fleet-internal host -- loopback (same-host agents advertise
/// a 127.0.0.1 fleet-notify hook) or a private-LAN IP (workers on 172.23.x, board on 10.2.x). Any
/// other target is untrusted. Link-local 169.254.0.0/16 (incl cloud metadata 169.254.169.254) and
/// IPv6 fe80::/10 are deliberately ABSENT, so the metadata SSRF target stays rejected by default.
pub const DEFAULT_WEBHOOK_ALLOWED_CIDRS: &[&str] = &[
    "127.0.0.0/8",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
];

/// The built-in default allowlist, parsed. The entries are known-good, so parsing cannot fail.
pub fn default_webhook_allowed_cidrs() -> Vec<Cidr> {
    DEFAULT_WEBHOOK_ALLOWED_CIDRS
        .iter()
        .map(|c| Cidr::parse(c).expect("built-in default webhook CIDR is valid"))
        .collect()
}

/// A parsed CIDR block (`<ip>/<prefix>`), used by the webhook_url guard allowlist (task_1492). The
/// network address is pre-masked at parse time so `contains` is a single mask-and-compare. A v4 CIDR
/// only matches v4 addresses and a v6 CIDR only v6 (never cross-family).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix_len: u8,
}

fn mask_v4(ip: Ipv4Addr, prefix_len: u8) -> Ipv4Addr {
    let bits = u32::from(ip);
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    };
    Ipv4Addr::from(bits & mask)
}

fn mask_v6(ip: Ipv6Addr, prefix_len: u8) -> Ipv6Addr {
    let bits = u128::from(ip);
    let mask = if prefix_len == 0 {
        0
    } else {
        u128::MAX << (128 - prefix_len)
    };
    Ipv6Addr::from(bits & mask)
}

impl Cidr {
    /// Parse a `<ip>/<prefix>` CIDR string, failing on a missing/bad IP, a non-numeric prefix, or a
    /// prefix that exceeds the address width (32 for v4, 128 for v6).
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let (addr, prefix) = s
            .trim()
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("missing '/<prefix>'"))?;
        let ip: IpAddr = addr
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid IP address: {e}"))?;
        let prefix_len: u8 = prefix
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid prefix length: {e}"))?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        if prefix_len > max {
            anyhow::bail!("prefix length {prefix_len} exceeds the maximum {max} for this address");
        }
        let network = match ip {
            IpAddr::V4(v4) => IpAddr::V4(mask_v4(v4, prefix_len)),
            IpAddr::V6(v6) => IpAddr::V6(mask_v6(v6, prefix_len)),
        };
        Ok(Self {
            network,
            prefix_len,
        })
    }

    /// True iff `ip` falls within this block (same address family and matching masked prefix).
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => mask_v4(ip, self.prefix_len) == net,
            (IpAddr::V6(net), IpAddr::V6(ip)) => mask_v6(ip, self.prefix_len) == net,
            _ => false,
        }
    }
}

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
    /// Trusted CIDR allowlist for self-registered `webhook_url` hosts (task_1492). The board POSTs
    /// event data to each agent's `webhook_url`, so the host is attacker-influencable and must be an
    /// outbound-push target the deployment trusts. The guard is ALLOWLIST-ONLY: a webhook_url whose
    /// host IP falls within one of these CIDRs is ACCEPTED and every other host is REJECTED --
    /// including public/external IPs, link-local/cloud-metadata (169.254.0.0/16), and non-IP DNS
    /// names. The fleet is private-only and co-resident with the board, so its private-LAN + loopback
    /// ranges must be listed here (that is the whole point of the default). An entry that is not a
    /// valid `<ip>/<prefix>` CIDR fails startup loudly. Default: the loopback + RFC1918 private
    /// ranges (see `DEFAULT_WEBHOOK_ALLOWED_CIDRS`); link-local/metadata is intentionally excluded.
    pub webhook_allowed_cidrs: Vec<String>,
    /// Retention window, in seconds, for ACKED (read) inbox rows (task_1491). A bounded, LOSSLESS
    /// sweep deletes inbox rows whose `read_at` is set AND older than this window, so the inbox
    /// table does not grow without bound. UNREAD rows (`read_at IS NULL`) are NEVER deleted
    /// regardless of age -- an unread row is a pending wake, and unread rows are the sole driver of
    /// wake + replay-on-reconnect (the no-lost-wake invariant). The full audit trail stays in the
    /// `events` table (never pruned here), so dropping old acked rows loses nothing. Default
    /// 604800 (7 days); set to 0 to disable the prune and keep every acked row.
    pub inbox_acked_retention_secs: i64,
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
    /// Enable the authenticated DB-snapshot download endpoint (`GET /api/admin/db-snapshot`).
    ///
    /// Default `false`: the endpoint 404s (hides its existence) unless explicitly turned on. When
    /// on, it serves a point-in-time-CONSISTENT copy of the SQLite database (produced via
    /// `VACUUM INTO`, so never a torn mid-write byte stream) behind HTTP Basic auth -- the daemon
    /// owns `db_path`, so an authed GET sidesteps the stop-daemon / cross-user file-copy problem for
    /// host migration + DR. The whole fleet's data lives in that file (including secret-broker
    /// records), so this is a full-exfil surface: the DEPLOYMENT must keep it loopback/LAN-bound and
    /// OFF the public tunnel. Requires `db_snapshot_user` + `db_snapshot_password` when enabled.
    pub db_snapshot_enabled: bool,
    /// HTTP Basic-auth username for the DB-snapshot endpoint. Required when it is enabled.
    pub db_snapshot_user: Option<String>,
    /// HTTP Basic-auth password for the DB-snapshot endpoint. Required when it is enabled; deliver it
    /// via the deployment's secret manager (agenix), never role-plaintext.
    pub db_snapshot_password: Option<String>,
    /// Per-host auth, keyed by the inbound `Host` authority's hostname (port stripped,
    /// case-insensitive). A host whose section sets `auth_header` FORCES the acting principal of a
    /// write to that header's value -- the client cannot attribute the write to anyone else
    /// (`actor`/`author`/`sender`/`created_by`/`from_agent`/`invited_by` are overwritten), and a
    /// write MISSING the header is rejected (401). A host with no `auth_header`, or any host not
    /// listed, stays permissive (the client sets its own actor). So a tunnel-fronted public
    /// hostname can force identity while localhost stays open for dev -- loopback is no longer
    /// special-cased, it just has no `auth_header`. Named `hosts` (plural) because `host` above is
    /// the bind address. Example TOML:
    ///   [hosts.'127.0.0.1']            # permissive -- no auth_header
    ///   [hosts.'board.example.com']
    ///   auth_header = "x-tunnel-user"
    #[serde(default)]
    pub hosts: HashMap<String, HostAuth>,
    /// Deployment-defined link-tag rules (task_1243): each a regex `pattern` + a `url_template` with
    /// `$1`, `$2`, ... capture substitutions. The UI linkifies matches of these patterns in rendered
    /// content (generalizing the built-in typed-ref linkification to custom refs like CR-NNNN). The
    /// patterns live HERE in the deployment config, never in source, so each deployment customizes
    /// its own tags and the open-source platform stays org-agnostic. Empty by default. Example TOML:
    ///   [[link_rules]]
    ///   pattern = "CR-(\\d+)"
    ///   url_template = "https://code.example.com/reviews/CR-$1"
    #[serde(default)]
    pub link_rules: Vec<LinkRule>,
}

/// A single deployment-configured link-tag rule (task_1243). See `Settings::link_rules`.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct LinkRule {
    /// A regex matched against rendered content; capture groups feed `url_template`.
    pub pattern: String,
    /// The link target, with `$1`, `$2`, ... replaced by the pattern's capture groups.
    pub url_template: String,
}

/// Per-host auth settings (a `[hosts.'<name>']` TOML section). Extensible; only `auth_header` today.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct HostAuth {
    /// Request header to force the acting username from, for requests whose `Host` matches this
    /// section. Absent -> this host is permissive (the client sets its own actor).
    pub auth_header: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            db_path: "/data/task-board/board.db".to_string(),
            host: "0.0.0.0".to_string(),
            port: 8079,
            webhook_timeout_secs: 5.0,
            webhook_allowed_cidrs: DEFAULT_WEBHOOK_ALLOWED_CIDRS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            inbox_acked_retention_secs: 7 * 24 * 60 * 60,
            mcp_allowed_hosts: Vec::new(),
            ipfs_api_url: None,
            db_snapshot_enabled: false,
            db_snapshot_user: None,
            db_snapshot_password: None,
            hosts: HashMap::new(),
            link_rules: Vec::new(),
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
    /// Trusted CIDR allowlist for self-registered `webhook_url` hosts (task_1492), parsed from
    /// `Settings::webhook_allowed_cidrs` and validated at load. ALLOWLIST-ONLY: a webhook host IP is
    /// accepted iff it falls within one of these, every other host rejected. See the Settings field.
    pub webhook_allowed_cidrs: Vec<Cidr>,
    /// Retention window, in seconds, for ACKED inbox rows (task_1491). See
    /// `Settings::inbox_acked_retention_secs`. 0 disables the prune.
    pub inbox_acked_retention_secs: i64,
    /// Directory of built web UI assets to serve at `/`, if present.
    pub web_dir: Option<String>,
    /// `Host` authorities accepted by the MCP endpoint. Empty == rmcp's loopback-only
    /// default; `["*"]` == accept any host. See `Settings::mcp_allowed_hosts`.
    pub mcp_allowed_hosts: Vec<String>,
    /// Optional IPFS HTTP API base URL for server-side content-addressing. See
    /// `Settings::ipfs_api_url`. `None` keeps the board CID-only.
    pub ipfs_api_url: Option<String>,
    /// Enable the authenticated DB-snapshot download endpoint. See `Settings::db_snapshot_enabled`.
    pub db_snapshot_enabled: bool,
    /// Basic-auth username for the DB-snapshot endpoint. See `Settings::db_snapshot_user`.
    pub db_snapshot_user: Option<String>,
    /// Basic-auth password for the DB-snapshot endpoint. See `Settings::db_snapshot_password`.
    pub db_snapshot_password: Option<String>,
    /// Per-host auth, keyed by hostname. A host with an `auth_header` forces the acting user from
    /// that header on writes; unlisted hosts (and hosts without `auth_header`) stay permissive. See
    /// `Settings::hosts`.
    pub hosts: HashMap<String, HostAuth>,
    /// Deployment-defined link-tag rules (regex pattern -> URL template), served to the UI for
    /// custom-ref linkification. See `Settings::link_rules`. Each pattern is validated to compile as
    /// a regex at startup (a bad pattern fails the load loudly).
    pub link_rules: Vec<LinkRule>,
}

impl Config {
    /// Load settings from a TOML file, then attach the (non-TOML) `web_dir`.
    pub fn load(path: &Path, web_dir: Option<String>) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading config file {}: {e}", path.display()))?;
        let settings: Settings = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing config file {}: {e}", path.display()))?;
        let cfg = Self::from_settings(settings, web_dir)
            .map_err(|e| anyhow::anyhow!("invalid configuration in {}: {e}", path.display()))?;
        // Fail the load loudly on a bad link-tag pattern (task_1243) rather than silently serving a
        // rule the UI cannot apply.
        for rule in &cfg.link_rules {
            regex::Regex::new(&rule.pattern).map_err(|e| {
                anyhow::anyhow!(
                    "invalid link_rules pattern {:?} in {}: {e}",
                    rule.pattern,
                    path.display()
                )
            })?;
        }
        Ok(cfg)
    }

    /// Build a config from built-in defaults (used when no `--config` is given). The defaults are
    /// known-good, so the fallible settings resolution cannot fail here.
    pub fn defaults(web_dir: Option<String>) -> Self {
        Self::from_settings(Settings::default(), web_dir).expect("built-in defaults are valid")
    }

    fn from_settings(s: Settings, web_dir: Option<String>) -> anyhow::Result<Self> {
        // task_1492: parse the webhook_url allowlist up front so an invalid CIDR fails startup
        // loudly (like link_rules) instead of silently dropping -- a dropped entry would reject a
        // legitimate fleet host and crash-loop the worker it belongs to.
        let webhook_allowed_cidrs = s
            .webhook_allowed_cidrs
            .iter()
            .map(|c| {
                Cidr::parse(c)
                    .map_err(|e| anyhow::anyhow!("invalid webhook_allowed_cidrs entry {c:?}: {e}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            db_path: s.db_path,
            host: s.host,
            port: s.port,
            webhook_timeout: Duration::from_secs_f64(s.webhook_timeout_secs),
            webhook_allowed_cidrs,
            inbox_acked_retention_secs: s.inbox_acked_retention_secs,
            web_dir: web_dir.filter(|s| !s.is_empty()),
            mcp_allowed_hosts: s.mcp_allowed_hosts,
            ipfs_api_url: s.ipfs_api_url.filter(|s| !s.is_empty()),
            db_snapshot_enabled: s.db_snapshot_enabled,
            db_snapshot_user: s.db_snapshot_user.filter(|s| !s.is_empty()),
            db_snapshot_password: s.db_snapshot_password.filter(|s| !s.is_empty()),
            hosts: s.hosts,
            link_rules: s.link_rules,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_parses_link_rules_and_rejects_a_bad_pattern() {
        // task_1243: valid [[link_rules]] parse through; an un-compilable regex fails the load.
        let tmp = tempfile::tempdir().unwrap();
        let good = tmp.path().join("good.toml");
        std::fs::write(
            &good,
            "[[link_rules]]\npattern = 'CR-(\\d+)'\nurl_template = 'https://code.example.com/reviews/CR-$1'\n",
        )
        .unwrap();
        let cfg = Config::load(&good, None).expect("valid link_rules parse");
        assert_eq!(cfg.link_rules.len(), 1);
        assert_eq!(cfg.link_rules[0].pattern, "CR-(\\d+)");
        assert_eq!(
            cfg.link_rules[0].url_template,
            "https://code.example.com/reviews/CR-$1"
        );

        let bad = tmp.path().join("bad.toml");
        std::fs::write(
            &bad,
            "[[link_rules]]\npattern = 'CR-([0-9'\nurl_template = 'https://x'\n",
        )
        .unwrap();
        let err = Config::load(&bad, None).unwrap_err().to_string();
        assert!(err.contains("invalid link_rules pattern"), "got: {err}");
    }

    #[test]
    fn cidr_parse_and_contains() {
        // task_1492: a parsed CIDR masks the network and matches only same-family addresses within.
        let c = Cidr::parse("172.16.0.0/12").unwrap();
        assert!(c.contains("172.23.235.215".parse().unwrap()));
        assert!(c.contains("172.16.0.1".parse().unwrap()));
        assert!(!c.contains("172.32.0.1".parse().unwrap()));
        assert!(!c.contains("10.0.0.1".parse().unwrap()));
        // A v4 CIDR never matches a v6 address and vice-versa.
        assert!(!c.contains("::1".parse().unwrap()));
        let v6 = Cidr::parse("fe80::/10").unwrap();
        assert!(v6.contains("fe80::1".parse().unwrap()));
        assert!(!v6.contains("127.0.0.1".parse().unwrap()));
        // /8 host-bit masking: any 10.x is inside 10.0.0.0/8.
        let c8 = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c8.contains("10.2.21.150".parse().unwrap()));
    }

    #[test]
    fn config_defaults_carry_the_fleet_lan_allowlist() {
        // The built-in default allowlist is loopback + the three RFC1918 ranges, parsed.
        let cfg = Config::defaults(None);
        assert_eq!(cfg.webhook_allowed_cidrs, default_webhook_allowed_cidrs());
        assert_eq!(cfg.webhook_allowed_cidrs.len(), 4);
        // link-local / metadata is NOT in the default -> no default CIDR contains it.
        let meta: std::net::IpAddr = "169.254.169.254".parse().unwrap();
        assert!(!cfg.webhook_allowed_cidrs.iter().any(|c| c.contains(meta)));
    }

    #[test]
    fn load_rejects_an_invalid_webhook_cidr_loudly() {
        // task_1492: a malformed webhook_allowed_cidrs entry must fail the load, not drop silently.
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("bad-cidr.toml");
        std::fs::write(
            &bad,
            "webhook_allowed_cidrs = [\"10.0.0.0/8\", \"not-a-cidr\"]\n",
        )
        .unwrap();
        let err = Config::load(&bad, None).unwrap_err().to_string();
        assert!(
            err.contains("invalid webhook_allowed_cidrs entry"),
            "got: {err}"
        );

        let bad2 = tmp.path().join("bad-prefix.toml");
        std::fs::write(&bad2, "webhook_allowed_cidrs = [\"10.0.0.0/40\"]\n").unwrap();
        let err2 = Config::load(&bad2, None).unwrap_err().to_string();
        assert!(
            err2.contains("invalid webhook_allowed_cidrs entry"),
            "got: {err2}"
        );
    }
}
