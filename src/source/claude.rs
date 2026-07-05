//! Claude Code transcript source.
//!
//! Discovers `<root>/projects/**/sessions/*.jsonl` under an injectable config
//! root (default `~/.claude`, overridable via the `CLAUDE_CONFIG_DIR`
//! environment variable), and maps Claude Code JSONL events onto the canonical
//! [`MessageRecord`]/[`SessionMeta`] types.
//!
//! Account attribution comes from the `oauthAccount` object in the config
//! directory's `.claude.json`, when present.
//!
//! Discovery and parsing read only files under the (temp-rootable) config dir,
//! so the whole module is unit-testable without a shim.

use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

use crate::error::Result;
use crate::model::{Account, MessageRecord, Role, SessionMeta, Tool};
use crate::source::fileid::{file_key, size_and_mtime};
use crate::source::jsonl::{read_from_offset, read_json_file};
use crate::source::{ParsedDelta, SessionFile, SessionSource};

/// Source adapter for Claude Code transcripts rooted at a config directory.
#[derive(Debug, Clone)]
pub struct ClaudeSource {
    root: PathBuf,
}

impl ClaudeSource {
    /// Build a source rooted at an explicit config directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ClaudeSource { root: root.into() }
    }

    /// Resolve the config root from the environment.
    ///
    /// Precedence: `CLAUDE_CONFIG_DIR` if set and non-empty, otherwise
    /// `~/.claude` (via `HOME`). Falls back to a relative `.claude` if `HOME`
    /// is unset, which keeps the constructor total for tests.
    pub fn from_env() -> Self {
        ClaudeSource::new(resolve_root(
            std::env::var_os("CLAUDE_CONFIG_DIR"),
            std::env::var_os("HOME"),
        ))
    }

    /// The config root this source reads from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Extract account attribution from `<root>/.claude.json`, if present.
    ///
    /// Returns `Ok(None)` when the file is absent or has no `oauthAccount`.
    pub fn account(&self) -> Result<Option<Account>> {
        let path = self.root.join(".claude.json");
        let Some(value) = read_json_file(&path)? else {
            return Ok(None);
        };
        Ok(account_from_config(&value))
    }
}

/// Resolve the Claude config root from explicit environment values.
///
/// Precedence: an explicit, non-empty `CLAUDE_CONFIG_DIR`; otherwise
/// `<home>/.claude`, falling back to a relative `.claude` when `home` is unset.
/// Pure so both branches are deterministically testable without touching the
/// process environment.
fn resolve_root(
    config_dir: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(dir) = config_dir {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home.map(PathBuf::from).unwrap_or_default().join(".claude")
}

/// Map a parsed `.claude.json` value onto an [`Account`].
///
/// Reads the `oauthAccount` object's `accountUuid`, `organizationUuid`,
/// `emailAddress`, and `organizationName`. Returns `None` when there is no
/// `oauthAccount` object at all.
fn account_from_config(value: &Value) -> Option<Account> {
    let oauth = value.get("oauthAccount")?;
    if !oauth.is_object() {
        return None;
    }
    Some(Account {
        account_uuid: str_field(oauth, "accountUuid"),
        org_uuid: str_field(oauth, "organizationUuid"),
        email: str_field(oauth, "emailAddress"),
        org: str_field(oauth, "organizationName"),
    })
}

/// Discover transcript files under `<root>/projects/**/sessions/*.jsonl`.
fn discover_files(root: &Path) -> Result<Vec<SessionFile>> {
    let projects = root.join("projects");
    let mut out = Vec::new();
    if !projects.exists() {
        return Ok(out);
    }
    for entry in WalkDir::new(&projects).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !is_session_transcript(path) {
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
    // Deterministic order regardless of walk order.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// True when `path` is a `*.jsonl` file whose immediate parent directory is
/// named `sessions` (i.e. `.../sessions/<file>.jsonl`).
fn is_session_transcript(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return false;
    }
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        == Some("sessions")
}

impl SessionSource for ClaudeSource {
    fn tool(&self) -> Tool {
        Tool::ClaudeCode
    }

    fn discover(&self) -> Result<Vec<SessionFile>> {
        discover_files(&self.root)
    }

    fn parse(&self, f: &SessionFile, from_watermark: u64) -> Result<ParsedDelta> {
        let chunk = read_from_offset(&f.path, from_watermark)?;
        let account = self.account()?;
        let delta = map_events(&chunk.values, from_watermark, chunk.new_offset, account);
        Ok(delta)
    }

    fn account(&self) -> Result<Option<Account>> {
        ClaudeSource::account(self)
    }

    fn config_dir(&self) -> Option<String> {
        Some(self.root.to_string_lossy().into_owned())
    }
}

/// Map a batch of Claude JSONL event values into a [`ParsedDelta`].
///
/// `base_seq` is the watermark passed in (used only as an opaque offset — the
/// real per-message `seq` restarts at 0 within the batch, matching how the
/// indexer replays a whole file). Non-message events (mode changes, snapshots,
/// summaries) are skipped for message extraction but still contribute their
/// session metadata (session id, cwd, branch, timestamps).
fn map_events(
    values: &[Value],
    _base_seq: u64,
    new_offset: u64,
    account: Option<Account>,
) -> ParsedDelta {
    let mut messages: Vec<MessageRecord> = Vec::new();
    let mut session_id: Option<String> = None;
    let mut project_path: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let mut seq: u64 = 0;

    for value in values {
        // Every event that carries a sessionId contributes it.
        if session_id.is_none() {
            if let Some(sid) = str_field(value, "sessionId") {
                session_id = Some(sid);
            }
        }
        if project_path.is_none() {
            if let Some(cwd) = str_field(value, "cwd") {
                project_path = Some(cwd);
            }
        }
        if git_branch.is_none() {
            if let Some(b) = str_field(value, "gitBranch") {
                if !b.is_empty() {
                    git_branch = Some(b);
                }
            }
        }

        let event_type = str_field(value, "type");
        let ts = timestamp_secs(value);

        // Only `user` and `assistant` event types carry a `message` object we
        // turn into records. Everything else is metadata-only.
        let role = match event_type.as_deref() {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };

        let Some(message) = value.get("message") else {
            continue;
        };
        let sid = session_id.clone().unwrap_or_default();
        let cwd = project_path.clone();

        for rec in flatten_message(message, role, &sid, ts, value, &mut seq, cwd.as_deref()) {
            if let Some(t) = ts {
                first_ts = Some(first_ts.map_or(t, |f| f.min(t)));
                last_ts = Some(last_ts.map_or(t, |l| l.max(t)));
            }
            messages.push(rec);
        }
    }

    let session = SessionMeta {
        session_id: session_id.unwrap_or_default(),
        tool: Tool::ClaudeCode,
        project_path: project_path.clone(),
        repo_id: None,
        project_name: project_path.as_deref().map(derive_project_name),
        git_branch,
        account,
        first_ts: first_ts.unwrap_or(0),
        last_ts: last_ts.unwrap_or(0),
    };

    ParsedDelta {
        messages,
        session,
        new_watermark: new_offset,
    }
}

/// Turn a single Claude `message` object into zero or more [`MessageRecord`]s.
///
/// A user/assistant message's `content` is either a plain string (one text
/// record) or an array of typed blocks. Text and thinking blocks become a
/// record with the outer role; `tool_use` blocks become a [`Role::Tool`] record
/// named after the tool; `tool_result` blocks become a [`Role::Tool`] record
/// carrying the referenced `tool_use_id` as the tool name.
fn flatten_message(
    message: &Value,
    role: Role,
    session_id: &str,
    ts: Option<i64>,
    event: &Value,
    seq: &mut u64,
    cwd: Option<&str>,
) -> Vec<MessageRecord> {
    let uuid = str_field(event, "uuid");
    let ts = ts.unwrap_or(0);
    let content = message.get("content");

    let mut out = Vec::new();
    let mut push = |role: Role, tool_name: Option<String>, text: String| {
        if text.trim().is_empty() && tool_name.is_none() {
            return;
        }
        out.push(MessageRecord {
            session_id: session_id.to_string(),
            tool: Tool::ClaudeCode,
            seq: *seq,
            ts,
            role,
            tool_name,
            uuid: uuid.clone(),
            text,
            cwd: cwd.map(str::to_string),
        });
        *seq += 1;
    };

    match content {
        Some(Value::String(s)) => push(role, None, s.clone()),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                map_content_block(block, role, &mut push);
            }
        }
        _ => {}
    }
    out
}

/// Map one content block, invoking `push(role, tool_name, text)` for each
/// record it yields.
fn map_content_block<F: FnMut(Role, Option<String>, String)>(
    block: &Value,
    outer_role: Role,
    push: &mut F,
) {
    let btype = str_field(block, "type");
    match btype.as_deref() {
        Some("text") | Some("thinking") => {
            let text = str_field(block, "text")
                .or_else(|| str_field(block, "thinking"))
                .unwrap_or_default();
            push(outer_role, None, text);
        }
        Some("tool_use") => {
            let name = str_field(block, "name");
            let input = block
                .get("input")
                .map(|v| v.to_string())
                .unwrap_or_default();
            push(Role::Tool, name, input);
        }
        Some("tool_result") => {
            let name = str_field(block, "tool_use_id");
            let text = tool_result_text(block.get("content"));
            push(Role::Tool, name, text);
        }
        _ => {}
    }
}

/// Extract plain text from a `tool_result` block's `content`, which may be a
/// string or an array of `{type:"text", text:...}` sub-blocks.
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| str_field(item, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Derive a human-friendly project name from a cwd path: its last component.
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

/// Parse a Claude ISO-8601 `timestamp` (e.g. `2026-07-04T13:44:20.966Z`) into
/// unix seconds without pulling in a date library.
fn timestamp_secs(value: &Value) -> Option<i64> {
    let raw = str_field(value, "timestamp")?;
    parse_iso8601_secs(&raw)
}

/// Minimal ISO-8601 (`YYYY-MM-DDThh:mm:ss[.fff]Z`) to unix-seconds parser.
///
/// Handles the fixed-width UTC form Claude and Codex both emit. Returns `None`
/// for anything that does not match, so callers fall back to `0`.
pub(crate) fn parse_iso8601_secs(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let min: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec)
}

/// Days since the unix epoch for a civil (proleptic Gregorian) date, using
/// Howard Hinnant's `days_from_civil` algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixtures_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("claude")
    }

    #[test]
    fn resolve_root_covers_every_branch() {
        use std::ffi::OsString;
        // Explicit, non-empty config dir wins.
        assert_eq!(
            resolve_root(Some(OsString::from("/somewhere/custom")), None),
            PathBuf::from("/somewhere/custom")
        );
        // Empty config dir is ignored; falls back to HOME/.claude.
        assert_eq!(
            resolve_root(
                Some(OsString::from("")),
                Some(OsString::from("/home/tester"))
            ),
            PathBuf::from("/home/tester/.claude")
        );
        // No config dir, no HOME: relative .claude.
        assert_eq!(resolve_root(None, None), PathBuf::from(".claude"));
    }

    #[test]
    fn from_env_reads_the_environment() {
        // Exercise the thin env-reading wrapper; the value depends on the live
        // environment, so we only assert it produced a claude-rooted path.
        let src = ClaudeSource::from_env();
        assert!(src.root().to_string_lossy().contains(".claude") || src.root().is_absolute());
    }

    #[test]
    fn tool_is_claude_code() {
        let src = ClaudeSource::new("/x");
        assert_eq!(src.tool(), Tool::ClaudeCode);
    }

    #[test]
    fn discover_finds_session_transcripts() {
        let src = ClaudeSource::new(fixtures_root());
        let files = src.discover().unwrap();
        assert!(!files.is_empty(), "expected fixture transcripts");
        assert!(files
            .iter()
            .all(|f| f.path.extension().and_then(|e| e.to_str()) == Some("jsonl")));
        assert!(files.iter().all(|f| !f.file_key.is_empty()));
        assert!(files.iter().all(|f| f.size > 0));
    }

    #[test]
    fn discover_empty_when_no_projects_dir() {
        let dir = std::env::temp_dir().join(format!("csx-claude-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = ClaudeSource::new(&dir);
        assert!(src.discover().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_session_transcript_rejects_wrong_parent_and_ext() {
        let dir = std::env::temp_dir().join(format!("csx-claude-pred-{}", std::process::id()));
        let sessions = dir.join("projects").join("proj").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let good = sessions.join("a.jsonl");
        std::fs::File::create(&good)
            .unwrap()
            .write_all(b"{}\n")
            .unwrap();
        assert!(is_session_transcript(&good));
        // Wrong extension.
        let txt = sessions.join("b.txt");
        std::fs::File::create(&txt).unwrap();
        assert!(!is_session_transcript(&txt));
        // Right ext, wrong parent directory name.
        let wrong = dir.join("projects").join("proj").join("other.jsonl");
        std::fs::File::create(&wrong).unwrap();
        assert!(!is_session_transcript(&wrong));
        // A directory is never a transcript.
        assert!(!is_session_transcript(&sessions));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn account_extracted_from_config() {
        let src = ClaudeSource::new(fixtures_root());
        let acct = src.account().unwrap().expect("claude account present");
        assert_eq!(acct.email.as_deref(), Some("dev@example.com"));
        assert_eq!(acct.org.as_deref(), Some("Acme Inc"));
        assert_eq!(
            acct.account_uuid.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(
            acct.org_uuid.as_deref(),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
    }

    #[test]
    fn account_none_when_config_missing() {
        let dir = std::env::temp_dir().join(format!("csx-claude-noacct-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = ClaudeSource::new(&dir);
        assert_eq!(src.account().unwrap(), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn account_none_when_no_oauth_object() {
        assert_eq!(account_from_config(&serde_json::json!({"other": 1})), None);
        // Present but not an object.
        assert_eq!(
            account_from_config(&serde_json::json!({"oauthAccount": "nope"})),
            None
        );
    }

    #[test]
    fn parse_fixture_yields_canonical_records() {
        let src = ClaudeSource::new(fixtures_root());
        let files = src.discover().unwrap();
        let main = files
            .iter()
            .find(|f| f.path.file_name().unwrap() == "session-main.jsonl")
            .expect("session-main fixture");
        let delta = src.parse(main, 0).unwrap();

        // Session rollup.
        assert_eq!(delta.session.session_id, "sess-claude-1");
        assert_eq!(delta.session.tool, Tool::ClaudeCode);
        assert_eq!(delta.session.git_branch.as_deref(), Some("main"));
        assert_eq!(
            delta.session.project_path.as_deref(),
            Some("/work/acme/api")
        );
        assert_eq!(delta.session.project_name.as_deref(), Some("api"));
        assert!(delta.session.account.is_some());
        assert!(delta.session.first_ts > 0);
        assert!(delta.session.last_ts >= delta.session.first_ts);
        assert_eq!(delta.new_watermark, main.size);

        // Roles present: a user text, an assistant text, a tool_use, and a
        // tool_result (also a Tool role).
        let roles: Vec<Role> = delta.messages.iter().map(|m| m.role).collect();
        assert!(roles.contains(&Role::User));
        assert!(roles.contains(&Role::Assistant));
        assert!(roles.contains(&Role::Tool));

        // The user's opening string message.
        let first = &delta.messages[0];
        assert_eq!(first.role, Role::User);
        assert!(first.text.contains("failing test"));
        assert_eq!(first.cwd.as_deref(), Some("/work/acme/api"));
        assert_eq!(first.seq, 0);

        // A tool_use record named after the invoked tool, carrying its input.
        let tool_use = delta
            .messages
            .iter()
            .find(|m| m.tool_name.as_deref() == Some("Bash"))
            .expect("Bash tool_use record");
        assert_eq!(tool_use.role, Role::Tool);
        assert!(tool_use.text.contains("cargo test"));

        // seq is monotonic and contiguous.
        for (i, m) in delta.messages.iter().enumerate() {
            assert_eq!(m.seq, i as u64);
        }
    }

    #[test]
    fn parse_incremental_from_nonzero_offset() {
        let src = ClaudeSource::new(fixtures_root());
        let files = src.discover().unwrap();
        let main = files
            .iter()
            .find(|f| f.path.file_name().unwrap() == "session-main.jsonl")
            .unwrap();
        // First pass over the whole file.
        let full = src.parse(main, 0).unwrap();
        assert!(full.messages.len() >= 2);

        // Resume from a nonzero offset: skip past the leading metadata event and
        // the first user message so the second pass sees strictly fewer records
        // than the full pass. (The first line is a message-less `mode` event.)
        let offset_after_two_lines = {
            let contents = std::fs::read_to_string(&main.path).unwrap();
            contents
                .split_inclusive('\n')
                .take(2)
                .map(str::len)
                .sum::<usize>() as u64
        };
        let partial = src.parse(main, offset_after_two_lines).unwrap();
        assert_eq!(partial.new_watermark, main.size);
        assert!(
            partial.messages.len() < full.messages.len(),
            "resuming past the first line must yield fewer records"
        );
    }

    #[test]
    fn map_events_skips_non_message_events() {
        // A mode event and a summary event carry no `message`, and a `user`
        // event that is missing its `message` object is skipped too (the
        // `let ... else { continue }` arm).
        let events = vec![
            serde_json::json!({"type": "mode", "sessionId": "s", "mode": "default"}),
            serde_json::json!({"type": "summary", "summary": "did things"}),
            serde_json::json!({"type": "user", "sessionId": "s"}),
        ];
        let delta = map_events(&events, 0, 42, None);
        assert!(delta.messages.is_empty());
        assert_eq!(delta.session.session_id, "s");
        assert_eq!(delta.new_watermark, 42);
        assert_eq!(delta.session.first_ts, 0);
    }

    #[test]
    fn map_events_tracks_first_and_last_timestamps() {
        // Two timestamped message events exercise the min/max first_ts/last_ts
        // update arms (the second event widens `last_ts`).
        let events = vec![
            serde_json::json!({
                "type": "user",
                "sessionId": "s",
                "cwd": "/proj",
                "gitBranch": "main",
                "timestamp": "2020-01-01T00:00:10Z",
                "message": {"role": "user", "content": "first"}
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2020-01-01T00:00:20Z",
                "message": {"role": "assistant", "content": "second"}
            }),
        ];
        let delta = map_events(&events, 0, 99, None);
        assert!(delta.messages.len() >= 2);
        assert_eq!(delta.session.project_path.as_deref(), Some("/proj"));
        assert_eq!(delta.session.git_branch.as_deref(), Some("main"));
        assert!(delta.session.last_ts > delta.session.first_ts);
    }

    #[test]
    fn source_usable_as_trait_object() {
        let src = ClaudeSource::new(fixtures_root());
        let dynsrc: &dyn SessionSource = &src;
        assert_eq!(dynsrc.tool(), Tool::ClaudeCode);
        assert!(!dynsrc.discover().unwrap().is_empty());
        // Account delegation and config_dir go through the trait object.
        let _ = dynsrc.account().unwrap();
        assert!(dynsrc.config_dir().unwrap().contains("claude"));
    }

    #[test]
    fn tool_result_text_handles_string_and_array() {
        assert_eq!(
            tool_result_text(Some(&serde_json::json!("hi"))),
            "hi".to_string()
        );
        assert_eq!(
            tool_result_text(Some(&serde_json::json!([
                {"type": "text", "text": "a"},
                {"type": "text", "text": "b"},
            ]))),
            "a\nb".to_string()
        );
        assert_eq!(tool_result_text(None), String::new());
        assert_eq!(tool_result_text(Some(&serde_json::json!(5))), String::new());
    }

    #[test]
    fn thinking_block_maps_to_outer_role() {
        let msg = serde_json::json!({
            "role": "assistant",
            "content": [{"type": "thinking", "thinking": "let me reason"}]
        });
        let mut seq = 0;
        let recs = flatten_message(
            &msg,
            Role::Assistant,
            "s",
            Some(10),
            &serde_json::json!({"uuid": "u"}),
            &mut seq,
            Some("/c"),
        );
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].role, Role::Assistant);
        assert_eq!(recs[0].text, "let me reason");
        assert_eq!(recs[0].uuid.as_deref(), Some("u"));
        assert_eq!(recs[0].cwd.as_deref(), Some("/c"));
    }

    #[test]
    fn flatten_skips_empty_text_without_tool() {
        let msg = serde_json::json!({"role": "user", "content": "   "});
        let mut seq = 0;
        let recs = flatten_message(
            &msg,
            Role::User,
            "s",
            None,
            &serde_json::json!({}),
            &mut seq,
            None,
        );
        assert!(recs.is_empty());
        // Non-string, non-array content yields nothing too.
        let msg2 = serde_json::json!({"role": "user", "content": 5});
        let recs2 = flatten_message(
            &msg2,
            Role::User,
            "s",
            None,
            &serde_json::json!({}),
            &mut seq,
            None,
        );
        assert!(recs2.is_empty());
    }

    #[test]
    fn map_content_block_ignores_unknown_types() {
        let mut calls = 0;
        let mut push = |_r: Role, _n: Option<String>, _t: String| calls += 1;
        map_content_block(&serde_json::json!({"type": "image"}), Role::User, &mut push);
        map_content_block(&serde_json::json!({"no": "type"}), Role::User, &mut push);
        assert_eq!(calls, 0);
    }

    #[test]
    fn derive_project_name_from_path() {
        assert_eq!(derive_project_name("/a/b/api"), "api");
        assert_eq!(derive_project_name("api"), "api");
        assert_eq!(derive_project_name("/"), "/");
    }

    #[test]
    fn iso8601_parser_matches_known_epochs() {
        // 1970-01-01T00:00:00Z == 0
        assert_eq!(parse_iso8601_secs("1970-01-01T00:00:00Z"), Some(0));
        // 2000-01-01T00:00:00Z == 946684800
        assert_eq!(
            parse_iso8601_secs("2000-01-01T00:00:00Z"),
            Some(946_684_800)
        );
        // With fractional seconds, the millis are ignored.
        assert_eq!(
            parse_iso8601_secs("2000-01-01T00:00:00.999Z"),
            Some(946_684_800)
        );
        // 2026-07-04T13:44:20Z
        assert_eq!(
            parse_iso8601_secs("2026-07-04T13:44:20Z"),
            Some(1_783_172_660)
        );
    }

    #[test]
    fn iso8601_parser_rejects_bad_input() {
        assert_eq!(parse_iso8601_secs("nope"), None);
        assert_eq!(parse_iso8601_secs("2026/07/04T00:00:00Z"), None);
        assert_eq!(parse_iso8601_secs("2026-07-04X00:00:00Z"), None);
        assert_eq!(parse_iso8601_secs("2026-13-40Txx:00:00Z"), None);
        assert_eq!(parse_iso8601_secs(""), None);
    }

    #[test]
    fn timestamp_secs_reads_field() {
        let v = serde_json::json!({"timestamp": "2000-01-01T00:00:00Z"});
        assert_eq!(timestamp_secs(&v), Some(946_684_800));
        assert_eq!(timestamp_secs(&serde_json::json!({})), None);
    }
}
