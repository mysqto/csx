//! Storage and query engine.
//!
//! [`Db`] wraps a [`rusqlite::Connection`] over either an on-disk file or an
//! in-memory database. It owns the full schema (FTS5 + trigram indexes) and
//! exposes upsert/insert helpers plus the scoped [`Db::search`] API.
//!
//! All logic here is exercised against `:memory:` databases in the unit tests,
//! so nothing in this module needs a shim.

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;
use crate::model::{Account, MessageRecord, SessionMeta};

/// The complete schema. Kept in one place so opening any database yields the
/// same shape. Applied inside a transaction by [`Db::init_schema`].
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sources (
    id           INTEGER PRIMARY KEY,
    tool         TEXT,
    config_dir   TEXT,
    profile      TEXT,
    account_uuid TEXT,
    org_uuid     TEXT,
    email        TEXT,
    org          TEXT,
    kind         TEXT
);

CREATE TABLE IF NOT EXISTS files (
    id         INTEGER PRIMARY KEY,
    source_id  INTEGER,
    path       TEXT,
    file_key   TEXT UNIQUE,
    size       INTEGER,
    mtime      INTEGER,
    watermark  INTEGER DEFAULT 0,
    session_id TEXT
);

CREATE TABLE IF NOT EXISTS sessions (
    session_id   TEXT PRIMARY KEY,
    source_id    INTEGER,
    tool         TEXT,
    repo_id      TEXT,
    project_path TEXT,
    project_name TEXT,
    git_branch   TEXT,
    first_ts     INTEGER,
    last_ts      INTEGER,
    msg_count    INTEGER,
    summary      TEXT
);
CREATE INDEX IF NOT EXISTS sess_repo ON sessions(repo_id);

CREATE TABLE IF NOT EXISTS messages (
    id         INTEGER PRIMARY KEY,
    session_id TEXT,
    source_id  INTEGER,
    tool       TEXT,
    seq        INTEGER,
    ts         INTEGER,
    role       TEXT,
    tool_name  TEXT,
    uuid       TEXT
);
CREATE INDEX IF NOT EXISTS msg_scope ON messages(source_id, ts);
CREATE INDEX IF NOT EXISTS msg_session ON messages(session_id);

CREATE TABLE IF NOT EXISTS messages_text (
    id   INTEGER PRIMARY KEY,
    body TEXT
);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    body,
    content='messages_text',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 2',
    prefix='2 3 4'
);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_trg USING fts5(
    body,
    content='messages_text',
    content_rowid='id',
    tokenize='trigram'
);
"#;

/// A row identifying a source (account + tool + config location).
///
/// `id` is ignored on upsert; the caller receives the assigned id back.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceRow {
    /// Tool identifier (see [`crate::model::Tool::as_str`]).
    pub tool: Option<String>,
    /// Root config directory this source was discovered under.
    pub config_dir: Option<String>,
    /// Named profile within the tool, when applicable.
    pub profile: Option<String>,
    /// Account UUID attribution.
    pub account_uuid: Option<String>,
    /// Organization UUID attribution.
    pub org_uuid: Option<String>,
    /// Account email attribution.
    pub email: Option<String>,
    /// Organization display name.
    pub org: Option<String>,
    /// Free-form source kind marker.
    pub kind: Option<String>,
}

/// A tracked transcript file: its persisted watermark and identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    /// Primary key.
    pub id: i64,
    /// Owning source id.
    pub source_id: i64,
    /// Absolute path on disk.
    pub path: Option<String>,
    /// Stable dedupe/watermark key.
    pub file_key: String,
    /// Size in bytes at last index.
    pub size: i64,
    /// Modification time (unix seconds) at last index.
    pub mtime: i64,
    /// Opaque parse watermark.
    pub watermark: i64,
    /// Session this file belongs to, when known.
    pub session_id: Option<String>,
}

/// Scope filters for [`Db::search`]. Every field is optional; a `None` field
/// imposes no constraint. All `Some` fields are ANDed together.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Restrict to a source account UUID.
    pub account_uuid: Option<String>,
    /// Restrict to a source org UUID.
    pub org_uuid: Option<String>,
    /// Restrict to a source profile.
    pub profile: Option<String>,
    /// Restrict to a tool identifier.
    pub tool: Option<String>,
    /// Restrict to a repository id.
    pub repo_id: Option<String>,
    /// Restrict to a session project path (`sessions.project_path`).
    pub cwd: Option<String>,
    /// Restrict to a git branch.
    pub branch: Option<String>,
    /// Restrict to a single session.
    pub session_id: Option<String>,
    /// Restrict to a message role.
    pub role: Option<String>,
    /// Restrict to a message tool name.
    pub tool_name: Option<String>,
    /// Lower bound on message ts (inclusive).
    pub since: Option<i64>,
    /// Upper bound on message ts (inclusive).
    pub until: Option<i64>,
}

/// Search tuning options.
#[derive(Debug, Clone)]
pub struct SearchOpts {
    /// Use the trigram index (substring / code search) instead of the
    /// unicode61 full-text index.
    pub code: bool,
    /// Maximum number of hits to return.
    pub limit: usize,
}

impl Default for SearchOpts {
    fn default() -> Self {
        SearchOpts {
            code: false,
            limit: 20,
        }
    }
}

/// A single search result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Session the matching message belongs to.
    pub session_id: String,
    /// Tool that produced the message.
    pub tool: Option<String>,
    /// Repository id of the session, when known.
    pub repo_id: Option<String>,
    /// Human-friendly project name of the session, when known.
    pub project_name: Option<String>,
    /// Message timestamp (unix seconds).
    pub ts: i64,
    /// Highlighted excerpt from the matching body.
    pub snippet: String,
    /// Blended relevance score (higher is better).
    pub score: f64,
}

/// A per-tool/account summary row for the `list` command: one row per distinct
/// source identity, with how many sessions and messages it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSummary {
    /// Source primary key.
    pub id: i64,
    /// Tool identifier, when known.
    pub tool: Option<String>,
    /// Account email attribution, when known.
    pub email: Option<String>,
    /// Organization display name, when known.
    pub org: Option<String>,
    /// Named profile, when known.
    pub profile: Option<String>,
    /// Number of sessions owned by this source.
    pub sessions: i64,
    /// Number of messages owned by this source.
    pub messages: i64,
}

/// A session summary row for the `sessions` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// Session identifier.
    pub session_id: String,
    /// Tool that produced the session.
    pub tool: Option<String>,
    /// Repository id, when known.
    pub repo_id: Option<String>,
    /// Human-friendly project name, when known.
    pub project_name: Option<String>,
    /// Git branch active during the session, when known.
    pub git_branch: Option<String>,
    /// Earliest message timestamp (unix seconds).
    pub first_ts: i64,
    /// Latest message timestamp (unix seconds).
    pub last_ts: i64,
    /// Persisted cumulative message count.
    pub msg_count: i64,
}

/// A single message row for the `show` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    /// Monotonic sequence number within the session.
    pub seq: i64,
    /// Message timestamp (unix seconds).
    pub ts: i64,
    /// Speaker/producer role.
    pub role: Option<String>,
    /// Tool name, when `role` is `tool`.
    pub tool_name: Option<String>,
    /// Plain-text body.
    pub body: String,
}

/// The active account for a tool, for the `current` command: derived from the
/// source that owns the most recently active session for each tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentAccount {
    /// Tool identifier.
    pub tool: String,
    /// Account email attribution, when known.
    pub email: Option<String>,
    /// Organization display name, when known.
    pub org: Option<String>,
    /// Named profile, when known.
    pub profile: Option<String>,
    /// Timestamp of the most recent activity for this tool.
    pub last_ts: i64,
}

/// Storage handle wrapping a SQLite connection.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open a database at `path`, or an in-memory database when `path` is
    /// `":memory:"`. The schema is initialized eagerly.
    pub fn open(path: &str) -> Result<Db> {
        let conn = if path == ":memory:" {
            Connection::open_in_memory()?
        } else {
            Connection::open(path)?
        };
        let db = Db { conn };
        db.init_schema()?;
        Ok(db)
    }

    /// Borrow the underlying connection (read-only helper for advanced callers
    /// and tests).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Create every table and index if absent. Idempotent.
    pub fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(SCHEMA)?;
        Ok(())
    }

    /// Insert a source and return its new id.
    ///
    /// Sources are append-only identity rows; a fresh row is created on each
    /// call and its assigned id returned.
    pub fn upsert_source(&self, s: &SourceRow) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sources
                (tool, config_dir, profile, account_uuid, org_uuid, email, org, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                s.tool,
                s.config_dir,
                s.profile,
                s.account_uuid,
                s.org_uuid,
                s.email,
                s.org,
                s.kind,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Insert or replace a session's aggregate metadata.
    ///
    /// `account` fields are folded into the session's owning source columns
    /// only via [`upsert_source`]; here we persist the session row itself.
    pub fn upsert_session(
        &self,
        source_id: i64,
        meta: &SessionMeta,
        msg_count: i64,
        summary: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions
                (session_id, source_id, tool, repo_id, project_path,
                 project_name, git_branch, first_ts, last_ts, msg_count, summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(session_id) DO UPDATE SET
                source_id    = excluded.source_id,
                tool         = excluded.tool,
                repo_id      = excluded.repo_id,
                project_path = excluded.project_path,
                project_name = excluded.project_name,
                git_branch   = excluded.git_branch,
                first_ts     = MIN(sessions.first_ts, excluded.first_ts),
                last_ts      = MAX(sessions.last_ts, excluded.last_ts),
                msg_count    = excluded.msg_count,
                summary      = excluded.summary",
            params![
                meta.session_id,
                source_id,
                meta.tool.as_str(),
                meta.repo_id,
                meta.project_path,
                meta.project_name,
                meta.git_branch,
                meta.first_ts,
                meta.last_ts,
                msg_count,
                summary,
            ],
        )?;
        Ok(())
    }

    /// Insert one message, writing the `messages` metadata row, the
    /// `messages_text` external-content body, and keeping both FTS tables in
    /// sync via explicit inserts (external-content FTS is not auto-populated).
    ///
    /// Returns the new `messages.id` (== `messages_text.id`).
    pub fn insert_message(&self, source_id: i64, m: &MessageRecord) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO messages
                (session_id, source_id, tool, seq, ts, role, tool_name, uuid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                m.session_id,
                source_id,
                m.tool.as_str(),
                m.seq as i64,
                m.ts,
                m.role.as_str(),
                m.tool_name,
                m.uuid,
            ],
        )?;
        let id = self.conn.last_insert_rowid();

        // External-content body shares the rowid with `messages`.
        self.conn.execute(
            "INSERT INTO messages_text (id, body) VALUES (?1, ?2)",
            params![id, m.text],
        )?;
        // Keep both external-content FTS indexes in sync explicitly.
        self.conn.execute(
            "INSERT INTO messages_fts (rowid, body) VALUES (?1, ?2)",
            params![id, m.text],
        )?;
        self.conn.execute(
            "INSERT INTO messages_trg (rowid, body) VALUES (?1, ?2)",
            params![id, m.text],
        )?;
        Ok(id)
    }

    /// Insert or update the tracked file row keyed by `file_key`, returning its
    /// id. Existing rows keep their persisted watermark unless a later
    /// [`set_watermark`] call changes it.
    pub fn upsert_file(
        &self,
        source_id: i64,
        path: Option<&str>,
        file_key: &str,
        size: i64,
        mtime: i64,
        session_id: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO files (source_id, path, file_key, size, mtime, session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(file_key) DO UPDATE SET
                source_id  = excluded.source_id,
                path       = excluded.path,
                size       = excluded.size,
                mtime      = excluded.mtime,
                session_id = excluded.session_id",
            params![source_id, path, file_key, size, mtime, session_id],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM files WHERE file_key = ?1",
            params![file_key],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Fetch a tracked file row by its `file_key`, if present.
    pub fn get_file(&self, file_key: &str) -> Result<Option<FileRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, source_id, path, file_key, size, mtime, watermark, session_id
                 FROM files WHERE file_key = ?1",
                params![file_key],
                |r| {
                    Ok(FileRow {
                        id: r.get(0)?,
                        source_id: r.get(1)?,
                        path: r.get(2)?,
                        file_key: r.get(3)?,
                        size: r.get(4)?,
                        mtime: r.get(5)?,
                        watermark: r.get(6)?,
                        session_id: r.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Read the persisted `msg_count` for a session, or `None` if the session
    /// is not yet tracked. Used by the indexer to grow the cumulative count as
    /// new message deltas are appended.
    pub fn session_msg_count(&self, session_id: &str) -> Result<Option<i64>> {
        let n = self
            .conn
            .query_row(
                "SELECT msg_count FROM sessions WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(n)
    }

    /// Read the persisted watermark for a `file_key`, or `None` if the file is
    /// not yet tracked.
    pub fn get_watermark(&self, file_key: &str) -> Result<Option<i64>> {
        let wm = self
            .conn
            .query_row(
                "SELECT watermark FROM files WHERE file_key = ?1",
                params![file_key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(wm)
    }

    /// Set the watermark for a tracked `file_key`. Returns the number of rows
    /// updated (0 if the file is not tracked).
    pub fn set_watermark(&self, file_key: &str, watermark: i64) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE files SET watermark = ?2 WHERE file_key = ?1",
            params![file_key, watermark],
        )?;
        Ok(n)
    }

    /// Full-text (or trigram) search, scoped and ranked.
    ///
    /// The FTS `MATCH` drives candidate selection; scope filters join in
    /// `sessions`/`sources`/`messages`. Results are ordered by a blend of the
    /// FTS `bm25` relevance and a recency term derived from the message ts, and
    /// the excerpt comes from `snippet()`.
    pub fn search(&self, query: &str, scope: &Scope, opts: &SearchOpts) -> Result<Vec<Hit>> {
        let fts_table = if opts.code {
            "messages_trg"
        } else {
            "messages_fts"
        };

        // Build the dynamic WHERE fragment and bound parameters. The MATCH
        // parameter and the recency reference-ts are always present; scope
        // filters are appended conditionally.
        let mut wheres: Vec<String> = Vec::new();
        let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        macro_rules! push_eq {
            ($col:expr, $val:expr) => {
                if let Some(v) = &$val {
                    wheres.push(format!("{} = ?", $col));
                    binds.push(Box::new(v.clone()));
                }
            };
        }

        push_eq!("src.account_uuid", scope.account_uuid);
        push_eq!("src.org_uuid", scope.org_uuid);
        push_eq!("src.profile", scope.profile);
        push_eq!("m.tool", scope.tool);
        push_eq!("s.repo_id", scope.repo_id);
        push_eq!("s.project_path", scope.cwd);
        push_eq!("s.git_branch", scope.branch);
        push_eq!("m.session_id", scope.session_id);
        push_eq!("m.role", scope.role);
        push_eq!("m.tool_name", scope.tool_name);

        if let Some(since) = scope.since {
            wheres.push("m.ts >= ?".into());
            binds.push(Box::new(since));
        }
        if let Some(until) = scope.until {
            wheres.push("m.ts <= ?".into());
            binds.push(Box::new(until));
        }

        let scope_sql = if wheres.is_empty() {
            String::new()
        } else {
            format!(" AND {}", wheres.join(" AND "))
        };

        // Reference timestamp for the recency term: the newest message ts in
        // the table (falls back to 0 for an empty db). Recency contributes a
        // bounded bonus that decays with age, blended into the bm25 score.
        let now: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(ts), 0) FROM messages", [], |r| {
                    r.get(0)
                })?;

        // bm25() returns a value where more-negative is more relevant, so we
        // negate it into a positive relevance. The recency term adds up to
        // RECENCY_WEIGHT, decaying linearly over RECENCY_WINDOW seconds.
        const RECENCY_WEIGHT: f64 = 2.0;
        const RECENCY_WINDOW: f64 = 30.0 * 24.0 * 3600.0;

        let sql = format!(
            "SELECT m.session_id, m.tool, s.repo_id, s.project_name, m.ts,
                    snippet({fts}, 0, '[', ']', ' … ', 10) AS snip,
                    bm25({fts}) AS rank
             FROM {fts}
             JOIN messages m ON m.id = {fts}.rowid
             LEFT JOIN sessions s ON s.session_id = m.session_id
             LEFT JOIN sources src ON src.id = m.source_id
             WHERE {fts} MATCH ?{scope}
             ORDER BY rank ASC
             LIMIT 100000",
            fts = fts_table,
            scope = scope_sql,
        );

        // Assemble the full parameter list: MATCH query first, then scope binds.
        let mut all: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::with_capacity(binds.len() + 1);
        all.push(Box::new(query.to_string()));
        all.extend(binds);
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = all.iter().map(|b| b.as_ref()).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |r| {
            let session_id: String = r.get(0)?;
            let tool: Option<String> = r.get(1)?;
            let repo_id: Option<String> = r.get(2)?;
            let project_name: Option<String> = r.get(3)?;
            let ts: i64 = r.get(4)?;
            let snippet: String = r.get(5)?;
            let rank: f64 = r.get(6)?;
            Ok((session_id, tool, repo_id, project_name, ts, snippet, rank))
        })?;

        let mut hits: Vec<Hit> = Vec::new();
        for row in rows {
            let (session_id, tool, repo_id, project_name, ts, snippet, rank) = row?;
            let relevance = -rank; // more relevant -> larger positive
            let age = (now - ts).max(0) as f64;
            let recency = RECENCY_WEIGHT * (1.0 - (age / RECENCY_WINDOW).min(1.0));
            let score = relevance + recency;
            hits.push(Hit {
                session_id,
                tool,
                repo_id,
                project_name,
                ts,
                snippet,
                score,
            });
        }

        // Blend: sort by the combined score descending, ts as a stable
        // tie-breaker (newer first), then truncate to the requested limit.
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.ts.cmp(&a.ts))
        });
        hits.truncate(opts.limit);
        Ok(hits)
    }

    /// Summarize every source with its session and message counts, ordered by
    /// tool then id. Powers the `list` command.
    pub fn list_sources(&self) -> Result<Vec<SourceSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT src.id, src.tool, src.email, src.org, src.profile,
                    (SELECT COUNT(*) FROM sessions s WHERE s.source_id = src.id),
                    (SELECT COUNT(*) FROM messages m WHERE m.source_id = src.id)
             FROM sources src
             ORDER BY src.tool IS NULL, src.tool, src.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SourceSummary {
                id: r.get(0)?,
                tool: r.get(1)?,
                email: r.get(2)?,
                org: r.get(3)?,
                profile: r.get(4)?,
                sessions: r.get(5)?,
                messages: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// List sessions matching `scope`, newest activity first, capped at `limit`.
    ///
    /// Only the session-level scope fields apply here (account/org/profile via
    /// the owning source, plus tool/repo/cwd/branch/session and the ts window
    /// against the session's activity span). Message-only fields (`role`,
    /// `tool_name`) are ignored.
    pub fn list_sessions(&self, scope: &Scope, limit: usize) -> Result<Vec<SessionRow>> {
        let mut wheres: Vec<String> = Vec::new();
        let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        macro_rules! push_eq {
            ($col:expr, $val:expr) => {
                if let Some(v) = &$val {
                    wheres.push(format!("{} = ?", $col));
                    binds.push(Box::new(v.clone()));
                }
            };
        }

        push_eq!("src.account_uuid", scope.account_uuid);
        push_eq!("src.org_uuid", scope.org_uuid);
        push_eq!("src.profile", scope.profile);
        push_eq!("s.tool", scope.tool);
        push_eq!("s.repo_id", scope.repo_id);
        push_eq!("s.project_path", scope.cwd);
        push_eq!("s.git_branch", scope.branch);
        push_eq!("s.session_id", scope.session_id);

        if let Some(since) = scope.since {
            wheres.push("s.last_ts >= ?".into());
            binds.push(Box::new(since));
        }
        if let Some(until) = scope.until {
            wheres.push("s.first_ts <= ?".into());
            binds.push(Box::new(until));
        }

        let where_sql = if wheres.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", wheres.join(" AND "))
        };

        let sql = format!(
            "SELECT s.session_id, s.tool, s.repo_id, s.project_name, s.git_branch,
                    s.first_ts, s.last_ts, s.msg_count
             FROM sessions s
             LEFT JOIN sources src ON src.id = s.source_id
             {where_sql}
             ORDER BY s.last_ts DESC, s.session_id
             LIMIT ?",
            where_sql = where_sql,
        );

        binds.push(Box::new(limit as i64));
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            binds.iter().map(|b| b.as_ref()).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |r| {
            Ok(SessionRow {
                session_id: r.get(0)?,
                tool: r.get(1)?,
                repo_id: r.get(2)?,
                project_name: r.get(3)?,
                git_branch: r.get(4)?,
                first_ts: r.get(5)?,
                last_ts: r.get(6)?,
                msg_count: r.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Fetch a session's messages in sequence order, joining in the stored body
    /// text. Powers the `show` command.
    pub fn session_messages(&self, session_id: &str) -> Result<Vec<MessageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.seq, m.ts, m.role, m.tool_name, COALESCE(t.body, '')
             FROM messages m
             LEFT JOIN messages_text t ON t.id = m.id
             WHERE m.session_id = ?1
             ORDER BY m.seq, m.id",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(MessageRow {
                seq: r.get(0)?,
                ts: r.get(1)?,
                role: r.get(2)?,
                tool_name: r.get(3)?,
                body: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The active account per tool: for each tool, the source owning the session
    /// with the most recent `last_ts`. Powers the `current` command.
    pub fn current_accounts(&self) -> Result<Vec<CurrentAccount>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.tool, src.email, src.org, src.profile, MAX(s.last_ts) AS m
             FROM sessions s
             LEFT JOIN sources src ON src.id = s.source_id
             WHERE s.tool IS NOT NULL
             GROUP BY s.tool
             ORDER BY s.tool",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(CurrentAccount {
                tool: r.get(0)?,
                email: r.get(1)?,
                org: r.get(2)?,
                profile: r.get(3)?,
                last_ts: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// Build a [`SourceRow`] from an [`Account`] plus tool/config context. A small
/// convenience for indexers so the mapping lives next to the schema.
pub fn source_row_from_account(
    tool: &str,
    config_dir: Option<&str>,
    profile: Option<&str>,
    account: &Account,
    kind: Option<&str>,
) -> SourceRow {
    SourceRow {
        tool: Some(tool.to_string()),
        config_dir: config_dir.map(str::to_string),
        profile: profile.map(str::to_string),
        account_uuid: account.account_uuid.clone(),
        org_uuid: account.org_uuid.clone(),
        email: account.email.clone(),
        org: account.org.clone(),
        kind: kind.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, Tool};

    #[allow(clippy::too_many_arguments)]
    fn meta(
        session_id: &str,
        tool: Tool,
        repo_id: Option<&str>,
        project_path: Option<&str>,
        project_name: Option<&str>,
        branch: Option<&str>,
        first_ts: i64,
        last_ts: i64,
    ) -> SessionMeta {
        SessionMeta {
            session_id: session_id.into(),
            tool,
            project_path: project_path.map(str::to_string),
            repo_id: repo_id.map(str::to_string),
            project_name: project_name.map(str::to_string),
            git_branch: branch.map(str::to_string),
            account: None,
            first_ts,
            last_ts,
        }
    }

    fn msg(
        session_id: &str,
        tool: Tool,
        seq: u64,
        ts: i64,
        role: Role,
        tool_name: Option<&str>,
        text: &str,
    ) -> MessageRecord {
        MessageRecord {
            session_id: session_id.into(),
            tool,
            seq,
            ts,
            role,
            tool_name: tool_name.map(str::to_string),
            uuid: None,
            text: text.into(),
            cwd: None,
        }
    }

    /// Build a fixture db spanning two tools, two accounts/orgs, two repos.
    ///
    /// Returns the db plus the two source ids (claude, codex).
    fn fixture() -> (Db, i64, i64) {
        let db = Db::open(":memory:").unwrap();

        let src_a = db
            .upsert_source(&SourceRow {
                tool: Some("claude-code".into()),
                config_dir: Some("/home/dev/.claude".into()),
                profile: Some("work".into()),
                account_uuid: Some("acct-A".into()),
                org_uuid: Some("org-1".into()),
                email: Some("a@example.com".into()),
                org: Some("Acme".into()),
                kind: Some("cli".into()),
            })
            .unwrap();

        let src_b = db
            .upsert_source(&SourceRow {
                tool: Some("codex".into()),
                config_dir: Some("/home/dev/.codex".into()),
                profile: Some("personal".into()),
                account_uuid: Some("acct-B".into()),
                org_uuid: Some("org-2".into()),
                email: Some("b@example.com".into()),
                org: Some("Beta".into()),
                kind: Some("cli".into()),
            })
            .unwrap();

        // Session 1: claude, repo-x, branch main.
        db.upsert_session(
            src_a,
            &meta(
                "s1",
                Tool::ClaudeCode,
                Some("repo-x"),
                Some("/proj/x"),
                Some("projx"),
                Some("main"),
                100,
                200,
            ),
            0,
            Some("summary one"),
        )
        .unwrap();
        // Session 2: codex, repo-y, branch feature.
        db.upsert_session(
            src_b,
            &meta(
                "s2",
                Tool::Codex,
                Some("repo-y"),
                Some("/proj/y"),
                Some("projy"),
                Some("feature"),
                300,
                400,
            ),
            0,
            None,
        )
        .unwrap();

        // Messages for s1 (claude / acct-A / org-1 / repo-x / main).
        db.insert_message(
            src_a,
            &msg(
                "s1",
                Tool::ClaudeCode,
                0,
                100,
                Role::User,
                None,
                "how do I fix the parser bug",
            ),
        )
        .unwrap();
        db.insert_message(
            src_a,
            &msg(
                "s1",
                Tool::ClaudeCode,
                1,
                150,
                Role::Assistant,
                None,
                "the parser needs a new token to fix the memory issue",
            ),
        )
        .unwrap();
        db.insert_message(
            src_a,
            &msg(
                "s1",
                Tool::ClaudeCode,
                2,
                200,
                Role::Tool,
                Some("grep"),
                "let mut counter = 0; // parser scaffolding",
            ),
        )
        .unwrap();

        // Messages for s2 (codex / acct-B / org-2 / repo-y / feature).
        db.insert_message(
            src_b,
            &msg(
                "s2",
                Tool::Codex,
                0,
                300,
                Role::User,
                None,
                "the parser is slow on large files",
            ),
        )
        .unwrap();
        db.insert_message(
            src_b,
            &msg(
                "s2",
                Tool::Codex,
                1,
                400,
                Role::Assistant,
                None,
                "cache the token stream to speed up the parser",
            ),
        )
        .unwrap();

        (db, src_a, src_b)
    }

    fn ids(hits: &[Hit]) -> Vec<String> {
        hits.iter().map(|h| h.session_id.clone()).collect()
    }

    #[test]
    fn open_memory_and_schema_idempotent() {
        let db = Db::open(":memory:").unwrap();
        // Re-running is a no-op.
        db.init_schema().unwrap();
        // Every table/vtable exists.
        for t in [
            "sources",
            "files",
            "sessions",
            "messages",
            "messages_text",
            "messages_fts",
            "messages_trg",
        ] {
            let n: i64 = db
                .conn()
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                    params![t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing object {t}");
        }
        // The sess_repo index exists.
        let idx: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='sess_repo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1);
    }

    #[test]
    fn open_file_path() {
        let dir = std::env::temp_dir().join(format!("csx-db-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sqlite");
        let p = path.to_str().unwrap();
        let db = Db::open(p).unwrap();
        db.upsert_source(&SourceRow::default()).unwrap();
        drop(db);
        // Reopening the same file preserves data.
        let db2 = Db::open(p).unwrap();
        let n: i64 = db2
            .conn()
            .query_row("SELECT count(*) FROM sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn source_row_from_account_maps_fields() {
        let acct = Account {
            account_uuid: Some("u".into()),
            org_uuid: Some("o".into()),
            email: Some("e@x.io".into()),
            org: Some("Org".into()),
        };
        let row = source_row_from_account("codex", Some("/c"), Some("p"), &acct, Some("k"));
        assert_eq!(row.tool.as_deref(), Some("codex"));
        assert_eq!(row.config_dir.as_deref(), Some("/c"));
        assert_eq!(row.profile.as_deref(), Some("p"));
        assert_eq!(row.account_uuid.as_deref(), Some("u"));
        assert_eq!(row.org_uuid.as_deref(), Some("o"));
        assert_eq!(row.email.as_deref(), Some("e@x.io"));
        assert_eq!(row.org.as_deref(), Some("Org"));
        assert_eq!(row.kind.as_deref(), Some("k"));
    }

    #[test]
    fn upsert_session_merges_bounds() {
        let db = Db::open(":memory:").unwrap();
        let src = db.upsert_source(&SourceRow::default()).unwrap();
        db.upsert_session(
            src,
            &meta("s", Tool::ClaudeCode, Some("r"), None, None, None, 100, 200),
            2,
            None,
        )
        .unwrap();
        // Second upsert with wider/narrower bounds: first_ts should take the
        // min, last_ts the max.
        db.upsert_session(
            src,
            &meta("s", Tool::ClaudeCode, Some("r"), None, None, None, 50, 150),
            5,
            Some("sum"),
        )
        .unwrap();
        let (first, last, count, summary): (i64, i64, i64, Option<String>) = db
            .conn()
            .query_row(
                "SELECT first_ts, last_ts, msg_count, summary FROM sessions WHERE session_id='s'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(first, 50);
        assert_eq!(last, 200);
        assert_eq!(count, 5);
        assert_eq!(summary.as_deref(), Some("sum"));
    }

    #[test]
    fn watermark_and_file_row_lifecycle() {
        let db = Db::open(":memory:").unwrap();
        let src = db.upsert_source(&SourceRow::default()).unwrap();

        // Not tracked yet.
        assert!(db.get_watermark("k1").unwrap().is_none());
        assert!(db.get_file("k1").unwrap().is_none());
        // Setting a watermark on an untracked file updates 0 rows.
        assert_eq!(db.set_watermark("k1", 5).unwrap(), 0);

        let id = db
            .upsert_file(src, Some("/a/b.jsonl"), "k1", 42, 999, Some("sess-1"))
            .unwrap();
        assert!(id > 0);
        // Default watermark is 0.
        assert_eq!(db.get_watermark("k1").unwrap(), Some(0));

        let row = db.get_file("k1").unwrap().unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.source_id, src);
        assert_eq!(row.path.as_deref(), Some("/a/b.jsonl"));
        assert_eq!(row.file_key, "k1");
        assert_eq!(row.size, 42);
        assert_eq!(row.mtime, 999);
        assert_eq!(row.watermark, 0);
        assert_eq!(row.session_id.as_deref(), Some("sess-1"));

        // Set and read back the watermark.
        assert_eq!(db.set_watermark("k1", 123).unwrap(), 1);
        assert_eq!(db.get_watermark("k1").unwrap(), Some(123));

        // Upsert on the same key updates size/mtime but preserves watermark and
        // keeps the same id.
        let id2 = db
            .upsert_file(src, Some("/a/b.jsonl"), "k1", 100, 1001, Some("sess-1"))
            .unwrap();
        assert_eq!(id2, id);
        let row2 = db.get_file("k1").unwrap().unwrap();
        assert_eq!(row2.size, 100);
        assert_eq!(row2.mtime, 1001);
        assert_eq!(row2.watermark, 123, "watermark must survive upsert");
    }

    #[test]
    fn session_msg_count_reads_or_none() {
        let db = Db::open(":memory:").unwrap();
        let src = db.upsert_source(&SourceRow::default()).unwrap();
        // Absent session -> None.
        assert_eq!(db.session_msg_count("ghost").unwrap(), None);
        db.upsert_session(
            src,
            &meta("s", Tool::ClaudeCode, Some("r"), None, None, None, 1, 2),
            7,
            None,
        )
        .unwrap();
        assert_eq!(db.session_msg_count("s").unwrap(), Some(7));
    }

    #[test]
    fn insert_message_populates_all_tables() {
        let db = Db::open(":memory:").unwrap();
        let src = db.upsert_source(&SourceRow::default()).unwrap();
        let id = db
            .insert_message(
                src,
                &msg(
                    "s",
                    Tool::ClaudeCode,
                    0,
                    10,
                    Role::User,
                    Some("bash"),
                    "hello world",
                ),
            )
            .unwrap();
        let text: String = db
            .conn()
            .query_row(
                "SELECT body FROM messages_text WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(text, "hello world");
        // Both FTS shadow tables can match it.
        let fts: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'hello'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts, 1);
        let trg: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM messages_trg WHERE messages_trg MATCH 'ell'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(trg, 1);
        // The metadata row carries the tool_name.
        let tn: Option<String> = db
            .conn()
            .query_row(
                "SELECT tool_name FROM messages WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tn.as_deref(), Some("bash"));
    }

    #[test]
    fn search_matches_and_snippet_highlights() {
        let (db, _, _) = fixture();
        let hits = db
            .search("parser", &Scope::default(), &SearchOpts::default())
            .unwrap();
        // "parser" appears in messages of both sessions.
        assert!(!hits.is_empty());
        let sset = ids(&hits);
        assert!(sset.contains(&"s1".to_string()));
        assert!(sset.contains(&"s2".to_string()));
        // Snippet highlights the term with the configured brackets.
        assert!(
            hits.iter().any(|h| h.snippet.contains("[parser]")),
            "expected a bracketed match, got: {:?}",
            hits.iter().map(|h| &h.snippet).collect::<Vec<_>>()
        );
        // Project name and repo id are joined in.
        let h1 = hits.iter().find(|h| h.session_id == "s1").unwrap();
        assert_eq!(h1.repo_id.as_deref(), Some("repo-x"));
        assert_eq!(h1.project_name.as_deref(), Some("projx"));
        assert_eq!(h1.tool.as_deref(), Some("claude-code"));
    }

    #[test]
    fn search_scope_by_tool() {
        let (db, _, _) = fixture();
        let scope = Scope {
            tool: Some("codex".into()),
            ..Default::default()
        };
        let hits = db.search("parser", &scope, &SearchOpts::default()).unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.session_id == "s2"));
    }

    #[test]
    fn search_scope_by_account_and_org_and_profile() {
        let (db, _, _) = fixture();
        for (field, val, want) in [
            ("account", "acct-A", "s1"),
            ("account", "acct-B", "s2"),
            ("org", "org-1", "s1"),
            ("org", "org-2", "s2"),
            ("profile", "work", "s1"),
            ("profile", "personal", "s2"),
        ] {
            let mut scope = Scope::default();
            match field {
                "account" => scope.account_uuid = Some(val.into()),
                "org" => scope.org_uuid = Some(val.into()),
                "profile" => scope.profile = Some(val.into()),
                _ => unreachable!(),
            }
            let hits = db.search("parser", &scope, &SearchOpts::default()).unwrap();
            assert!(!hits.is_empty(), "no hits for {field}={val}");
            assert!(
                hits.iter().all(|h| h.session_id == want),
                "{field}={val} leaked other sessions: {:?}",
                ids(&hits)
            );
        }
    }

    #[test]
    fn search_scope_by_repo_cwd_branch_session() {
        let (db, _, _) = fixture();
        for (field, val, want) in [
            ("repo", "repo-x", "s1"),
            ("cwd", "/proj/y", "s2"),
            ("branch", "main", "s1"),
            ("branch", "feature", "s2"),
            ("session", "s2", "s2"),
        ] {
            let mut scope = Scope::default();
            match field {
                "repo" => scope.repo_id = Some(val.into()),
                "cwd" => scope.cwd = Some(val.into()),
                "branch" => scope.branch = Some(val.into()),
                "session" => scope.session_id = Some(val.into()),
                _ => unreachable!(),
            }
            let hits = db.search("parser", &scope, &SearchOpts::default()).unwrap();
            assert!(!hits.is_empty(), "no hits for {field}={val}");
            assert!(
                hits.iter().all(|h| h.session_id == want),
                "{field}={val} leaked: {:?}",
                ids(&hits)
            );
        }
    }

    #[test]
    fn search_scope_by_role_and_tool_name() {
        let (db, _, _) = fixture();
        // role=tool only matches the grep tool message in s1.
        let scope = Scope {
            role: Some("tool".into()),
            ..Default::default()
        };
        let hits = db.search("parser", &scope, &SearchOpts::default()).unwrap();
        assert_eq!(ids(&hits), vec!["s1".to_string()]);

        // tool_name=grep likewise.
        let scope = Scope {
            tool_name: Some("grep".into()),
            ..Default::default()
        };
        let hits = db.search("parser", &scope, &SearchOpts::default()).unwrap();
        assert_eq!(ids(&hits), vec!["s1".to_string()]);

        // A non-existent tool_name yields nothing.
        let scope = Scope {
            tool_name: Some("nope".into()),
            ..Default::default()
        };
        let hits = db.search("parser", &scope, &SearchOpts::default()).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_scope_by_time_window() {
        let (db, _, _) = fixture();
        // since/until bounding to s1's timespan (100..=200) excludes s2 (300+).
        let scope = Scope {
            since: Some(100),
            until: Some(250),
            ..Default::default()
        };
        let hits = db.search("parser", &scope, &SearchOpts::default()).unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.session_id == "s1"));

        // until below everything -> empty.
        let scope = Scope {
            until: Some(50),
            ..Default::default()
        };
        let hits = db.search("parser", &scope, &SearchOpts::default()).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_ranking_blends_bm25_and_recency() {
        // Two messages both containing the query term exactly once, but with
        // very different timestamps. With comparable bm25, the newer one should
        // rank first because of the recency term.
        let db = Db::open(":memory:").unwrap();
        let src = db.upsert_source(&SourceRow::default()).unwrap();
        db.upsert_session(
            src,
            &meta("old", Tool::ClaudeCode, Some("r"), None, None, None, 0, 0),
            1,
            None,
        )
        .unwrap();
        db.upsert_session(
            src,
            &meta("new", Tool::ClaudeCode, Some("r"), None, None, None, 0, 0),
            1,
            None,
        )
        .unwrap();
        db.insert_message(
            src,
            &msg(
                "old",
                Tool::ClaudeCode,
                0,
                0,
                Role::User,
                None,
                "widget alpha",
            ),
        )
        .unwrap();
        db.insert_message(
            src,
            &msg(
                "new",
                Tool::ClaudeCode,
                0,
                10_000_000_000,
                Role::User,
                None,
                "widget beta",
            ),
        )
        .unwrap();
        let hits = db
            .search("widget", &Scope::default(), &SearchOpts::default())
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].session_id, "new", "newer message should rank first");
        assert!(hits[0].score >= hits[1].score);
    }

    #[test]
    fn search_limit_truncates() {
        let (db, _, _) = fixture();
        let opts = SearchOpts {
            code: false,
            limit: 1,
        };
        let hits = db.search("parser", &Scope::default(), &opts).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_trigram_substring_path() {
        let (db, _, _) = fixture();
        // "arse" is a substring inside "parser" — only the trigram index can
        // match a mid-token substring like this.
        let opts = SearchOpts {
            code: true,
            limit: 20,
        };
        let hits = db.search("arse", &Scope::default(), &opts).unwrap();
        assert!(!hits.is_empty(), "trigram substring should match 'parser'");
        // The default (unicode61) index must NOT match a bare mid-token
        // substring, proving the two paths differ.
        let hits_fts = db
            .search("arse", &Scope::default(), &SearchOpts::default())
            .unwrap();
        assert!(hits_fts.is_empty(), "fts should not match mid-token 'arse'");
    }

    #[test]
    fn search_code_finds_code_fragment() {
        let (db, _, _) = fixture();
        // Substring search for a code-ish fragment stored in s1's tool message.
        let opts = SearchOpts {
            code: true,
            limit: 20,
        };
        let hits = db.search("counter", &Scope::default(), &opts).unwrap();
        assert!(hits.iter().any(|h| h.session_id == "s1"));
    }

    #[test]
    fn search_empty_db_is_empty() {
        let db = Db::open(":memory:").unwrap();
        let hits = db
            .search("anything", &Scope::default(), &SearchOpts::default())
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn list_sources_counts_sessions_and_messages() {
        let (db, _, _) = fixture();
        let sources = db.list_sources().unwrap();
        assert_eq!(sources.len(), 2);
        // Ordered by tool: claude-code before codex.
        assert_eq!(sources[0].tool.as_deref(), Some("claude-code"));
        assert_eq!(sources[0].email.as_deref(), Some("a@example.com"));
        assert_eq!(sources[0].org.as_deref(), Some("Acme"));
        assert_eq!(sources[0].profile.as_deref(), Some("work"));
        assert_eq!(sources[0].sessions, 1);
        assert_eq!(sources[0].messages, 3);
        assert_eq!(sources[1].tool.as_deref(), Some("codex"));
        assert_eq!(sources[1].sessions, 1);
        assert_eq!(sources[1].messages, 2);
    }

    #[test]
    fn list_sources_empty_db() {
        let db = Db::open(":memory:").unwrap();
        assert!(db.list_sources().unwrap().is_empty());
    }

    #[test]
    fn list_sessions_orders_by_recency_and_respects_limit() {
        let (db, _, _) = fixture();
        let rows = db.list_sessions(&Scope::default(), 20).unwrap();
        assert_eq!(rows.len(), 2);
        // s2 (last_ts 400) is newer than s1 (last_ts 200).
        assert_eq!(rows[0].session_id, "s2");
        assert_eq!(rows[1].session_id, "s1");
        assert_eq!(rows[0].tool.as_deref(), Some("codex"));
        assert_eq!(rows[0].repo_id.as_deref(), Some("repo-y"));
        assert_eq!(rows[0].project_name.as_deref(), Some("projy"));
        assert_eq!(rows[0].git_branch.as_deref(), Some("feature"));
        assert_eq!(rows[0].first_ts, 300);
        assert_eq!(rows[0].last_ts, 400);

        // Limit truncates.
        let one = db.list_sessions(&Scope::default(), 1).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].session_id, "s2");
    }

    #[test]
    fn list_sessions_scopes_by_every_field() {
        let (db, _, _) = fixture();
        for (field, val, want) in [
            ("account", "acct-A", "s1"),
            ("org", "org-2", "s2"),
            ("profile", "work", "s1"),
            ("tool", "codex", "s2"),
            ("repo", "repo-x", "s1"),
            ("cwd", "/proj/y", "s2"),
            ("branch", "feature", "s2"),
            ("session", "s1", "s1"),
        ] {
            let mut scope = Scope::default();
            match field {
                "account" => scope.account_uuid = Some(val.into()),
                "org" => scope.org_uuid = Some(val.into()),
                "profile" => scope.profile = Some(val.into()),
                "tool" => scope.tool = Some(val.into()),
                "repo" => scope.repo_id = Some(val.into()),
                "cwd" => scope.cwd = Some(val.into()),
                "branch" => scope.branch = Some(val.into()),
                "session" => scope.session_id = Some(val.into()),
                _ => unreachable!(),
            }
            let rows = db.list_sessions(&scope, 20).unwrap();
            assert_eq!(rows.len(), 1, "{field}={val}");
            assert_eq!(rows[0].session_id, want, "{field}={val}");
        }
    }

    #[test]
    fn list_sessions_time_window() {
        let (db, _, _) = fixture();
        // Window overlapping only s1's span (100..=200).
        let scope = Scope {
            since: Some(50),
            until: Some(250),
            ..Default::default()
        };
        let rows = db.list_sessions(&scope, 20).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "s1");

        // until below everything -> empty.
        let scope = Scope {
            until: Some(10),
            ..Default::default()
        };
        assert!(db.list_sessions(&scope, 20).unwrap().is_empty());
    }

    #[test]
    fn session_messages_in_order() {
        let (db, _, _) = fixture();
        let msgs = db.session_messages("s1").unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].seq, 0);
        assert_eq!(msgs[0].role.as_deref(), Some("user"));
        assert_eq!(msgs[0].body, "how do I fix the parser bug");
        assert_eq!(msgs[2].seq, 2);
        assert_eq!(msgs[2].role.as_deref(), Some("tool"));
        assert_eq!(msgs[2].tool_name.as_deref(), Some("grep"));
        assert_eq!(msgs[2].body, "let mut counter = 0; // parser scaffolding");

        // Unknown session -> empty.
        assert!(db.session_messages("ghost").unwrap().is_empty());
    }

    #[test]
    fn current_accounts_one_per_tool_most_recent() {
        let (db, _, _) = fixture();
        let cur = db.current_accounts().unwrap();
        assert_eq!(cur.len(), 2);
        // Ordered by tool.
        assert_eq!(cur[0].tool, "claude-code");
        assert_eq!(cur[0].email.as_deref(), Some("a@example.com"));
        assert_eq!(cur[0].org.as_deref(), Some("Acme"));
        assert_eq!(cur[0].profile.as_deref(), Some("work"));
        assert_eq!(cur[0].last_ts, 200);
        assert_eq!(cur[1].tool, "codex");
        assert_eq!(cur[1].email.as_deref(), Some("b@example.com"));
        assert_eq!(cur[1].last_ts, 400);
    }

    #[test]
    fn current_accounts_picks_latest_source_per_tool() {
        // Two claude sources; the tool's "current" account is the one owning the
        // most recently active session.
        let db = Db::open(":memory:").unwrap();
        let older = db
            .upsert_source(&SourceRow {
                tool: Some("claude-code".into()),
                email: Some("old@example.com".into()),
                profile: Some("stale".into()),
                ..Default::default()
            })
            .unwrap();
        let newer = db
            .upsert_source(&SourceRow {
                tool: Some("claude-code".into()),
                email: Some("new@example.com".into()),
                profile: Some("fresh".into()),
                ..Default::default()
            })
            .unwrap();
        db.upsert_session(
            older,
            &meta("a", Tool::ClaudeCode, None, None, None, None, 1, 100),
            0,
            None,
        )
        .unwrap();
        db.upsert_session(
            newer,
            &meta("b", Tool::ClaudeCode, None, None, None, None, 1, 500),
            0,
            None,
        )
        .unwrap();
        let cur = db.current_accounts().unwrap();
        assert_eq!(cur.len(), 1);
        assert_eq!(cur[0].email.as_deref(), Some("new@example.com"));
        assert_eq!(cur[0].profile.as_deref(), Some("fresh"));
        assert_eq!(cur[0].last_ts, 500);
    }

    #[test]
    fn current_accounts_empty_db() {
        let db = Db::open(":memory:").unwrap();
        assert!(db.current_accounts().unwrap().is_empty());
    }

    /// A fresh db with a single table dropped, to force the SQL calls that touch
    /// it down their `?` error-propagation arms.
    fn db_without(table: &str) -> Db {
        let db = Db::open(":memory:").unwrap();
        db.conn()
            .execute_batch(&format!("DROP TABLE {table};"))
            .unwrap();
        db
    }

    fn any_msg() -> MessageRecord {
        msg("s1", Tool::Codex, 0, 5, Role::User, None, "hello")
    }

    fn any_meta() -> SessionMeta {
        meta(
            "s1",
            Tool::Codex,
            Some("r"),
            Some("/p"),
            Some("p"),
            Some("main"),
            1,
            5,
        )
    }

    #[test]
    fn upsert_source_reports_sql_errors() {
        let db = db_without("sources");
        assert!(db.upsert_source(&SourceRow::default()).is_err());
    }

    #[test]
    fn upsert_session_reports_sql_errors() {
        let db = db_without("sessions");
        assert!(db.upsert_session(1, &any_meta(), 0, None).is_err());
    }

    #[test]
    fn insert_message_reports_sql_errors() {
        // Dropping each table in turn exercises a distinct `?` arm: the metadata
        // insert, the external-content body, and the two FTS mirrors.
        for table in ["messages", "messages_text", "messages_fts", "messages_trg"] {
            let db = db_without(table);
            assert!(
                db.insert_message(1, &any_msg()).is_err(),
                "insert_message should fail with {table} dropped"
            );
        }
    }

    #[test]
    fn file_helpers_report_sql_errors() {
        let db = db_without("files");
        assert!(db.upsert_file(1, None, "k", 0, 0, None).is_err());
        assert!(db.get_file("k").is_err());
        assert!(db.get_watermark("k").is_err());
        assert!(db.set_watermark("k", 1).is_err());
    }

    #[test]
    fn session_msg_count_reports_sql_errors() {
        let db = db_without("sessions");
        assert!(db.session_msg_count("s1").is_err());
    }

    #[test]
    fn search_reports_sql_errors() {
        let db = db_without("messages_fts");
        assert!(db
            .search("hi", &Scope::default(), &SearchOpts::default())
            .is_err());
    }

    #[test]
    fn list_helpers_report_sql_errors() {
        assert!(db_without("sources").list_sources().is_err());
        assert!(db_without("sessions")
            .list_sessions(&Scope::default(), 10)
            .is_err());
        assert!(db_without("messages").session_messages("s1").is_err());
        assert!(db_without("sources").current_accounts().is_err());
    }

    /// A NULL in a column mapped to a non-`Option` Rust type makes the row
    /// closure's `r.get(_)?` fail, exercising the column-extraction error arms
    /// (distinct from a whole-statement SQL failure).
    #[test]
    fn row_closures_report_column_extraction_errors() {
        // get_file: a matching row whose non-Option `size` column is NULL makes
        // the closure's `r.get(4)?` fail.
        let db = Db::open(":memory:").unwrap();
        db.conn()
            .execute(
                "INSERT INTO files (id, source_id, path, file_key, size, mtime, watermark, session_id)
                 VALUES (1, 1, NULL, 'k', NULL, 0, 0, NULL)",
                [],
            )
            .unwrap();
        assert!(db.get_file("k").is_err());

        // session_messages: a message row whose ts (mapped to i64) is NULL.
        let db = Db::open(":memory:").unwrap();
        db.conn()
            .execute(
                "INSERT INTO messages (id, session_id, source_id, tool, seq, ts, role, tool_name, uuid)
                 VALUES (1, 's1', 1, 'codex', 0, NULL, 'user', NULL, NULL)",
                [],
            )
            .unwrap();
        db.conn()
            .execute("INSERT INTO messages_text (id, body) VALUES (1, 'x')", [])
            .unwrap();
        assert!(db.session_messages("s1").is_err());

        // list_sessions: a session row whose first_ts (i64) is NULL.
        let db = Db::open(":memory:").unwrap();
        db.conn()
            .execute(
                "INSERT INTO sessions (session_id, source_id, first_ts, last_ts, msg_count)
                 VALUES ('s1', 1, NULL, 0, 0)",
                [],
            )
            .unwrap();
        assert!(db.list_sessions(&Scope::default(), 10).is_err());
    }
}
