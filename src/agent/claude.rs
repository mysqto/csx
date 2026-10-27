//! Claude Code agent detection.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use super::{base_info, is_uuid, Agent, AgentContext};
use crate::tabs::process::Process;
use crate::tabs::{AgentInfo, AgentStatus};

/// How many bytes at the end of a transcript are read.
const TAIL_BYTES: u64 = 400_000;

/// Claude Code.
pub struct ClaudeAgent;

/// Titles and prompt found in a transcript tail.
#[derive(Default)]
struct TranscriptInfo {
    custom_title: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Directory name Claude uses for a project: every non-alphanumeric byte
/// becomes `-`.
fn slug(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Quote `s` for a POSIX shell inside single quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Read the last [`TAIL_BYTES`] of a file as text; a cut first line is dropped.
fn read_tail(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().map_or(0, |m| m.len());
    let start = len.saturating_sub(TAIL_BYTES);
    let mut buf = Vec::new();
    // A failed read leaves `buf` short or empty, which yields no titles.
    let _ = f
        .seek(SeekFrom::Start(start))
        .and_then(|_| f.read_to_end(&mut buf));
    let text = String::from_utf8_lossy(&buf).into_owned();
    if start == 0 {
        return Some(text);
    }
    Some(match text.split_once('\n') {
        Some((_, rest)) => rest.to_string(),
        None => String::new(),
    })
}

fn scan_transcript(text: &str) -> TranscriptInfo {
    let mut t = TranscriptInfo::default();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("custom-title") => t.custom_title = str_of(&v, "customTitle").or(t.custom_title),
            Some("ai-title") => t.ai_title = str_of(&v, "aiTitle").or(t.ai_title),
            Some("last-prompt") => t.last_prompt = str_of(&v, "lastPrompt").or(t.last_prompt),
            _ => {}
        }
    }
    t
}

fn read_session(ctx: &AgentContext, pid: u32) -> Option<(std::path::PathBuf, Value)> {
    ctx.claude_dirs.iter().find_map(|dir| {
        let text =
            std::fs::read_to_string(dir.join("sessions").join(format!("{pid}.json"))).ok()?;
        let v = serde_json::from_str::<Value>(&text).ok()?;
        Some((dir.clone(), v))
    })
}

impl Agent for ClaudeAgent {
    fn tool(&self) -> &'static str {
        "claude-code"
    }

    fn matches(&self, p: &Process) -> bool {
        p.program() == "claude"
    }

    fn enrich(&self, p: &Process, ctx: &AgentContext) -> AgentInfo {
        let mut info = base_info(self.tool(), p);
        let Some((dir, sess)) = read_session(ctx, p.pid) else {
            return info;
        };
        info.session_id = str_of(&sess, "sessionId");
        info.cwd = str_of(&sess, "cwd");
        info.status = match sess.get("status").and_then(Value::as_str) {
            Some("busy") => AgentStatus::Busy,
            Some("idle") => AgentStatus::Idle,
            _ => AgentStatus::Unknown,
        };
        info.updated_at = sess.get("updatedAt").and_then(Value::as_i64);
        let mut tr = TranscriptInfo::default();
        if let (Some(cwd), Some(sid)) = (&info.cwd, &info.session_id) {
            let path = dir
                .join("projects")
                .join(slug(cwd))
                .join(format!("{sid}.jsonl"));
            if let Some(text) = read_tail(&path) {
                tr = scan_transcript(&text);
            }
            info.resume_command =
                is_uuid(sid).then(|| format!("cd {} && claude --resume {sid}", shell_quote(cwd)));
        }
        info.title = tr
            .custom_title
            .or(tr.ai_title)
            .or_else(|| str_of(&sess, "name"));
        info.last_prompt = tr.last_prompt;
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn proc(pid: u32) -> Process {
        Process {
            pid,
            ppid: 1,
            pgid: pid,
            tpgid: pid,
            tty: Some("ttys9".into()),
            elapsed_secs: 5,
            args: "claude".into(),
        }
    }

    fn ctx(dirs: Vec<PathBuf>) -> AgentContext {
        AgentContext {
            home: PathBuf::from("/nonexistent"),
            claude_dirs: dirs,
            codex_home: PathBuf::from("/nonexistent"),
            now_ms: 0,
        }
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn session(dir: &Path, pid: u32, cwd: &str, status: &str) {
        write(
            &dir.join(format!("sessions/{pid}.json")),
            &format!(
                r#"{{"sessionId":"aaaaaaaa-1111-2222-3333-444444444444","cwd":"{cwd}","status":"{status}","name":"the name","updatedAt":1700000000123}}"#
            ),
        );
    }

    fn transcript(dir: &Path, cwd: &str, body: &str) {
        let slug: String = cwd
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        write(
            &dir.join(format!(
                "projects/{slug}/aaaaaaaa-1111-2222-3333-444444444444.jsonl"
            )),
            body,
        );
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("csx-claude-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn claude_reads_session_file_and_transcript_titles() {
        let d = tmp("basic");
        session(&d, 11, "/w/my.proj", "busy");
        transcript(
            &d,
            "/w/my.proj",
            concat!(
                "{\"type\":\"ai-title\",\"aiTitle\":\"first ai\"}\n",
                "not json\n",
                "{\"type\":\"user\"}\n",
                "{\"type\":\"ai-title\",\"aiTitle\":\"last ai\"}\n",
                "{\"type\":\"last-prompt\",\"lastPrompt\":\"p1\"}\n",
                "{\"type\":\"last-prompt\",\"lastPrompt\":\"p2\"}\n",
            ),
        );
        let info = ClaudeAgent.enrich(&proc(11), &ctx(vec![d.clone()]));
        assert_eq!(info.tool, "claude-code");
        assert_eq!(info.pid, 11);
        assert_eq!(info.tty.as_deref(), Some("ttys9"));
        assert_eq!(info.cwd.as_deref(), Some("/w/my.proj"));
        assert_eq!(
            info.session_id.as_deref(),
            Some("aaaaaaaa-1111-2222-3333-444444444444")
        );
        assert_eq!(info.status, AgentStatus::Busy);
        assert_eq!(info.updated_at, Some(1_700_000_000_123));
        assert_eq!(info.title.as_deref(), Some("last ai"));
        assert_eq!(info.last_prompt.as_deref(), Some("p2"));
        assert_eq!(
            info.resume_command.as_deref(),
            Some("cd '/w/my.proj' && claude --resume aaaaaaaa-1111-2222-3333-444444444444")
        );
        session(&d, 12, "/w/my.proj", "idle");
        let i = ClaudeAgent.enrich(&proc(12), &ctx(vec![d.clone()]));
        assert_eq!(i.status, AgentStatus::Idle);
        session(&d, 13, "/w/my.proj", "weird");
        let i = ClaudeAgent.enrich(&proc(13), &ctx(vec![d]));
        assert_eq!(i.status, AgentStatus::Unknown);
    }

    #[test]
    fn claude_custom_title_beats_ai_title() {
        let d = tmp("custom");
        session(&d, 11, "/w/a", "idle");
        transcript(
            &d,
            "/w/a",
            "{\"type\":\"custom-title\",\"customTitle\":\"mine\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"ai\"}\n",
        );
        let info = ClaudeAgent.enrich(&proc(11), &ctx(vec![d.clone()]));
        assert_eq!(info.title.as_deref(), Some("mine"));
        // No transcript: falls back to the session name.
        let _ = fs::remove_dir_all(d.join("projects"));
        let info = ClaudeAgent.enrich(&proc(11), &ctx(vec![d]));
        assert_eq!(info.title.as_deref(), Some("the name"));
        assert_eq!(info.last_prompt, None);
    }

    #[test]
    fn claude_tail_skips_cut_first_line() {
        let d = tmp("tail");
        session(&d, 11, "/w/a", "idle");
        let bad = "{\"type\":\"custom-title\",\"customTitle\":\"BAD\"}";
        let good_title = "{\"type\":\"ai-title\",\"aiTitle\":\"GOOD\"}\n";
        // The tail window is exactly `bad` (cut line remainder), a newline, then `rest`.
        let fixed = good_title.len() + "{\"type\":\"last-prompt\",\"lastPrompt\":\"\"}\n".len();
        let pad = 400_000 - bad.len() - 1 - fixed;
        let rest = format!(
            "{good_title}{{\"type\":\"last-prompt\",\"lastPrompt\":\"{}\"}}\n",
            "x".repeat(pad)
        );
        let prefix = "y".repeat(50_000);
        let body = format!("{prefix}{bad}\n{rest}");
        assert_eq!(body.len() - prefix.len(), 400_000);
        transcript(&d, "/w/a", &body);
        let info = ClaudeAgent.enrich(&proc(11), &ctx(vec![d]));
        assert_eq!(info.title.as_deref(), Some("GOOD"));
    }

    #[test]
    fn claude_without_session_file_has_tool_only() {
        let d = tmp("none");
        let info = ClaudeAgent.enrich(&proc(99), &ctx(vec![d.clone()]));
        assert_eq!(
            info,
            AgentInfo {
                tool: "claude-code".into(),
                pid: 99,
                tty: Some("ttys9".into()),
                ..AgentInfo::default()
            }
        );
        write(&d.join("sessions/98.json"), "{broken");
        let info = ClaudeAgent.enrich(&proc(98), &ctx(vec![d.clone()]));
        assert_eq!(info.session_id, None);
        write(&d.join("sessions/97.json"), "{\"cwd\":\"/x\"}");
        let info = ClaudeAgent.enrich(&proc(97), &ctx(vec![d]));
        assert_eq!(info.session_id, None);
    }

    #[test]
    fn claude_searches_every_config_dir() {
        let a = tmp("dir-a");
        let b = tmp("dir-b");
        session(&b, 11, "/w/b", "busy");
        let info = ClaudeAgent.enrich(&proc(11), &ctx(vec![a, b]));
        assert_eq!(info.cwd.as_deref(), Some("/w/b"));
        assert_eq!(info.status, AgentStatus::Busy);
    }

    #[test]
    fn claude_resume_command_quotes_cwd() {
        let d = tmp("quote");
        session(&d, 11, "/w/it's here", "idle");
        let info = ClaudeAgent.enrich(&proc(11), &ctx(vec![d]));
        assert_eq!(
            info.resume_command.as_deref(),
            Some("cd '/w/it'\\''s here' && claude --resume aaaaaaaa-1111-2222-3333-444444444444")
        );
    }

    #[test]
    fn claude_resume_command_needs_a_uuid_session_id() {
        let d = tmp("crafted");
        write(
            &d.join("sessions/11.json"),
            r#"{"sessionId":"x; rm -rf ~","cwd":"/w/c","status":"idle"}"#,
        );
        let info = ClaudeAgent.enrich(&proc(11), &ctx(vec![d]));
        assert_eq!(info.session_id.as_deref(), Some("x; rm -rf ~"));
        assert_eq!(info.resume_command, None);
    }

    #[test]
    fn claude_matches_by_program() {
        assert!(ClaudeAgent.matches(&proc(1)));
        let mut p = proc(1);
        p.args = "node /x/claude/cli.js".into();
        assert!(ClaudeAgent.matches(&p));
        p.args = "vim".into();
        assert!(!ClaudeAgent.matches(&p));
        assert_eq!(ClaudeAgent.tool(), "claude-code");
    }

    #[test]
    fn claude_session_without_cwd_has_no_transcript_or_resume() {
        let d = tmp("nocwd");
        write(
            &d.join("sessions/11.json"),
            "{\"sessionId\":\"s\",\"name\":\"n\"}",
        );
        let info = ClaudeAgent.enrich(&proc(11), &ctx(vec![d]));
        assert_eq!(info.session_id.as_deref(), Some("s"));
        assert_eq!(info.title.as_deref(), Some("n"));
        assert_eq!(info.resume_command, None);
        assert_eq!(info.updated_at, None);
    }
}
