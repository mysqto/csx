//! Codex CLI transcript source.
//!
//! Discovers `<root>/sessions/**/*.jsonl` under an injectable config root
//! (default `~/.codex`, overridable via `CODEX_HOME`), and maps Codex rollout
//! JSONL onto the same canonical [`MessageRecord`]/[`SessionMeta`] types used
//! by the Claude source.
//!
//! Codex has no OAuth account object, so [`CodexSource::account`] is always
//! `None` and parsed sessions carry `account: None`.
//!
//! Rollout lines take the shape `{"timestamp", "type", "payload"}`. The first
//! `session_meta` line carries session id / cwd / git info; subsequent
//! `response_item` lines carry `message`, `reasoning`, `function_call`, and
//! `function_call_output` payloads that map onto records.

use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

use crate::error::Result;
use crate::model::{Account, MessageRecord, Role, SessionMeta, Tool};
use crate::source::claude::parse_iso8601_secs;
use crate::source::fileid::{file_key, size_and_mtime};
use crate::source::jsonl::read_from_offset;
use crate::source::{ParsedDelta, SessionFile, SessionSource};

/// Source adapter for Codex CLI transcripts rooted at a config directory.
#[derive(Debug, Clone)]
pub struct CodexSource {
    root: PathBuf,
}

impl CodexSource {
    /// Build a source rooted at an explicit config directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        CodexSource { root: root.into() }
    }

    /// Resolve the config root from the environment.
    ///
    /// Precedence: `CODEX_HOME` if set and non-empty, otherwise `~/.codex`.
    pub fn from_env() -> Self {
        CodexSource::new(resolve_root(
            std::env::var_os("CODEX_HOME"),
            std::env::var_os("HOME"),
        ))
    }

    /// The config root this source reads from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Codex has no OAuth account object: attribution is always `None`.
    pub fn account(&self) -> Result<Option<Account>> {
        Ok(None)
    }
}

/// Resolve the Codex config root from explicit environment values.
///
/// Precedence: an explicit, non-empty `CODEX_HOME`; otherwise `<home>/.codex`,
/// falling back to a relative `.codex` when `home` is unset. Pure so both
/// branches are deterministically testable without touching the process
/// environment.
fn resolve_root(
    codex_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(dir) = codex_home {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home.map(PathBuf::from).unwrap_or_default().join(".codex")
}

/// Discover transcript files under `<root>/sessions/**/*.jsonl`.
fn discover_files(root: &Path) -> Result<Vec<SessionFile>> {
    let sessions = root.join("sessions");
    let mut out = Vec::new();
    if !sessions.exists() {
        return Ok(out);
    }
    for entry in WalkDir::new(&sessions).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !is_jsonl_file(path) {
            continue;
        }
        let (size, mtime) = size_and_mtime(path)?;
        out.push(SessionFile {
            path: path.to_path_buf(),
            file_key: file_key(path),
            size,
            mtime,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// True when `path` is a regular `*.jsonl` file (Codex nests these under dated
/// subdirectories, so parent name is not constrained).
fn is_jsonl_file(path: &Path) -> bool {
    path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("jsonl")
}

impl SessionSource for CodexSource {
    fn tool(&self) -> Tool {
        Tool::Codex
    }

    fn discover(&self) -> Result<Vec<SessionFile>> {
        discover_files(&self.root)
    }

    fn parse(&self, f: &SessionFile, from_watermark: u64) -> Result<ParsedDelta> {
        let chunk = read_from_offset(&f.path, from_watermark)?;
        Ok(map_events(&chunk.values, chunk.new_offset))
    }

    fn config_dir(&self) -> Option<String> {
        Some(self.root.to_string_lossy().into_owned())
    }
}

/// Map a batch of Codex rollout event values into a [`ParsedDelta`].
fn map_events(values: &[Value], new_offset: u64) -> ParsedDelta {
    let mut messages: Vec<MessageRecord> = Vec::new();
    let mut session_id: Option<String> = None;
    let mut project_path: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let mut seq: u64 = 0;

    for value in values {
        let ts = event_ts(value);
        let payload = value.get("payload").unwrap_or(value);
        let etype = str_field(value, "type").or_else(|| str_field(payload, "type"));

        match etype.as_deref() {
            Some("session_meta") | Some("session") => {
                absorb_session_meta(payload, &mut session_id, &mut project_path, &mut git_branch);
            }
            Some("response_item")
            | Some("message")
            | Some("reasoning")
            | Some("function_call")
            | Some("function_call_output") => {
                let sid = session_id.clone().unwrap_or_default();
                let cwd = project_path.clone();
                if let Some(rec) = map_response_item(payload, &sid, ts, &mut seq, cwd.as_deref()) {
                    if let Some(t) = ts {
                        first_ts = Some(first_ts.map_or(t, |f| f.min(t)));
                        last_ts = Some(last_ts.map_or(t, |l| l.max(t)));
                    }
                    messages.push(rec);
                }
            }
            _ => {}
        }
    }

    let session = SessionMeta {
        session_id: session_id.unwrap_or_default(),
        tool: Tool::Codex,
        project_path: project_path.clone(),
        repo_id: None,
        project_name: project_path.as_deref().map(derive_project_name),
        git_branch,
        account: None,
        first_ts: first_ts.unwrap_or(0),
        last_ts: last_ts.unwrap_or(0),
    };

    ParsedDelta {
        messages,
        session,
        new_watermark: new_offset,
    }
}

/// Absorb session id / cwd / git branch from a `session_meta` payload.
fn absorb_session_meta(
    payload: &Value,
    session_id: &mut Option<String>,
    project_path: &mut Option<String>,
    git_branch: &mut Option<String>,
) {
    if session_id.is_none() {
        *session_id = str_field(payload, "id").or_else(|| str_field(payload, "session_id"));
    }
    if project_path.is_none() {
        *project_path = str_field(payload, "cwd");
    }
    if git_branch.is_none() {
        // Git info nests under a `git` object in the rollout meta.
        if let Some(git) = payload.get("git") {
            *git_branch = str_field(git, "branch");
        }
    }
}

/// Map one Codex response item payload into a [`MessageRecord`], or `None` when
/// it carries no usable text.
fn map_response_item(
    payload: &Value,
    session_id: &str,
    ts: Option<i64>,
    seq: &mut u64,
    cwd: Option<&str>,
) -> Option<MessageRecord> {
    let itype = str_field(payload, "type")?;
    let ts = ts.unwrap_or(0);

    let (role, tool_name, text) = match itype.as_str() {
        "message" => {
            let role = match str_field(payload, "role").as_deref() {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                Some("system") => Role::System,
                _ => Role::User,
            };
            (role, None, content_text(payload.get("content")))
        }
        "reasoning" => (Role::Assistant, None, reasoning_text(payload)),
        "function_call" => {
            let name = str_field(payload, "name");
            let args = str_field(payload, "arguments")
                .or_else(|| payload.get("arguments").map(|v| v.to_string()))
                .unwrap_or_default();
            (Role::Tool, name, args)
        }
        "function_call_output" => {
            let name = str_field(payload, "call_id");
            (
                Role::Tool,
                name,
                function_output_text(payload.get("output")),
            )
        }
        _ => return None,
    };

    if text.trim().is_empty() && tool_name.is_none() {
        return None;
    }

    let uuid = str_field(payload, "id");
    let rec = MessageRecord {
        session_id: session_id.to_string(),
        tool: Tool::Codex,
        seq: *seq,
        ts,
        role,
        tool_name,
        uuid,
        text,
        cwd: cwd.map(str::to_string),
    };
    *seq += 1;
    Some(rec)
}

/// Extract text from a Codex message `content`, which is an array of typed
/// parts (`input_text` / `output_text` / `text`) or a bare string.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| {
                str_field(p, "text").or_else(|| match p {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Extract reasoning text: Codex stores it as a `summary` array of parts or a
/// plain `text` field.
fn reasoning_text(payload: &Value) -> String {
    if let Some(Value::Array(parts)) = payload.get("summary") {
        let joined = parts
            .iter()
            .filter_map(|p| str_field(p, "text"))
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.is_empty() {
            return joined;
        }
    }
    str_field(payload, "text").unwrap_or_default()
}

/// Extract text from a `function_call_output` `output`, which may be a string,
/// an object with an `output`/`content` string, or absent.
fn function_output_text(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(s)) => s.clone(),
        Some(v @ Value::Object(_)) => str_field(v, "output")
            .or_else(|| str_field(v, "content"))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Read the event timestamp: rollout lines carry a top-level `timestamp`.
fn event_ts(value: &Value) -> Option<i64> {
    str_field(value, "timestamp").and_then(|s| parse_iso8601_secs(&s))
}

/// Derive a project name from a cwd path: its last component.
fn derive_project_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| cwd.to_string())
}

/// Read a string field, returning `None` when absent or not a string.
fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("codex")
    }

    #[test]
    fn resolve_root_covers_every_branch() {
        use std::ffi::OsString;
        assert_eq!(
            resolve_root(Some(OsString::from("/custom/codex")), None),
            PathBuf::from("/custom/codex")
        );
        assert_eq!(
            resolve_root(
                Some(OsString::from("")),
                Some(OsString::from("/home/tester"))
            ),
            PathBuf::from("/home/tester/.codex")
        );
        assert_eq!(resolve_root(None, None), PathBuf::from(".codex"));
    }

    #[test]
    fn from_env_reads_the_environment() {
        let src = CodexSource::from_env();
        assert!(src.root().to_string_lossy().contains(".codex") || src.root().is_absolute());
    }

    #[test]
    fn tool_is_codex_and_account_none() {
        let src = CodexSource::new("/x");
        assert_eq!(src.tool(), Tool::Codex);
        assert_eq!(src.account().unwrap(), None);
    }

    #[test]
    fn discover_finds_nested_transcripts() {
        let src = CodexSource::new(fixtures_root());
        let files = src.discover().unwrap();
        assert!(!files.is_empty());
        assert!(files
            .iter()
            .all(|f| f.path.extension().and_then(|e| e.to_str()) == Some("jsonl")));
        assert!(files.iter().all(|f| f.size > 0 && !f.file_key.is_empty()));
    }

    #[test]
    fn discover_empty_when_no_sessions_dir() {
        let dir = std::env::temp_dir().join(format!("csx-codex-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = CodexSource::new(&dir);
        assert!(src.discover().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_fixture_yields_canonical_records() {
        let src = CodexSource::new(fixtures_root());
        let files = src.discover().unwrap();
        let f = files
            .iter()
            .find(|f| f.path.file_name().unwrap() == "rollout-main.jsonl")
            .expect("rollout-main fixture");
        let delta = src.parse(f, 0).unwrap();

        assert_eq!(delta.session.session_id, "codex-sess-1");
        assert_eq!(delta.session.tool, Tool::Codex);
        assert_eq!(delta.session.git_branch.as_deref(), Some("develop"));
        assert_eq!(
            delta.session.project_path.as_deref(),
            Some("/work/acme/worker")
        );
        assert_eq!(delta.session.project_name.as_deref(), Some("worker"));
        // Codex never resolves an account.
        assert_eq!(delta.session.account, None);
        assert!(delta.session.first_ts > 0);
        assert!(delta.session.last_ts >= delta.session.first_ts);
        assert_eq!(delta.new_watermark, f.size);

        let roles: Vec<Role> = delta.messages.iter().map(|m| m.role).collect();
        assert!(roles.contains(&Role::User));
        assert!(roles.contains(&Role::Assistant));
        assert!(roles.contains(&Role::Tool));

        // The user opening message.
        let first_user = delta
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .unwrap();
        assert!(first_user.text.contains("flaky worker"));
        assert_eq!(first_user.cwd.as_deref(), Some("/work/acme/worker"));

        // A function_call (tool) record with its name and arguments.
        let call = delta
            .messages
            .iter()
            .find(|m| m.tool_name.as_deref() == Some("shell"))
            .expect("shell function_call record");
        assert_eq!(call.role, Role::Tool);
        assert!(call.text.contains("cargo"));

        // seq is contiguous.
        for (i, m) in delta.messages.iter().enumerate() {
            assert_eq!(m.seq, i as u64);
        }
    }

    #[test]
    fn parse_incremental_from_nonzero_offset() {
        let src = CodexSource::new(fixtures_root());
        let files = src.discover().unwrap();
        let f = files
            .iter()
            .find(|f| f.path.file_name().unwrap() == "rollout-main.jsonl")
            .unwrap();
        let full = src.parse(f, 0).unwrap();
        assert!(full.messages.len() >= 2);

        // Resume after the first (session_meta) line.
        let first_line_len = {
            let contents = std::fs::read_to_string(&f.path).unwrap();
            contents.split_inclusive('\n').next().unwrap().len() as u64
        };
        let partial = src.parse(f, first_line_len).unwrap();
        assert_eq!(partial.new_watermark, f.size);
        // The resumed pass sees no session_meta, so session_id is empty, and it
        // parses fewer/equal records but crucially still maps message content.
        assert!(partial.messages.len() <= full.messages.len());
        assert!(!partial.messages.is_empty());
    }

    #[test]
    fn map_events_maps_all_item_kinds() {
        let events = vec![
            serde_json::json!({
                "timestamp": "2026-07-04T10:00:00Z",
                "type": "session_meta",
                "payload": {"id": "sX", "cwd": "/repo/app", "git": {"branch": "main"}}
            }),
            serde_json::json!({
                "timestamp": "2026-07-04T10:00:01Z",
                "type": "response_item",
                "payload": {
                    "type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "hello there"}]
                }
            }),
            serde_json::json!({
                "timestamp": "2026-07-04T10:00:02Z",
                "type": "response_item",
                "payload": {"type": "reasoning", "summary": [{"type": "summary_text", "text": "think"}]}
            }),
            serde_json::json!({
                "timestamp": "2026-07-04T10:00:03Z",
                "type": "response_item",
                "payload": {"type": "function_call", "name": "shell", "arguments": "{\"cmd\":\"ls\"}", "call_id": "c1"}
            }),
            serde_json::json!({
                "timestamp": "2026-07-04T10:00:04Z",
                "type": "response_item",
                "payload": {"type": "function_call_output", "call_id": "c1", "output": {"output": "file.txt"}}
            }),
        ];
        let delta = map_events(&events, 500);
        assert_eq!(delta.session.session_id, "sX");
        assert_eq!(delta.session.git_branch.as_deref(), Some("main"));
        assert_eq!(delta.session.project_name.as_deref(), Some("app"));
        assert_eq!(delta.new_watermark, 500);
        assert_eq!(delta.messages.len(), 4);
        assert_eq!(delta.messages[0].role, Role::User);
        assert_eq!(delta.messages[0].text, "hello there");
        assert_eq!(delta.messages[1].role, Role::Assistant);
        assert_eq!(delta.messages[1].text, "think");
        assert_eq!(delta.messages[2].tool_name.as_deref(), Some("shell"));
        assert_eq!(delta.messages[3].role, Role::Tool);
        assert_eq!(delta.messages[3].text, "file.txt");
    }

    #[test]
    fn map_events_handles_flat_lines_without_payload() {
        // Some rollouts inline the item at top level (no `payload` wrapper).
        let events = vec![
            serde_json::json!({"type": "session", "id": "flat", "cwd": "/x/y"}),
            serde_json::json!({
                "timestamp": "2026-01-01T00:00:00Z",
                "type": "message", "role": "assistant", "content": "flat text"
            }),
        ];
        let delta = map_events(&events, 10);
        assert_eq!(delta.session.session_id, "flat");
        assert_eq!(delta.messages.len(), 1);
        assert_eq!(delta.messages[0].text, "flat text");
        assert_eq!(delta.messages[0].role, Role::Assistant);
    }

    #[test]
    fn map_events_skips_unknown_and_empty() {
        let events = vec![
            serde_json::json!({"type": "turn_context", "payload": {"type": "turn_context"}}),
            serde_json::json!({
                "type": "response_item",
                "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "   "}]}
            }),
        ];
        let delta = map_events(&events, 0);
        assert!(delta.messages.is_empty());
        assert_eq!(delta.session.session_id, "");
        assert_eq!(delta.session.first_ts, 0);
    }

    #[test]
    fn content_text_variants() {
        assert_eq!(content_text(Some(&serde_json::json!("s"))), "s");
        assert_eq!(
            content_text(Some(&serde_json::json!([
                {"type": "input_text", "text": "a"},
                {"type": "output_text", "text": "b"},
                "c"
            ]))),
            "a\nb\nc"
        );
        assert_eq!(content_text(None), "");
        assert_eq!(content_text(Some(&serde_json::json!(3))), "");
    }

    #[test]
    fn reasoning_text_prefers_summary_then_text() {
        assert_eq!(
            reasoning_text(&serde_json::json!({"summary": [{"text": "x"}]})),
            "x"
        );
        // Empty summary falls back to a `text` field.
        assert_eq!(
            reasoning_text(&serde_json::json!({"summary": [], "text": "y"})),
            "y"
        );
        assert_eq!(reasoning_text(&serde_json::json!({})), "");
    }

    #[test]
    fn function_output_text_variants() {
        assert_eq!(
            function_output_text(Some(&serde_json::json!("done"))),
            "done"
        );
        assert_eq!(
            function_output_text(Some(&serde_json::json!({"output": "o"}))),
            "o"
        );
        assert_eq!(
            function_output_text(Some(&serde_json::json!({"content": "c"}))),
            "c"
        );
        assert_eq!(function_output_text(Some(&serde_json::json!({}))), "");
        assert_eq!(function_output_text(None), "");
    }

    #[test]
    fn message_role_defaults_to_user_for_unknown() {
        let payload = serde_json::json!({"type": "message", "role": "weird", "content": "hi"});
        let mut seq = 0;
        let rec = map_response_item(&payload, "s", Some(1), &mut seq, None).unwrap();
        assert_eq!(rec.role, Role::User);
        // System role maps through.
        let payload = serde_json::json!({"type": "message", "role": "system", "content": "sys"});
        let rec = map_response_item(&payload, "s", Some(1), &mut seq, None).unwrap();
        assert_eq!(rec.role, Role::System);
    }

    #[test]
    fn map_response_item_none_for_untyped() {
        let mut seq = 0;
        assert!(map_response_item(&serde_json::json!({}), "s", None, &mut seq, None).is_none());
        // Unknown item type.
        assert!(map_response_item(
            &serde_json::json!({"type": "web_search"}),
            "s",
            None,
            &mut seq,
            None
        )
        .is_none());
    }

    #[test]
    fn is_jsonl_file_predicate() {
        let dir = std::env::temp_dir().join(format!("csx-codex-pred-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.jsonl");
        std::fs::File::create(&f).unwrap();
        assert!(is_jsonl_file(&f));
        let t = dir.join("a.txt");
        std::fs::File::create(&t).unwrap();
        assert!(!is_jsonl_file(&t));
        assert!(!is_jsonl_file(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn derive_project_name_from_path() {
        assert_eq!(derive_project_name("/a/worker"), "worker");
        assert_eq!(derive_project_name("worker"), "worker");
    }

    #[test]
    fn function_call_arguments_object_stringified() {
        // When `arguments` is an object rather than a string, it is stringified.
        let payload = serde_json::json!({
            "type": "function_call", "name": "apply_patch",
            "arguments": {"path": "a.rs"}
        });
        let mut seq = 0;
        let rec = map_response_item(&payload, "s", Some(1), &mut seq, None).unwrap();
        assert_eq!(rec.tool_name.as_deref(), Some("apply_patch"));
        assert!(rec.text.contains("a.rs"));
    }

    #[test]
    fn absorb_session_meta_fills_each_field() {
        // The `id` key wins for the session id.
        let mut sid = None;
        let mut path = None;
        let mut branch = None;
        absorb_session_meta(
            &serde_json::json!({"id": "sess-a", "cwd": "/w", "git": {"branch": "main"}}),
            &mut sid,
            &mut path,
            &mut branch,
        );
        assert_eq!(sid.as_deref(), Some("sess-a"));
        assert_eq!(path.as_deref(), Some("/w"));
        assert_eq!(branch.as_deref(), Some("main"));

        // When `id` is absent, `session_id` is the fallback; a `git` object
        // without a branch leaves the branch unset.
        let mut sid = None;
        let mut path = None;
        let mut branch = None;
        absorb_session_meta(
            &serde_json::json!({"session_id": "sess-b", "git": {}}),
            &mut sid,
            &mut path,
            &mut branch,
        );
        assert_eq!(sid.as_deref(), Some("sess-b"));
        assert_eq!(path, None);
        assert_eq!(branch, None);

        // Already-populated fields are never overwritten, and a payload with no
        // git object skips the branch block entirely.
        let mut sid = Some("keep".to_string());
        let mut path = Some("/keep".to_string());
        let mut branch = Some("keep-branch".to_string());
        absorb_session_meta(
            &serde_json::json!({"id": "other", "cwd": "/other"}),
            &mut sid,
            &mut path,
            &mut branch,
        );
        assert_eq!(sid.as_deref(), Some("keep"));
        assert_eq!(path.as_deref(), Some("/keep"));
        assert_eq!(branch.as_deref(), Some("keep-branch"));
    }

    #[test]
    fn source_usable_as_trait_object() {
        let src = CodexSource::new(fixtures_root());
        let dynsrc: &dyn SessionSource = &src;
        assert_eq!(dynsrc.tool(), Tool::Codex);
        assert!(!dynsrc.discover().unwrap().is_empty());
        assert_eq!(dynsrc.account().unwrap(), None);
        assert!(dynsrc.config_dir().unwrap().contains("codex"));
    }
}
