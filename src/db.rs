//! SQLite storage: schema + a shared connection pool. WAL mode so readers never
//! block the single writer. A faithful port of the Python `board.db` schema.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;

pub type Pool = SqlitePool;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agents (
    id             TEXT PRIMARY KEY,
    display_name   TEXT,
    kind           TEXT,
    status         TEXT NOT NULL DEFAULT 'offline',
    status_message TEXT,
    charter        TEXT,
    metadata       TEXT NOT NULL DEFAULT '{}',
    webhook_url    TEXT,
    created_at     TEXT NOT NULL,
    last_seen      TEXT
);
CREATE TABLE IF NOT EXISTS projects (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    description TEXT,
    status      TEXT NOT NULL DEFAULT 'active',
    metadata    TEXT NOT NULL DEFAULT '{}',
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
    parent_id   INTEGER REFERENCES tasks(id),
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
CREATE TABLE IF NOT EXISTS channels (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL,
    topic       TEXT,
    status      TEXT NOT NULL DEFAULT 'active',
    -- private channels (incl. DMs) are hidden from list_channels for non-members.
    private     INTEGER NOT NULL DEFAULT 0,
    -- Canonical key for a 1:1 direct-message channel (the two agent ids, sorted, joined by a
    -- NUL). NULL for ordinary named channels. UNIQUE so a DM pair resolves to one channel
    -- regardless of who opens it first. This is how DMs reuse the channel data model.
    dm_key      TEXT UNIQUE,
    metadata    TEXT NOT NULL DEFAULT '{}',
    created_by  TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
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
    channel_id INTEGER,
    document_id INTEGER,
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
-- Documents: publishable, versioned content. The board stores only the content IDENTIFIER
-- (a bare CID) plus metadata. The bytes live on IPFS and the CID is resolved by the client,
-- never the board (content addressing keeps the identifier location-independent). Each
-- version is an immutable row.
CREATE TABLE IF NOT EXISTS documents (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    title               TEXT NOT NULL,
    slug                TEXT,
    project_id          INTEGER REFERENCES projects(id),
    status              TEXT NOT NULL DEFAULT 'draft',
    current_version_id  INTEGER REFERENCES document_versions(id),
    approved_version_id INTEGER REFERENCES document_versions(id),
    approved_by         TEXT,
    metadata            TEXT NOT NULL DEFAULT '{}',
    created_by          TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS document_versions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    document_id INTEGER NOT NULL REFERENCES documents(id),
    version_no  INTEGER NOT NULL,
    cid         TEXT NOT NULL,
    summary     TEXT,
    created_by  TEXT,
    created_at  TEXT NOT NULL,
    UNIQUE(document_id, version_no)
);
-- Comments on a document, optionally anchored to a region of a specific (immutable) version.
-- region is a JSON string of W3C/Hypothesis-style selectors (NULL for a doc-level comment).
-- reply_to gives one-level threading, like channel posts.
CREATE TABLE IF NOT EXISTS document_comments (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    document_id INTEGER NOT NULL REFERENCES documents(id),
    version_id  INTEGER REFERENCES document_versions(id),
    author      TEXT,
    body        TEXT NOT NULL,
    region      TEXT,
    status      TEXT NOT NULL DEFAULT 'open',
    reply_to    INTEGER REFERENCES document_comments(id),
    created_at  TEXT NOT NULL
);
-- Many-to-many links between documents and tasks (a design doc can back several tasks).
CREATE TABLE IF NOT EXISTS document_attachments (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    document_id INTEGER NOT NULL REFERENCES documents(id),
    task_id     INTEGER NOT NULL REFERENCES tasks(id),
    created_at  TEXT NOT NULL,
    UNIQUE(document_id, task_id)
);
-- External identities: humans/actors that originate from a bridged external system (Slack,
-- GitHub, ...), kept DISTINCT from fleet `agents`. The id is namespaced `source:handle`
-- (e.g. "slack:U123ABC"). An ingested post/comment records its external author here so it
-- renders as that person, not as the fleet agent that performed the ingest. Shared by every
-- bridge (Slack, GitHub) — build once.
CREATE TABLE IF NOT EXISTS external_identities (
    id           TEXT PRIMARY KEY,
    source       TEXT NOT NULL,
    display_name TEXT,
    metadata     TEXT NOT NULL DEFAULT '{}',
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
-- Durable link between a board task and an external/internal SOURCE it mirrors (a promoted
-- channel thread, a bridged GitHub issue, ...). Adapter-agnostic: `source_kind` names the kind
-- (e.g. "channel_thread") and `source_id` is that source's canonical key. UNIQUE(kind,id) makes
-- promotion/import idempotent — one source maps to exactly one task. Imported/synced items carry
-- their own origin id (see comments.origin_ref) so bidirectional sync never re-mirrors.
CREATE TABLE IF NOT EXISTS task_links (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id     INTEGER NOT NULL REFERENCES tasks(id),
    source_kind TEXT NOT NULL,
    source_id   TEXT NOT NULL,
    metadata    TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL,
    UNIQUE(source_kind, source_id)
);
CREATE INDEX IF NOT EXISTS idx_inbox_unread  ON inbox(recipient, read_at);
CREATE INDEX IF NOT EXISTS idx_tasks_project ON tasks(project_id);
CREATE INDEX IF NOT EXISTS idx_comments_task ON comments(task_id);
CREATE INDEX IF NOT EXISTS idx_subs_target   ON subscriptions(target_type, target_id);
CREATE INDEX IF NOT EXISTS idx_docs_project  ON documents(project_id);
CREATE INDEX IF NOT EXISTS idx_docversions   ON document_versions(document_id, version_no);
CREATE INDEX IF NOT EXISTS idx_doc_comments  ON document_comments(document_id, id);
CREATE INDEX IF NOT EXISTS idx_doc_attach_task ON document_attachments(task_id);
CREATE INDEX IF NOT EXISTS idx_doc_attach_doc  ON document_attachments(document_id);
CREATE INDEX IF NOT EXISTS idx_ext_ident_source ON external_identities(source);
CREATE INDEX IF NOT EXISTS idx_task_links_task ON task_links(task_id);
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

    // Migration: back-fill tasks.metadata on a DB created before it existed (CREATE
    // TABLE IF NOT EXISTS won't add columns to an existing table). Mirrors the original
    // Python init_db, so the Rust impl can open an old board.db in place.
    let has_metadata = sqlx::query("PRAGMA table_info(tasks)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "metadata");
    if !has_metadata {
        sqlx::query("ALTER TABLE tasks ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}'")
            .execute(&pool)
            .await?;
    }

    // Back-fill tasks.parent_id (added when tasks gained nesting/epics). Nullable, self-
    // referential; pre-existing tasks are top-level (NULL parent) until reparented.
    let tasks_have_parent = sqlx::query("PRAGMA table_info(tasks)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "parent_id");
    if !tasks_have_parent {
        sqlx::query("ALTER TABLE tasks ADD COLUMN parent_id INTEGER REFERENCES tasks(id)")
            .execute(&pool)
            .await?;
    }
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_id)")
        .execute(&pool)
        .await?;

    // Same back-fill for projects.metadata (added when projects gained arbitrary props, e.g.
    // a repo link). An old board.db created before it keeps working: existing rows default
    // to '{}'.
    let projects_have_metadata = sqlx::query("PRAGMA table_info(projects)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "metadata");
    if !projects_have_metadata {
        sqlx::query("ALTER TABLE projects ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}'")
            .execute(&pool)
            .await?;
    }

    // Back-fill agents.charter (free-form role/mission text, editable over time). Nullable,
    // so pre-existing agents simply have no charter until they set one.
    let agents_have_charter = sqlx::query("PRAGMA table_info(agents)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "charter");
    if !agents_have_charter {
        sqlx::query("ALTER TABLE agents ADD COLUMN charter TEXT")
            .execute(&pool)
            .await?;
    }

    // Back-fill agents.metadata (an arbitrary props bag, mirroring projects/tasks). This is
    // what lets the board's agent list serve as the fleet registry: role, model, effort,
    // interval, worktree, area, and `repos: [{repo, branch}, ...]` (an agent may span
    // several repos, each checked out in its own workspace) all live here. Defaults to '{}'.
    let agents_have_metadata = sqlx::query("PRAGMA table_info(agents)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "metadata");
    if !agents_have_metadata {
        sqlx::query("ALTER TABLE agents ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}'")
            .execute(&pool)
            .await?;
    }

    // Back-fill events.channel_id (added when channels landed). Nullable; pre-existing task/
    // project events simply have no channel. Channel posts set it so get_channel_posts can
    // read a channel's backlog directly. `channels` itself is created by CREATE TABLE above,
    // so only the events column needs an explicit ALTER on an old DB.
    let events_have_channel = sqlx::query("PRAGMA table_info(events)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "channel_id");
    if !events_have_channel {
        sqlx::query("ALTER TABLE events ADD COLUMN channel_id INTEGER")
            .execute(&pool)
            .await?;
    }
    // Index for channel-post backlog reads. Created after the column exists (an old DB adds
    // it via the ALTER just above; a fresh DB via CREATE TABLE), so it's safe either way.
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_events_channel ON events(channel_id, seq)")
        .execute(&pool)
        .await?;

    // Back-fill events.document_id (added when documents became subscribable). Nullable, like
    // channel_id above. Document events set it so a subscriber can trace a doc's activity.
    let events_have_document = sqlx::query("PRAGMA table_info(events)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "document_id");
    if !events_have_document {
        sqlx::query("ALTER TABLE events ADD COLUMN document_id INTEGER")
            .execute(&pool)
            .await?;
    }
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_events_document ON events(document_id, seq)")
        .execute(&pool)
        .await?;

    // Back-fill comments.external_author (added for bridged/ingested attribution): when set, it
    // holds an external_identities id so the comment renders as that person, not the fleet agent
    // that ingested it. Nullable; existing comments stay agent-authored.
    let comments_have_ext_author = sqlx::query("PRAGMA table_info(comments)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "external_author");
    if !comments_have_ext_author {
        sqlx::query("ALTER TABLE comments ADD COLUMN external_author TEXT")
            .execute(&pool)
            .await?;
    }

    // Back-fill comments.origin_ref (added for imported/synced comments): the origin id of the
    // source item a comment mirrors (e.g. the source post seq of a promoted thread reply), so a
    // thread↔task link can dedup and never re-mirror. Nullable; native comments leave it NULL.
    let comments_have_origin_ref = sqlx::query("PRAGMA table_info(comments)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "origin_ref");
    if !comments_have_origin_ref {
        sqlx::query("ALTER TABLE comments ADD COLUMN origin_ref TEXT")
            .execute(&pool)
            .await?;
    }

    Ok(pool)
}
