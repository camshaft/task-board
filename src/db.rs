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
    -- What a BLOCKED task is waiting on (operator seq-1361), so nothing sits blocked opaquely.
    -- blocked_on_kind is one of task, agent, operator (NULL when the task is not blocked).
    -- blocked_on_ref is the blocking task id (as text) or agent id, NULL for operator. note is
    -- free text. A blocked task must carry a kind (enforced in update_task).
    blocked_on_kind TEXT,
    blocked_on_ref  TEXT,
    blocked_on_note TEXT,
    -- Soft-archive stamp (mirrors documents.archived_at): a retired task drops out of list_tasks
    -- by default but stays queryable. Orthogonal to status; NULL = not archived.
    archived_at TEXT,
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
    -- When 1, every agent is a member: existing agents are joined when the flag is set, and each
    -- newly-registered agent auto-joins on register. For a fleet-wide broadcast channel.
    auto_join   INTEGER NOT NULL DEFAULT 0,
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
-- Per-agent-per-task mute: an agent in a task's fan-out (creator/assignee/subscriber) can
-- detach from that task's event notifications. Subtracted from the task recipient set so a
-- stood-down owner stops getting FYI wakes on a task they opened (unsubscribe can't, since the
-- creator is in the fan-out independent of a subscription row). UNIQUE keeps mute idempotent.
CREATE TABLE IF NOT EXISTS task_mutes (
    task_id    INTEGER NOT NULL REFERENCES tasks(id),
    agent      TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(task_id, agent)
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
    -- Optional wiki path (e.g. architecture/board/events). Nullable: a doc can exist unfiled.
    -- Uniqueness among non-null paths is enforced by a partial unique index (see below).
    path                TEXT,
    project_id          INTEGER REFERENCES projects(id),
    status              TEXT NOT NULL DEFAULT 'draft',
    current_version_id  INTEGER REFERENCES document_versions(id),
    approved_version_id INTEGER REFERENCES document_versions(id),
    approved_by         TEXT,
    metadata            TEXT NOT NULL DEFAULT '{}',
    -- Soft-archive stamp. NULL = live; a timestamp = retired (hidden from listings by default,
    -- reversible, and the append-only event log is preserved). Orthogonal to the review status.
    archived_at         TEXT,
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
    -- MIME type of the bytes the CID points at (e.g. text/markdown, image/png, application/pdf).
    -- The board records only the label and never fetches/transcodes -- rendering is the client's job.
    content_type TEXT NOT NULL DEFAULT 'text/markdown',
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
    created_at  TEXT NOT NULL,
    external_author TEXT
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
-- Generic mapping between a board entity and an entity in a bridged external system: the ONE
-- link model behind the Slack channel-map, the GitHub issue-to-task bridge, and thread-to-task
-- promotion. source names the system (slack, github). external_id is that system's canonical key
-- (a Slack channel id, a thread ts, an issue url). external_parent_id is an optional container
-- (e.g. the Slack channel of a thread). board_kind is channel, task, or thread and board_id is
-- the board-side id. UNIQUE(source, external_id) keeps the mapping idempotent -- one external
-- entity maps to one board entity per system.
-- (This SCHEMA is applied statement-by-statement by split_schema_statements, which strips these
-- comments before splitting on the semicolon, so a comment may safely contain one.)
CREATE TABLE IF NOT EXISTS external_links (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    source             TEXT NOT NULL,
    external_id        TEXT NOT NULL,
    external_parent_id TEXT,
    board_kind         TEXT NOT NULL,
    board_id           INTEGER NOT NULL,
    metadata           TEXT NOT NULL DEFAULT '{}',
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL,
    UNIQUE(source, external_id)
);
-- [[wiki-link]] edges between documents (a jump). One row per distinct target_path a source
-- links to. We store only the raw target_path (and optional |label), never a resolved id -- the
-- target is resolved at read time by joining on documents.path, so edges stay correct as docs
-- are filed, renamed, or unfiled (a link can also dangle, pointing at a path nothing occupies
-- yet). Edges are recomputed from content whenever a version is published WITH raw content (the
-- board only sees a CID otherwise, so a CID-only publish leaves prior edges as-is). The kind /
-- target_version_id / region columns are legacy (embeds moved to document_embeds so a doc can
-- both link AND embed the same path -- see task 108) and are effectively always link/null now.
CREATE TABLE IF NOT EXISTS document_links (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    source_document_id INTEGER NOT NULL REFERENCES documents(id),
    target_path        TEXT NOT NULL,
    label              TEXT,
    kind               TEXT NOT NULL DEFAULT 'link',
    target_version_id  INTEGER REFERENCES document_versions(id),
    region             TEXT,
    created_at         TEXT NOT NULL,
    UNIQUE(source_document_id, target_path)
);
-- ![[transclusion]] edges (embed one doc's content inside another, rendered in place). Kept in a
-- SEPARATE table from links so a document can BOTH link and embed the same target_path without
-- the (source, target_path) uniqueness colliding (task 108). Same resolve-at-read-time model as
-- links. target_version_id pins the embed to an immutable version (NULL = floats to the target's
-- current version) and region holds an optional raw #fragment/selector for a partial embed.
CREATE TABLE IF NOT EXISTS document_embeds (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    source_document_id INTEGER NOT NULL REFERENCES documents(id),
    target_path        TEXT NOT NULL,
    label              TEXT,
    target_version_id  INTEGER REFERENCES document_versions(id),
    region             TEXT,
    created_at         TEXT NOT NULL,
    UNIQUE(source_document_id, target_path)
);
-- A named, reusable WORKSPACE KIND: the setup/checkout script + config an agent is configured
-- with when its workspace is materialized. Environment-specific setup lives here as board DATA,
-- so fleet spin-up can support custom environment kinds defined in board resources and stay
-- generic. `name` is the key an agent's metadata.workspace_kind references; `setup_script` is run
-- to materialize the workspace; `config` is a free-form JSON bag of hints the consumer reads
-- (cwd, launch, repo, branch, env, ...).
CREATE TABLE IF NOT EXISTS workspace_kinds (
    name         TEXT PRIMARY KEY,
    setup_script TEXT NOT NULL DEFAULT '',
    config       TEXT NOT NULL DEFAULT '{}',
    description  TEXT,
    created_by   TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
-- A maintained list of BANNED PHRASES: jargon/idioms the fleet has agreed not to use in docs and
-- comments. Data-driven so the list grows without a code change; the pre-submit scanner checks
-- authored content against it (case-insensitive, whole-word). `phrase` is stored lowercased and is
-- the key; `note` optionally explains why it's banned or what to write instead.
CREATE TABLE IF NOT EXISTS banned_phrases (
    phrase     TEXT PRIMARY KEY,
    note       TEXT,
    created_by TEXT,
    created_at TEXT NOT NULL
);
-- SECRET REQUESTS: the board is an ephemeral secret-REQUEST broker, never a secret store. An agent
-- files a named request carrying the (non-secret) age recipient pubkeys + human instructions; an
-- operator opens a single-use capability link and submits the value ENCRYPTED IN THE BROWSER, so
-- the board only ever holds ciphertext, and only transiently. A fulfiller pulls the ciphertext once
-- (fulfiller-token-gated), relocates it to durable storage, then the row is deleted. `ciphertext`
-- is NULL except in the submit->fulfill window; the row is hard-deleted on fulfill/cancel and a
-- stuck submitted row is purged after `expires_at`. Nothing here is content-addressed or put on the
-- firehose. `recipients` is a JSON array of age recipient pubkey strings; `target` is an advisory
-- placement hint the fulfiller owns; `submit_used` enforces the single-use submit link.
CREATE TABLE IF NOT EXISTS secret_requests (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT NOT NULL,
    requested_by    TEXT,
    fulfiller       TEXT,
    status          TEXT NOT NULL DEFAULT 'requested',
    recipients      TEXT NOT NULL DEFAULT '[]',
    instructions    TEXT,
    target          TEXT,
    submit_token    TEXT NOT NULL,
    fulfiller_token TEXT NOT NULL,
    submit_used     INTEGER NOT NULL DEFAULT 0,
    ciphertext      TEXT,
    created_at      TEXT NOT NULL,
    submitted_at    TEXT,
    expires_at      TEXT
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
CREATE INDEX IF NOT EXISTS idx_external_links_board ON external_links(board_kind, board_id);
-- NOTE: the unique index on documents(path) is intentionally NOT here. path is a back-filled
-- column (added by an ALTER in init after this SCHEMA runs), so on a pre-path DB an index over
-- documents(path) in the SCHEMA apply loop fails with "no such column: path" and crash-loops the
-- process. It is created after the back-fill instead (see init), which is correct for fresh and
-- existing DBs alike. Any future index/constraint on a back-filled column must follow the same rule.
-- (document_links is a brand-new table with no back-filled columns, so indexing it here is safe.)
CREATE INDEX IF NOT EXISTS idx_doclinks_source ON document_links(source_document_id);
CREATE INDEX IF NOT EXISTS idx_doclinks_target ON document_links(target_path);
CREATE INDEX IF NOT EXISTS idx_docembeds_source ON document_embeds(source_document_id);
CREATE INDEX IF NOT EXISTS idx_docembeds_target ON document_embeds(target_path);
"#;

/// Split the embedded SCHEMA into individual statements for the init apply loop (sqlx has no
/// multi-statement execute). Strips `--` line comments FIRST, then splits on ';'. Doing the
/// strip before the split is what lets a comment safely contain a semicolon: historically a `;`
/// inside a `-- comment` truncated the CREATE statement mid-definition and broke init with a
/// cryptic "near ...: syntax error" (a trap that bit repeatedly). Assumes no `--` appears inside
/// a string literal in the schema — none does, it is plain DDL; revisit if that ever changes.
fn split_schema_statements(schema: &str) -> Vec<String> {
    let without_comments: String = schema
        .lines()
        .map(|line| match line.find("--") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    without_comments
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Open (creating if needed) the pool and apply the schema. WAL + foreign keys on.
pub async fn init(db_path: &str) -> anyhow::Result<Pool> {
    if let Some(parent) = std::path::Path::new(db_path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // Concurrency posture (see task 191, "database is locked" under fleet load). WAL lets many
    // readers run alongside a single writer. busy_timeout makes a writer WAIT for the lock
    // instead of erroring immediately on contention -- a generous 30s absorbs bursty fleet
    // writes and WAL checkpoints. synchronous=NORMAL is the recommended WAL pairing: it fsyncs
    // far less than FULL (so the write lock is held briefly, shrinking the contention window)
    // while staying durable across an app crash (only an OS/power crash can lose the last commit,
    // acceptable for a coordination board). Transactions here are short (a few statements, and
    // webhooks fire AFTER commit), so writers release the lock quickly.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{db_path}"))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(30));

    // Single connection = serialized DB access (task 191). SQLite allows only one writer, and a
    // multi-connection pool lets two deferred transactions each take a read snapshot and then
    // race to upgrade to a write — the loser gets SQLITE_BUSY_SNAPSHOT (code 517, surfaced as
    // "database is locked") IMMEDIATELY, which busy_timeout cannot wait out. Every agent polls
    // check_notifications (a read-then-write: mark-read + last_seen) each tick, so under fleet
    // concurrency that deadlock was hitting live writes. One pooled connection means only one
    // transaction runs at a time, so there is never a competing writer to invalidate a snapshot
    // — the contention becomes a brief queue (bounded by busy_timeout), not an error. Board ops
    // are short, indexed, and hold no connection across an await (webhooks fire post-commit; SSE
    // streams from the broadcast bus, not a held connection), so serial access is fine at this
    // scale. If read throughput ever bottlenecks, the next step is a read pool + a single writer
    // connection (or per-write BEGIN IMMEDIATE), not a wider undifferentiated pool.
    let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;

    // executescript-equivalent: sqlx has no multi-statement execute, so apply the schema one
    // statement at a time (split_schema_statements strips comments, then splits on ';').
    for stmt in split_schema_statements(SCHEMA) {
        sqlx::query(&stmt).execute(&pool).await?;
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

    // Back-fill tasks.blocked_on_{kind,ref,note} (operator seq-1361: a blocked task records
    // what it's waiting on). All nullable; existing tasks carry no blocked_on until set.
    let tasks_cols = sqlx::query("PRAGMA table_info(tasks)").fetch_all(&pool).await?;
    let tasks_has = |c: &str| tasks_cols.iter().any(|r| r.get::<String, _>("name") == c);
    if !tasks_has("blocked_on_kind") {
        sqlx::query("ALTER TABLE tasks ADD COLUMN blocked_on_kind TEXT").execute(&pool).await?;
    }
    if !tasks_has("blocked_on_ref") {
        sqlx::query("ALTER TABLE tasks ADD COLUMN blocked_on_ref TEXT").execute(&pool).await?;
    }
    if !tasks_has("blocked_on_note") {
        sqlx::query("ALTER TABLE tasks ADD COLUMN blocked_on_note TEXT").execute(&pool).await?;
    }
    // Back-fill tasks.archived_at (soft-archive, mirroring documents.archived_at): a retired task
    // stays queryable but drops out of list_tasks by default. Orthogonal to status; nullable.
    if !tasks_has("archived_at") {
        sqlx::query("ALTER TABLE tasks ADD COLUMN archived_at TEXT").execute(&pool).await?;
    }

    // Back-fill channels.auto_join (fleet-wide broadcast channels): a legacy channels table opens
    // without it, defaulting every channel to opt-in membership.
    let channels_have_auto_join = sqlx::query("PRAGMA table_info(channels)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "auto_join");
    if !channels_have_auto_join {
        sqlx::query("ALTER TABLE channels ADD COLUMN auto_join INTEGER NOT NULL DEFAULT 0")
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

    // Back-fill document_comments.external_author (bridged/ingested attribution, mirroring the
    // task-comment + channel-post columns) — an ingested human's review comment renders as them.
    let doc_comments_have_ext_author = sqlx::query("PRAGMA table_info(document_comments)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "external_author");
    if !doc_comments_have_ext_author {
        sqlx::query("ALTER TABLE document_comments ADD COLUMN external_author TEXT")
            .execute(&pool)
            .await?;
    }

    // Back-fill document_versions.content_type (documents became any MIME type, not just markdown).
    // Existing versions default to text/markdown, matching the initial docs work.
    let versions_have_content_type = sqlx::query("PRAGMA table_info(document_versions)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "content_type");
    if !versions_have_content_type {
        sqlx::query("ALTER TABLE document_versions ADD COLUMN content_type TEXT NOT NULL DEFAULT 'text/markdown'")
            .execute(&pool)
            .await?;
    }

    // Back-fill documents.path (the wiki organization layer). Nullable; SQLite can't ALTER-ADD a
    // UNIQUE column, so uniqueness among non-null paths is a partial unique index (created next).
    let documents_have_path = sqlx::query("PRAGMA table_info(documents)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "path");
    if !documents_have_path {
        sqlx::query("ALTER TABLE documents ADD COLUMN path TEXT")
            .execute(&pool)
            .await?;
    }
    sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS idx_documents_path ON documents(path) WHERE path IS NOT NULL")
        .execute(&pool)
        .await?;

    // Back-fill documents.archived_at (the soft-archive/retire path). Nullable; NULL = live.
    let documents_have_archived_at = sqlx::query("PRAGMA table_info(documents)")
        .fetch_all(&pool)
        .await?
        .iter()
        .any(|r| r.get::<String, _>("name") == "archived_at");
    if !documents_have_archived_at {
        sqlx::query("ALTER TABLE documents ADD COLUMN archived_at TEXT")
            .execute(&pool)
            .await?;
    }

    // Back-fill document_links.{kind,target_version_id,region} (transclusion/embeds). A DB whose
    // document_links table was created before embeds existed keeps its rows as kind='link'. All
    // three are nullable-or-defaulted, so no index/constraint over them goes in SCHEMA (the #63
    // path-index crash-loop lesson: never index a back-filled column in the SCHEMA apply loop).
    let doclinks_cols = sqlx::query("PRAGMA table_info(document_links)").fetch_all(&pool).await?;
    let has = |c: &str| doclinks_cols.iter().any(|r| r.get::<String, _>("name") == c);
    if !has("kind") {
        sqlx::query("ALTER TABLE document_links ADD COLUMN kind TEXT NOT NULL DEFAULT 'link'")
            .execute(&pool)
            .await?;
    }
    if !has("target_version_id") {
        sqlx::query("ALTER TABLE document_links ADD COLUMN target_version_id INTEGER")
            .execute(&pool)
            .await?;
    }
    if !has("region") {
        sqlx::query("ALTER TABLE document_links ADD COLUMN region TEXT")
            .execute(&pool)
            .await?;
    }

    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for the recurring "semicolon in a schema comment" trap: a `;` inside a `--`
    /// comment used to truncate the following CREATE statement (init failed with a cryptic
    /// "near ...: syntax error"). split_schema_statements strips comments before splitting, so a
    /// comment may now contain semicolons and no comment text survives into a statement.
    #[test]
    fn split_schema_strips_comments_and_tolerates_semicolons_in_them() {
        let schema = "\
-- a leading comment; with a semicolon in it
CREATE TABLE a (id INTEGER); -- trailing; comment; here
CREATE TABLE b (id INTEGER);
-- a dangling; comment; after the last statement
";
        let stmts = split_schema_statements(schema);
        assert_eq!(stmts.len(), 2, "expected exactly two statements, got: {stmts:?}");
        assert!(stmts[0].starts_with("CREATE TABLE a"), "got: {:?}", stmts[0]);
        assert!(stmts[1].starts_with("CREATE TABLE b"), "got: {:?}", stmts[1]);
        assert!(
            stmts.iter().all(|s| !s.contains("comment")),
            "comment text leaked into a statement: {stmts:?}"
        );
        // Sanity-check the real embedded SCHEMA too: it splits into many statements and none of
        // them still carry a `--` comment marker.
        let real = split_schema_statements(SCHEMA);
        assert!(real.len() > 5, "SCHEMA should split into many statements, got {}", real.len());
        assert!(real.iter().all(|s| !s.contains("--")), "a `--` comment survived the split");
    }

    /// Regression for the #63 crash-loop: a DB whose `documents` table predates the `path`
    /// column must migrate cleanly. `path` is back-filled by an ALTER after the SCHEMA apply
    /// loop, so any index over `documents(path)` inside SCHEMA fails there with
    /// "no such column: path" and wedges the process. A fresh-DB test can't catch this — the
    /// legacy table must be seeded WITHOUT `path` first.
    #[tokio::test]
    async fn init_migrates_pre_path_documents_db() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let db_path = tmp.path().join("legacy.db");
        let dbp = db_path.to_str().unwrap();

        // Seed a pre-#63 documents table: no `path` column.
        {
            let opts = SqliteConnectOptions::from_str(&format!("sqlite://{dbp}"))?
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
            sqlx::query(
                "CREATE TABLE documents (\
                    id INTEGER PRIMARY KEY AUTOINCREMENT, \
                    title TEXT NOT NULL, \
                    slug TEXT, \
                    project_id INTEGER, \
                    status TEXT NOT NULL DEFAULT 'draft', \
                    current_version_id INTEGER, \
                    approved_version_id INTEGER, \
                    approved_by TEXT, \
                    metadata TEXT NOT NULL DEFAULT '{}', \
                    created_by TEXT, \
                    created_at TEXT NOT NULL, \
                    updated_at TEXT NOT NULL)",
            )
            .execute(&pool)
            .await?;
            pool.close().await;
        }

        // init() must succeed (this is the crash-loop that #63 introduced).
        let pool = init(dbp).await?;

        // The back-fill added documents.path...
        let has_path = sqlx::query("PRAGMA table_info(documents)")
            .fetch_all(&pool)
            .await?
            .iter()
            .any(|r| r.get::<String, _>("name") == "path");
        assert!(has_path, "init should back-fill documents.path on a legacy DB");

        // ...and the partial unique index exists (created after the back-fill).
        let has_index = sqlx::query(
            "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_documents_path'",
        )
        .fetch_optional(&pool)
        .await?
        .is_some();
        assert!(has_index, "idx_documents_path should exist after migration");

        // And it's usable: filing two docs at the same path is rejected by the unique index.
        let ts = "2026-01-01T00:00:00Z";
        for (i, p) in [("A", "a/b"), ("B", "a/b")].iter().enumerate() {
            let r = sqlx::query(
                "INSERT INTO documents(title, path, created_at, updated_at) VALUES(?,?,?,?)",
            )
            .bind(p.0)
            .bind(p.1)
            .bind(ts)
            .bind(ts)
            .execute(&pool)
            .await;
            if i == 0 {
                r.expect("first doc at a/b inserts");
            } else {
                assert!(r.is_err(), "second doc at the same path violates the unique index");
            }
        }
        Ok(())
    }
}
