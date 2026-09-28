//! SQLite storage: schema + a shared connection pool. WAL mode so readers never
//! block the single writer. A faithful port of the Python `board.db` schema.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;

pub type Pool = SqlitePool;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agents (
    id             TEXT PRIMARY KEY,
    display_name   TEXT,
    kind           TEXT,
    status         TEXT NOT NULL DEFAULT 'offline',
    status_message TEXT,
    webhook_url    TEXT,
    created_at     TEXT NOT NULL,
    last_seen      TEXT
);
CREATE TABLE IF NOT EXISTS projects (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    description TEXT,
    status      TEXT NOT NULL DEFAULT 'active',
    created_by  TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS tasks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  INTEGER NOT NULL REFERENCES projects(id),
    title       TEXT NOT NULL,
    description TEXT,
    status      TEXT NOT NULL DEFAULT 'todo',
    priority    TEXT,
    assignee    TEXT,
    created_by  TEXT,
    metadata    TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS comments (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id    INTEGER NOT NULL REFERENCES tasks(id),
    author     TEXT,
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS subscriptions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    subscriber  TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_id   INTEGER NOT NULL,
    created_at  TEXT NOT NULL,
    UNIQUE(subscriber, target_type, target_id)
);
CREATE TABLE IF NOT EXISTS events (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    type       TEXT NOT NULL,
    actor      TEXT,
    project_id INTEGER,
    task_id    INTEGER,
    data       TEXT,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS inbox (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    recipient  TEXT NOT NULL,
    event_seq  INTEGER NOT NULL REFERENCES events(seq),
    created_at TEXT NOT NULL,
    read_at    TEXT
);
CREATE INDEX IF NOT EXISTS idx_inbox_unread  ON inbox(recipient, read_at);
CREATE INDEX IF NOT EXISTS idx_tasks_project ON tasks(project_id);
CREATE INDEX IF NOT EXISTS idx_comments_task ON comments(task_id);
CREATE INDEX IF NOT EXISTS idx_subs_target   ON subscriptions(target_type, target_id);
"#;

/// Open (creating if needed) the pool and apply the schema. WAL + foreign keys on.
pub async fn init(db_path: &str) -> anyhow::Result<Pool> {
    if let Some(parent) = std::path::Path::new(db_path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{db_path}"))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePoolOptions::new().max_connections(8).connect_with(opts).await?;

    // executescript-equivalent: split on ';' and run each statement.
    for stmt in SCHEMA.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            sqlx::query(s).execute(&pool).await?;
        }
    }
    Ok(pool)
}
