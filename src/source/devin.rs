//! Devin CLI transcript source.
//!
//! Devin keeps every session in one SQLite database, `<root>/sessions.db`,
//! under an injectable data root (default `~/.local/share/devin/cli`, or
//! `$XDG_DATA_HOME/devin/cli` when `XDG_DATA_HOME` is set and non-empty).
//! csx opens that database read-only and never writes to it.
//!
//! Each visible session becomes one [`SessionFile`]: they all share the
//! database path and differ by `file_key` (`devin:<session id>`). The `size`
//! is the session's message-node count and the `mtime` its last activity, so
//! the indexer's unchanged-skip works per session.
//!
//! Messages live in `message_nodes`, one JSON `chat_message` per node.
//! Context compaction and retries copy a message into new nodes that keep the
//! same `message_id`, so parsing builds the list of unique messages by first
//! appearance. System prompts are skipped, and a user-role message that
//! Devin did not mark `is_user_input` is stored as a system message. The
//! watermark is the number of unique messages already seen, so a reparse
//! after a compaction adds nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::model::{Account, MessageRecord, Role, SessionMeta, Tool};
use crate::source::claude::parse_iso8601_secs;
use crate::source::{ParsedDelta, SessionFile, SessionSource};

/// The `file_key` prefix that marks a Devin session.
const KEY_PREFIX: &str = "devin:";

/// How long a read waits for a lock held by Devin's writer.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// Source adapter for Devin CLI sessions rooted at a data directory.
#[derive(Debug, Clone)]
pub struct DevinSource {
    root: PathBuf,
}

impl DevinSource {
    /// Build a source rooted at an explicit data directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DevinSource { root: root.into() }
    }

    /// Resolve the data root from the environment.
    ///
    /// Precedence: `$XDG_DATA_HOME/devin/cli` if `XDG_DATA_HOME` is set and
    /// non-empty, otherwise `~/.local/share/devin/cli`.
    pub fn from_env() -> Self {
        DevinSource::new(resolve_root(
            std::env::var_os("XDG_DATA_HOME"),
            std::env::var_os("HOME"),
        ))
    }

    /// The data root this source reads from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Devin has no account object: attribution is always `None`.
    pub fn account(&self) -> Result<Option<Account>> {
        Ok(None)
    }

    /// The session database under the root.
    fn db_path(&self) -> PathBuf {
        self.root.join("sessions.db")
    }
}

/// Resolve the Devin data root from explicit environment values.
///
/// Precedence: an explicit, non-empty `XDG_DATA_HOME` (joined with
/// `devin/cli`); otherwise `<home>/.local/share/devin/cli`, falling back to a
/// relative path when `home` is unset. Pure so every branch is testable
/// without touching the process environment.
fn resolve_root(
    xdg_data_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    match xdg_data_home.filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("devin").join("cli"),
        None => home
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".local/share/devin/cli"),
    }
}

/// Open a Devin database read-only. The connection is opened with
/// `SQLITE_OPEN_READ_ONLY`, so SQLite refuses every write, and
/// `PRAGMA query_only` is a second guard. A missing file is an error; it is
/// never created. A busy timeout waits out a brief lock by Devin's writer.
pub(crate) fn open_readonly(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA query_only = ON")?;
    Ok(conn)
}

impl SessionSource for DevinSource {
    fn tool(&self) -> Tool {
        Tool::Devin
    }

    fn discover(&self) -> Result<Vec<SessionFile>> {
        let db = self.db_path();
        if !db.is_file() {
            return Ok(Vec::new());
        }
        let conn = open_readonly(&db)?;
        let mut stmt = conn.prepare(
            "SELECT s.id, s.last_activity_at,
                    (SELECT COUNT(*) FROM message_nodes m WHERE m.session_id = s.id)
             FROM sessions s
             WHERE s.hidden = 0
             ORDER BY s.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SessionFile {
                path: db.clone(),
                file_key: format!("{KEY_PREFIX}{}", r.get::<_, String>(0)?),
                size: r.get::<_, i64>(2)? as u64,
                mtime: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn parse(&self, f: &SessionFile, from_watermark: u64) -> Result<ParsedDelta> {
        let session_id = f
            .file_key
            .strip_prefix(KEY_PREFIX)
            .ok_or_else(|| Error::parse(format!("not a devin session key: {}", f.file_key)))?;
        let conn = open_readonly(&f.path)?;
        let cwd: Option<String> = conn
            .query_row(
                "SELECT working_directory FROM sessions WHERE id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?;
        let mut stmt = conn.prepare(
            "SELECT chat_message, created_at, metadata FROM message_nodes
             WHERE session_id = ?1 ORDER BY node_id",
        )?;
        let nodes = stmt
            .query_map([session_id], |r| {
                Ok(Node {
                    chat_message: r.get(0)?,
                    created_at: r.get(1)?,
                    metadata: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(map_nodes(session_id, cwd, &nodes, from_watermark))
    }

    fn config_dir(&self) -> Option<String> {
        Some(self.root.to_string_lossy().into_owned())
    }
}

/// A project name from a working directory: its last component.
fn project_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .map_or_else(|| cwd.to_string(), str::to_string)
}

/// One row of `message_nodes`, in `node_id` order.
#[derive(Debug, Clone)]
struct Node {
    chat_message: String,
    created_at: i64,
    metadata: Option<String>,
}

/// Turn a session's nodes into a [`ParsedDelta`].
///
/// Unique messages are taken by first appearance of `message_id`; system
/// messages and system-prefix nodes are skipped, and a node whose JSON does
/// not parse is not a message. Message `i` of that list yields its records
/// with `seq = i`; only messages at `from_watermark` and later are returned,
/// and the new watermark is the number of unique messages. The session span
/// covers every record, so it stays right when nothing new is returned.
fn map_nodes(
    session_id: &str,
    cwd: Option<String>,
    nodes: &[Node],
    from_watermark: u64,
) -> ParsedDelta {
    let mut seen: HashSet<String> = HashSet::new();
    let mut unique: u64 = 0;
    let mut messages = Vec::new();
    let mut span: Option<(i64, i64)> = None;

    for node in nodes {
        let Ok(msg) = serde_json::from_str::<Value>(&node.chat_message) else {
            continue;
        };
        if is_system_prefix(node.metadata.as_deref()) || str_field(&msg, "role") == Some("system") {
            continue;
        }
        let id = str_field(&msg, "message_id");
        if let Some(id) = id {
            if !seen.insert(id.to_string()) {
                continue;
            }
        }
        let seq = unique;
        unique += 1;
        let ts = str_field(msg.get("metadata").unwrap_or(&Value::Null), "created_at")
            .and_then(parse_iso8601_secs)
            .unwrap_or(node.created_at);
        let records = message_records(&msg, session_id, seq, ts, cwd.as_deref());
        if !records.is_empty() {
            span = Some(span.map_or((ts, ts), |(f, l)| (f.min(ts), l.max(ts))));
        }
        if seq >= from_watermark {
            messages.extend(records);
        }
    }

    let (first_ts, last_ts) = span.unwrap_or((0, 0));
    ParsedDelta {
        messages,
        session: SessionMeta {
            session_id: session_id.to_string(),
            tool: Tool::Devin,
            project_name: cwd.as_deref().map(project_name),
            project_path: cwd,
            repo_id: None,
            git_branch: None,
            account: None,
            first_ts,
            last_ts,
        },
        new_watermark: unique,
    }
}

/// True when node metadata marks the node as part of the system prefix.
fn is_system_prefix(metadata: Option<&str>) -> bool {
    metadata
        .and_then(|m| serde_json::from_str::<Value>(m).ok())
        .and_then(|v| v.get("is_system_prefix").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// The records of one unique message: its text (when non-empty), then one
/// tool record per tool call. Tool-call uuids are `<message_id>#<n>`.
fn message_records(
    msg: &Value,
    session_id: &str,
    seq: u64,
    ts: i64,
    cwd: Option<&str>,
) -> Vec<MessageRecord> {
    let id = str_field(msg, "message_id");
    let record =
        |role, tool_name: Option<&str>, uuid: Option<String>, text: String| MessageRecord {
            session_id: session_id.to_string(),
            tool: Tool::Devin,
            seq,
            ts,
            role,
            tool_name: tool_name.map(str::to_string),
            uuid,
            text,
            cwd: cwd.map(str::to_string),
        };
    let (role, tool_name) = match str_field(msg, "role") {
        // Devin also writes user-role messages the person did not type; like
        // Devin's own search, only `is_user_input` marks a typed prompt.
        Some("user") if is_user_input(msg) => (Role::User, None),
        Some("user") => (Role::System, None),
        Some("assistant") => (Role::Assistant, None),
        Some("tool") => (
            Role::Tool,
            str_field(msg, "name")
                .or_else(|| str_field(msg, "tool_name"))
                .or_else(|| str_field(msg, "tool_call_id")),
        ),
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    let text = content_text(msg.get("content"));
    if !text.trim().is_empty() {
        out.push(record(role, tool_name, id.map(str::to_string), text));
    }
    let calls = msg.get("tool_calls").and_then(Value::as_array);
    for (n, call) in calls.into_iter().flatten().enumerate() {
        let func = call.get("function").unwrap_or(&Value::Null);
        let name = str_field(call, "name").or_else(|| str_field(func, "name"));
        let args = ["arguments", "input"]
            .iter()
            .find_map(|k| call.get(*k).or_else(|| func.get(*k)))
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
            .unwrap_or_default();
        if name.is_none() && args.is_empty() {
            continue;
        }
        let uuid = id.map(|id| format!("{id}#{n}"));
        out.push(record(Role::Tool, name, uuid, args));
    }
    out
}

/// True when the message metadata marks it as typed by the person:
/// `is_user_input` is `true` or `1`, the test Devin's own search uses.
fn is_user_input(msg: &Value) -> bool {
    match msg.pointer("/metadata/is_user_input") {
        Some(Value::Bool(b)) => *b,
        Some(v) => v.as_i64() == Some(1),
        None => false,
    }
}

/// Text of a message `content`: a string, or an array of parts that are
/// strings or objects with a `text` field. Anything else is empty.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.as_str().or_else(|| str_field(p, "text")))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A string field, or `None` when absent or not a string.
fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::ffi::OsString;

    const SCHEMA: &str = "
        CREATE TABLE sessions (
          id TEXT PRIMARY KEY,
          working_directory TEXT NOT NULL,
          backend_type TEXT NOT NULL, model TEXT NOT NULL, agent_mode TEXT NOT NULL,
          created_at INTEGER NOT NULL,
          last_activity_at INTEGER NOT NULL,
          title TEXT, main_chain_id INTEGER, shell_last_seen_index INTEGER DEFAULT 0,
          cogs_json TEXT, workspace_dirs TEXT, hidden INTEGER NOT NULL DEFAULT 0,
          metadata TEXT);
        CREATE TABLE message_nodes (
          row_id INTEGER PRIMARY KEY AUTOINCREMENT,
          session_id TEXT NOT NULL,
          node_id INTEGER NOT NULL,
          parent_node_id INTEGER,
          chat_message TEXT NOT NULL,
          created_at INTEGER NOT NULL,
          metadata TEXT,
          UNIQUE(session_id, node_id));
        CREATE TABLE prompt_history (
          id INTEGER PRIMARY KEY AUTOINCREMENT, content TEXT NOT NULL,
          timestamp INTEGER NOT NULL, session_id TEXT NOT NULL,
          is_shell INTEGER NOT NULL DEFAULT 0);
    ";

    /// A fresh temp data root.
    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("csx-devin-src-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Create `<root>/sessions.db` with the Devin schema and return a
    /// writable connection for fixtures.
    pub(crate) fn fixture_db(root: &Path) -> Connection {
        let conn = Connection::open(root.join("sessions.db")).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    pub(crate) fn add_session(conn: &Connection, id: &str, cwd: &str, last: i64, hidden: i64) {
        conn.execute(
            "INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode,
                                   created_at, last_activity_at, title, hidden)
             VALUES (?1, ?2, 'b', 'm', 'normal', 100, ?3, 'a title', ?4)",
            rusqlite::params![id, cwd, last, hidden],
        )
        .unwrap();
    }

    fn add_node(conn: &Connection, sid: &str, node: i64, msg: &Value, meta: Option<&str>) {
        conn.execute(
            "INSERT INTO message_nodes (session_id, node_id, chat_message, created_at, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![sid, node, msg.to_string(), 1_000 + node, meta],
        )
        .unwrap();
    }

    /// A message as Devin writes it. User messages are marked as typed by
    /// the person (`is_user_input: true`).
    fn msg(id: &str, role: &str, content: &str) -> Value {
        serde_json::json!({
            "message_id": id, "role": role, "content": content,
            "metadata": {
                "created_at": "2026-07-04T10:00:00.123456Z",
                "is_user_input": if role == "user" { Value::Bool(true) } else { Value::Null }
            }
        })
    }

    const PREFIX: &str = r#"{"is_system_prefix": true}"#;
    const COMPACT: &str =
        r#"{"is_system_prefix": null, "extensions": {"compact/prior_node_ids": [1]}}"#;

    fn file_for(src: &DevinSource, id: &str) -> SessionFile {
        src.discover()
            .unwrap()
            .into_iter()
            .find(|f| f.file_key == format!("devin:{id}"))
            .expect("session discovered")
    }

    #[test]
    fn resolve_root_covers_every_branch() {
        assert_eq!(
            resolve_root(Some(OsString::from("/xdg")), Some(OsString::from("/h"))),
            PathBuf::from("/xdg/devin/cli")
        );
        assert_eq!(
            resolve_root(Some(OsString::from("")), Some(OsString::from("/h"))),
            PathBuf::from("/h/.local/share/devin/cli")
        );
        assert_eq!(
            resolve_root(None, Some(OsString::from("/h"))),
            PathBuf::from("/h/.local/share/devin/cli")
        );
        assert_eq!(
            resolve_root(None, None),
            PathBuf::from(".local/share/devin/cli")
        );
    }

    #[test]
    fn from_env_reads_the_environment() {
        let src = DevinSource::from_env();
        assert!(src.root().ends_with("devin/cli"));
    }

    #[test]
    fn tool_is_devin_account_none_and_config_dir_is_root() {
        let src = DevinSource::new("/data/devin/cli");
        assert_eq!(src.tool(), Tool::Devin);
        assert_eq!(src.account().unwrap(), None);
        let dynsrc: &dyn SessionSource = &src;
        assert_eq!(dynsrc.account().unwrap(), None);
        assert_eq!(dynsrc.config_dir().as_deref(), Some("/data/devin/cli"));
    }

    #[test]
    fn open_readonly_refuses_writes() {
        let root = tmp("ro");
        let conn = fixture_db(&root);
        add_session(&conn, "a-b", "/work/acme/app", 5, 0);
        drop(conn);
        let ro = open_readonly(&root.join("sessions.db")).unwrap();
        let only: i64 = ro.query_row("PRAGMA query_only", [], |r| r.get(0)).unwrap();
        assert_eq!(only, 1);
        // A brief lock by Devin's writer is waited out, not reported.
        let busy: i64 = ro
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(busy, 2_000);
        let n: i64 = ro
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        // Neither a write nor turning query_only back off gets through: the
        // connection itself is read-only.
        assert!(ro.execute("DELETE FROM sessions", []).is_err());
        ro.execute_batch("PRAGMA query_only = OFF").unwrap();
        assert!(ro.execute("UPDATE sessions SET title = 'x'", []).is_err());
        // A missing file is an error, not a new empty database.
        assert!(open_readonly(&root.join("absent.db")).is_err());
        assert!(!root.join("absent.db").exists());
    }

    #[test]
    fn discover_empty_without_database() {
        let root = tmp("nodb");
        assert!(DevinSource::new(&root).discover().unwrap().is_empty());
    }

    #[test]
    fn discover_lists_visible_sessions_with_node_count_and_activity() {
        let root = tmp("discover");
        let conn = fixture_db(&root);
        add_session(&conn, "swift-otter", "/work/acme/app", 1_700_000_500, 0);
        add_session(&conn, "quiet-fox", "/work/acme/lib", 1_700_000_900, 0);
        add_session(&conn, "hidden-one", "/work/acme/x", 1_700_000_100, 1);
        for n in 0..3 {
            add_node(
                &conn,
                "swift-otter",
                n,
                &msg(&format!("m{n}"), "user", "hi"),
                None,
            );
        }
        add_node(&conn, "hidden-one", 0, &msg("h", "user", "hi"), None);
        drop(conn);

        let files = DevinSource::new(&root).discover().unwrap();
        let keys: Vec<&str> = files.iter().map(|f| f.file_key.as_str()).collect();
        assert_eq!(keys, vec!["devin:quiet-fox", "devin:swift-otter"]);
        assert!(files.iter().all(|f| f.path == root.join("sessions.db")));
        assert_eq!((files[0].size, files[0].mtime), (0, 1_700_000_900));
        assert_eq!((files[1].size, files[1].mtime), (3, 1_700_000_500));
    }

    #[test]
    fn discover_errors_on_a_database_without_the_schema() {
        let root = tmp("badschema");
        Connection::open(root.join("sessions.db"))
            .unwrap()
            .execute_batch("CREATE TABLE other (x)")
            .unwrap();
        assert!(DevinSource::new(&root).discover().is_err());
    }

    #[test]
    fn parse_skips_system_dedupes_copies_and_maps_roles() {
        let root = tmp("parse");
        let conn = fixture_db(&root);
        add_session(&conn, "swift-otter", "/work/acme/app", 1_700_000_500, 0);
        let s = "swift-otter";
        // System prompt (role) and a system-prefix node (metadata only).
        add_node(&conn, s, 0, &msg("sys", "system", "be helpful"), None);
        add_node(
            &conn,
            s,
            1,
            &msg("pre", "user", "prefix rules"),
            Some(PREFIX),
        );
        add_node(
            &conn,
            s,
            2,
            &msg("u1", "user", "fix the parser"),
            Some("null"),
        );
        // Compaction copies the user message into a new node, same id.
        add_node(
            &conn,
            s,
            3,
            &msg("sys", "system", "be helpful"),
            Some(COMPACT),
        );
        add_node(
            &conn,
            s,
            4,
            &msg("u1", "user", "fix the parser"),
            Some(COMPACT),
        );
        add_node(
            &conn,
            s,
            5,
            &msg("a1", "assistant", "done, parser fixed"),
            None,
        );
        // A retry sibling with the same message id.
        add_node(
            &conn,
            s,
            6,
            &msg("a1", "assistant", "done, parser fixed"),
            None,
        );
        add_node(
            &conn,
            s,
            7,
            &serde_json::json!({"message_id": "t1", "role": "tool", "name": "read_file",
                                "content": "fn main() {}"}),
            None,
        );
        drop(conn);

        let src = DevinSource::new(&root);
        let f = file_for(&src, s);
        assert_eq!(f.size, 8);
        let delta = src.parse(&f, 0).unwrap();
        let got: Vec<(u64, Role, &str)> = delta
            .messages
            .iter()
            .map(|m| (m.seq, m.role, m.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, Role::User, "fix the parser"),
                (1, Role::Assistant, "done, parser fixed"),
                (2, Role::Tool, "fn main() {}"),
            ]
        );
        assert_eq!(delta.new_watermark, 3);
        let m = &delta.messages[0];
        assert_eq!(m.session_id, s);
        assert_eq!(m.tool, Tool::Devin);
        assert_eq!(m.uuid.as_deref(), Some("u1"));
        assert_eq!(m.cwd.as_deref(), Some("/work/acme/app"));
        // ISO-8601 created_at from the message metadata.
        assert_eq!(m.ts, parse_iso8601_secs("2026-07-04T10:00:00Z").unwrap());
        assert_eq!(delta.messages[2].tool_name.as_deref(), Some("read_file"));

        let meta = &delta.session;
        assert_eq!(meta.session_id, s);
        assert_eq!(meta.tool, Tool::Devin);
        assert_eq!(meta.project_path.as_deref(), Some("/work/acme/app"));
        assert_eq!(meta.project_name.as_deref(), Some("app"));
        assert_eq!(meta.git_branch, None);
        assert_eq!(meta.account, None);
        assert_eq!(meta.first_ts, 1_007);
        // The tool result has no metadata, so it falls back to node 7's time.
        assert_eq!(delta.messages[2].ts, 1_007);
        assert_eq!(meta.last_ts, m.ts);
    }

    #[test]
    fn parse_user_messages_not_typed_by_the_person_become_system() {
        let root = tmp("userinput");
        let conn = fixture_db(&root);
        let s = "keen-lynx";
        add_session(&conn, s, "/work/acme/app", 10, 0);
        let user = |id: &str, text: &str, flag: Value| {
            serde_json::json!({"message_id": id, "role": "user", "content": text,
                               "metadata": {"is_user_input": flag}})
        };
        add_node(&conn, s, 0, &msg("u1", "user", "typed question"), None);
        add_node(&conn, s, 1, &user("u2", "injected null", Value::Null), None);
        add_node(
            &conn,
            s,
            2,
            &user("u3", "injected false", Value::Bool(false)),
            None,
        );
        add_node(
            &conn,
            s,
            3,
            &user("u4", "typed as one", serde_json::json!(1)),
            None,
        );
        add_node(
            &conn,
            s,
            4,
            &serde_json::json!({"message_id": "u5", "role": "user", "content": "no metadata"}),
            None,
        );
        drop(conn);
        let src = DevinSource::new(&root);
        let delta = src.parse(&file_for(&src, s), 0).unwrap();
        let got: Vec<(Role, &str)> = delta
            .messages
            .iter()
            .map(|m| (m.role, m.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Role::User, "typed question"),
                (Role::System, "injected null"),
                (Role::System, "injected false"),
                (Role::User, "typed as one"),
                (Role::System, "no metadata"),
            ]
        );
    }

    #[test]
    fn parse_watermark_returns_only_new_and_compaction_adds_nothing() {
        let root = tmp("watermark");
        let conn = fixture_db(&root);
        let s = "quiet-fox";
        add_session(&conn, s, "/work/acme/lib", 10, 0);
        add_node(&conn, s, 0, &msg("u1", "user", "first question"), None);
        add_node(&conn, s, 1, &msg("a1", "assistant", "first answer"), None);
        let src = DevinSource::new(&root);
        let first = src.parse(&file_for(&src, s), 0).unwrap();
        assert_eq!(first.messages.len(), 2);
        assert_eq!(first.new_watermark, 2);

        // Compaction: copies of both messages, nothing new.
        add_node(
            &conn,
            s,
            2,
            &msg("u1", "user", "first question"),
            Some(COMPACT),
        );
        add_node(
            &conn,
            s,
            3,
            &msg("a1", "assistant", "first answer"),
            Some(COMPACT),
        );
        let again = src.parse(&file_for(&src, s), first.new_watermark).unwrap();
        assert!(again.messages.is_empty());
        assert_eq!(again.new_watermark, 2);
        // The session span still covers the whole session.
        assert_eq!(again.session.first_ts, first.session.first_ts);

        // A real new turn after the compaction.
        add_node(&conn, s, 4, &msg("u2", "user", "second question"), None);
        let next = src.parse(&file_for(&src, s), again.new_watermark).unwrap();
        assert_eq!(next.messages.len(), 1);
        assert_eq!(next.messages[0].text, "second question");
        assert_eq!(next.messages[0].seq, 2);
        assert_eq!(next.new_watermark, 3);
    }

    #[test]
    fn parse_tool_calls_become_tool_records() {
        let root = tmp("toolcalls");
        let conn = fixture_db(&root);
        let s = "busy-bee";
        add_session(&conn, s, "/work/acme/app", 10, 0);
        // Empty assistant content with only tool calls: tool records only.
        add_node(
            &conn,
            s,
            0,
            &serde_json::json!({
                "message_id": "a1", "role": "assistant", "content": "",
                "tool_calls": [
                    {"id": "c1", "name": "run_command", "arguments": {"cmd": "cargo test"}},
                    {"id": "c2", "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}},
                    {"id": "c3"}
                ]
            }),
            None,
        );
        // Text plus a call: one assistant record, then the tool record.
        add_node(
            &conn,
            s,
            1,
            &serde_json::json!({
                "message_id": "a2", "role": "assistant", "content": "let me look",
                "tool_calls": [{"name": "grep", "input": {"q": "todo"}}]
            }),
            None,
        );
        // A tool result carrying `tool_name`, with content as text parts.
        add_node(
            &conn,
            s,
            2,
            &serde_json::json!({
                "message_id": "t1", "role": "tool", "tool_name": "grep",
                "content": [{"type": "text", "text": "src/a.rs: todo"}, "more"]
            }),
            None,
        );
        // A tool result named only by its call id.
        add_node(
            &conn,
            s,
            3,
            &serde_json::json!({"message_id": "t2", "role": "tool", "tool_call_id": "c1",
                                "content": "ok"}),
            None,
        );
        drop(conn);

        let src = DevinSource::new(&root);
        let delta = src.parse(&file_for(&src, s), 0).unwrap();
        let got: Vec<(u64, Role, Option<&str>, Option<&str>)> = delta
            .messages
            .iter()
            .map(|m| (m.seq, m.role, m.tool_name.as_deref(), m.uuid.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, Role::Tool, Some("run_command"), Some("a1#0")),
                (0, Role::Tool, Some("read_file"), Some("a1#1")),
                (1, Role::Assistant, None, Some("a2")),
                (1, Role::Tool, Some("grep"), Some("a2#0")),
                (2, Role::Tool, Some("grep"), Some("t1")),
                (3, Role::Tool, Some("c1"), Some("t2")),
            ]
        );
        assert!(delta.messages[0].text.contains("cargo test"));
        assert!(delta.messages[1].text.contains("a.rs"));
        assert!(delta.messages[3].text.contains("todo"));
        assert_eq!(delta.messages[4].text, "src/a.rs: todo\nmore");
        assert_eq!(delta.new_watermark, 4);
    }

    #[test]
    fn parse_timestamp_falls_back_to_node_created_at() {
        let root = tmp("ts");
        let conn = fixture_db(&root);
        let s = "late-owl";
        add_session(&conn, s, "/work/acme/app", 10, 0);
        add_node(
            &conn,
            s,
            0,
            &serde_json::json!({"message_id": "u1", "role": "user", "content": "no metadata"}),
            None,
        );
        add_node(
            &conn,
            s,
            1,
            &serde_json::json!({"message_id": "u2", "role": "user", "content": "bad ts",
                                "metadata": {"created_at": "yesterday"}}),
            None,
        );
        drop(conn);
        let src = DevinSource::new(&root);
        let delta = src.parse(&file_for(&src, s), 0).unwrap();
        let ts: Vec<i64> = delta.messages.iter().map(|m| m.ts).collect();
        assert_eq!(ts, vec![1_000, 1_001]);
        assert_eq!(
            (delta.session.first_ts, delta.session.last_ts),
            (1_000, 1_001)
        );
    }

    #[test]
    fn parse_skips_empty_unknown_and_malformed_nodes() {
        let root = tmp("skips");
        let conn = fixture_db(&root);
        let s = "odd-duck";
        add_session(&conn, s, "/work/acme/app", 10, 0);
        // Malformed JSON is not a message at all.
        conn.execute(
            "INSERT INTO message_nodes (session_id, node_id, chat_message, created_at)
             VALUES (?1, 0, '{broken', 1)",
            [s],
        )
        .unwrap();
        // Empty text: a unique message that yields no record.
        add_node(&conn, s, 1, &msg("e1", "user", "   "), None);
        // Unknown role: counted, no record.
        add_node(&conn, s, 2, &msg("x1", "narrator", "aside"), None);
        // No message id: still indexed, each node on its own.
        add_node(
            &conn,
            s,
            3,
            &serde_json::json!({"role": "user", "content": "anonymous"}),
            Some("{broken meta"),
        );
        // Content of an unexpected type yields no text.
        add_node(
            &conn,
            s,
            4,
            &serde_json::json!({"message_id": "n1", "role": "assistant", "content": 42}),
            None,
        );
        drop(conn);
        let src = DevinSource::new(&root);
        let delta = src.parse(&file_for(&src, s), 0).unwrap();
        let texts: Vec<&str> = delta.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["anonymous"]);
        assert_eq!(delta.messages[0].seq, 2);
        assert_eq!(delta.messages[0].uuid, None);
        assert_eq!(delta.new_watermark, 4);
    }

    #[test]
    fn parse_missing_session_row_and_bad_key() {
        let root = tmp("missing");
        drop(fixture_db(&root));
        let src = DevinSource::new(&root);
        let f = SessionFile {
            path: root.join("sessions.db"),
            file_key: "devin:gone-away".into(),
            size: 0,
            mtime: 0,
        };
        let delta = src.parse(&f, 0).unwrap();
        assert!(delta.messages.is_empty());
        assert_eq!(delta.session.session_id, "gone-away");
        assert_eq!(delta.session.project_path, None);
        assert_eq!(delta.session.project_name, None);
        assert_eq!((delta.session.first_ts, delta.session.last_ts), (0, 0));

        let bad = SessionFile {
            file_key: "other:x".into(),
            ..f
        };
        let err = src.parse(&bad, 0).unwrap_err();
        assert!(err.to_string().contains("other:x"));
    }

    #[test]
    fn unreadable_database_and_mistyped_rows_are_errors() {
        use std::os::unix::fs::PermissionsExt;
        // A database file that cannot be opened.
        let root = tmp("unreadable");
        drop(fixture_db(&root));
        let db = root.join("sessions.db");
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(DevinSource::new(&root).discover().is_err());
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
        let missing = SessionFile {
            path: root.join("absent.db"),
            file_key: "devin:a".into(),
            size: 0,
            mtime: 0,
        };
        assert!(DevinSource::new(&root).parse(&missing, 0).is_err());

        // Columns of the wrong type fail the read instead of guessing.
        let root = tmp("mistyped");
        let conn = fixture_db(&root);
        conn.execute_batch(
            "INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode,
                                   created_at, last_activity_at)
             VALUES ('late', '/w', 'b', 'm', 'n', 1, 'soon');",
        )
        .unwrap();
        let src = DevinSource::new(&root);
        assert!(src.discover().is_err());
        conn.execute_batch(
            "UPDATE sessions SET last_activity_at = 1;
             INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode,
                                   created_at, last_activity_at)
             VALUES (NULL, '/w', 'b', 'm', 'n', 1, 1);",
        )
        .unwrap();
        assert!(src.discover().is_err());
        let file = |id: &str| SessionFile {
            path: root.join("sessions.db"),
            file_key: format!("devin:{id}"),
            size: 0,
            mtime: 0,
        };
        conn.execute_batch(
            "INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode,
                                   created_at, last_activity_at)
             VALUES ('blob-cwd', X'00', 'b', 'm', 'n', 1, 1);
             INSERT INTO message_nodes (session_id, node_id, chat_message, created_at)
             VALUES ('blob', 0, X'00', 1);
             INSERT INTO message_nodes (session_id, node_id, chat_message, created_at)
             VALUES ('text-ts', 0, '{}', 'x');
             INSERT INTO message_nodes (session_id, node_id, chat_message, created_at, metadata)
             VALUES ('blob-meta', 0, '{}', 1, X'00');",
        )
        .unwrap();
        for id in ["blob-cwd", "blob", "text-ts", "blob-meta"] {
            assert!(src.parse(&file(id), 0).is_err(), "{id}");
        }
    }

    #[test]
    fn project_name_is_last_path_component() {
        assert_eq!(project_name("/work/acme/app"), "app");
        assert_eq!(project_name("/"), "/");
    }

    #[test]
    fn sync_indexes_many_sessions_from_one_database() {
        use crate::db::{Db, Scope, SearchOpts};
        use crate::git_shim::GitRunner;

        struct NoGit;
        impl GitRunner for NoGit {
            fn run(&self, _cwd: &str, _args: &[&str]) -> Result<String> {
                Err(Error::other("no git"))
            }
        }

        let root = tmp("sync");
        let conn = fixture_db(&root);
        add_session(&conn, "swift-otter", "/work/acme/app", 10, 0);
        add_session(&conn, "quiet-fox", "/work/acme/lib", 20, 0);
        add_node(
            &conn,
            "swift-otter",
            0,
            &msg("u1", "user", "zebra parser"),
            None,
        );
        add_node(
            &conn,
            "quiet-fox",
            0,
            &msg("u2", "user", "zebra lexer"),
            None,
        );
        let sources: Vec<Box<dyn SessionSource>> = vec![Box::new(DevinSource::new(&root))];
        let db = Db::open(":memory:").unwrap();
        let stats = crate::index::sync(&sources, &db, &NoGit).unwrap();
        assert_eq!((stats.files_seen, stats.files_indexed), (2, 2));
        assert_eq!(stats.messages_added, 2);
        let scope = Scope {
            tool: Some("devin".into()),
            ..Scope::default()
        };
        let hits = db.search("zebra", &scope, &SearchOpts::default()).unwrap();
        assert_eq!(hits.len(), 2);

        // Unchanged sessions are skipped; one that grew is reparsed alone.
        let stats = crate::index::sync(&sources, &db, &NoGit).unwrap();
        assert_eq!((stats.files_skipped, stats.files_indexed), (2, 0));
        add_node(
            &conn,
            "quiet-fox",
            1,
            &msg("u2", "user", "zebra lexer"),
            Some(COMPACT),
        );
        add_node(
            &conn,
            "quiet-fox",
            2,
            &msg("a2", "assistant", "zebra done"),
            None,
        );
        conn.execute(
            "UPDATE sessions SET last_activity_at = 30 WHERE id = 'quiet-fox'",
            [],
        )
        .unwrap();
        let stats = crate::index::sync(&sources, &db, &NoGit).unwrap();
        assert_eq!((stats.files_skipped, stats.files_indexed), (1, 1));
        assert_eq!(stats.messages_added, 1);
        let hits = db.search("zebra", &scope, &SearchOpts::default()).unwrap();
        assert_eq!(hits.len(), 3);
    }
}
