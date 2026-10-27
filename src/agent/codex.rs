//! Codex CLI agent detection.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use walkdir::WalkDir;

use super::{base_info, Agent, AgentContext};
use crate::source::claude::parse_iso8601_secs;
use crate::tabs::process::Process;
use crate::tabs::AgentInfo;

const WEEK_MS: i64 = 7 * 86_400 * 1000;
/// Slack between process start and rollout start, in milliseconds.
const START_SLACK_MS: i64 = 5000;
/// Longest title, in characters.
const TITLE_CHARS: usize = 80;

/// Codex CLI.
pub struct CodexAgent;

/// A rollout file whose header was read.
struct Rollout {
    id: String,
    cwd: Option<String>,
    started_ms: i64,
    mtime_ms: i64,
    path: std::path::PathBuf,
}

fn mtime_ms(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

fn first_line(path: &Path) -> Option<String> {
    File::open(path).ok().and_then(|f| {
        let mut line = String::new();
        BufReader::new(f).read_line(&mut line).ok().map(|_| line)
    })
}

fn read_header(path: &Path) -> Option<(String, Option<String>, i64)> {
    let v: Value = serde_json::from_str(&first_line(path)?).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let id = v
        .pointer("/payload/id")
        .and_then(Value::as_str)?
        .to_string();
    let cwd = v
        .pointer("/payload/cwd")
        .and_then(Value::as_str)
        .map(str::to_string);
    let secs = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_iso8601_secs)?;
    Some((id, cwd, secs * 1000))
}

fn is_rollout(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.starts_with("rollout-") && name.ends_with(".jsonl")
}

/// Newest recent rollout that started at or after `min_start_ms`.
fn find_rollout(ctx: &AgentContext, min_start_ms: i64) -> Option<Rollout> {
    WalkDir::new(ctx.codex_home.join("sessions"))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && is_rollout(e.path()))
        .filter_map(|e| {
            let mtime_ms = mtime_ms(e.path()).unwrap_or(0);
            if ctx.now_ms - mtime_ms > WEEK_MS {
                return None;
            }
            let (id, cwd, started_ms) = read_header(e.path())?;
            (started_ms >= min_start_ms).then(|| Rollout {
                id,
                cwd,
                started_ms,
                mtime_ms,
                path: e.path().to_path_buf(),
            })
        })
        .max_by_key(|r| r.started_ms)
}

/// True for context Codex injects as a user message, not typed by the user.
fn is_injected(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('<') || t.starts_with("# AGENTS.md")
}

/// First typed user `input_text` of a `response_item`, cut to [`TITLE_CHARS`].
fn first_user_text(path: &Path) -> Option<String> {
    File::open(path)
        .into_iter()
        .flat_map(|f| BufReader::new(f).lines().map_while(Result::ok))
        .find_map(|line| {
            let v: Value = serde_json::from_str(&line).ok()?;
            if v["type"] != "response_item"
                || v.pointer("/payload/role") != Some(&Value::from("user"))
            {
                return None;
            }
            let parts = v.pointer("/payload/content").and_then(Value::as_array)?;
            parts
                .iter()
                .filter(|p| p["type"] == "input_text")
                .filter_map(|p| p["text"].as_str())
                .find(|t| !is_injected(t))
                .map(|t| t.chars().take(TITLE_CHARS).collect())
        })
}

impl Agent for CodexAgent {
    fn tool(&self) -> &'static str {
        "codex"
    }

    fn matches(&self, p: &Process) -> bool {
        p.program() == "codex"
    }

    fn enrich(&self, p: &Process, ctx: &AgentContext) -> AgentInfo {
        let mut info = base_info(self.tool(), p);
        let start_ms = ctx.now_ms - (p.elapsed_secs as i64) * 1000;
        let Some(r) = find_rollout(ctx, start_ms - START_SLACK_MS) else {
            return info;
        };
        info.resume_command = Some(format!("codex resume {}", r.id));
        info.title = first_user_text(&r.path);
        info.session_id = Some(r.id);
        info.cwd = r.cwd;
        info.updated_at = Some(r.mtime_ms);
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    const NOW_MS: i64 = 1_783_172_660_000; // 2026-07-04T13:44:20Z

    fn proc(elapsed_secs: u64) -> Process {
        Process {
            pid: 5,
            ppid: 1,
            pgid: 5,
            tpgid: 5,
            tty: Some("ttys3".into()),
            elapsed_secs,
            args: "codex".into(),
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("csx-codex-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx(home: &Path) -> AgentContext {
        AgentContext {
            home: PathBuf::from("/nonexistent"),
            claude_dirs: vec![],
            codex_home: home.to_path_buf(),
            now_ms: NOW_MS,
        }
    }

    fn rollout(home: &Path, name: &str, id: &str, ts: &str, extra: &str, age_days: u64) -> PathBuf {
        let path = home.join(format!("sessions/2026/07/04/rollout-{name}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let meta = format!(
            "{{\"timestamp\":\"{ts}\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"/w/c\"}}}}\n"
        );
        fs::write(&path, format!("{meta}{extra}")).unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_millis(NOW_MS as u64)
            - Duration::from_secs(age_days * 86_400);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        path
    }

    fn user_line(text: &str) -> String {
        format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{text}\"}}]}}}}\n"
        )
    }

    #[test]
    fn codex_picks_newest_rollout_after_process_start() {
        let h = tmp("newest");
        // Process started 1000 s ago: 13:27:40Z.
        let long = "z".repeat(100);
        rollout(
            &h,
            "a",
            "id-old",
            "2026-07-04T13:30:00Z",
            &user_line("older"),
            0,
        );
        rollout(
            &h,
            "b",
            "id-new",
            "2026-07-04T13:40:00Z",
            &format!("garbage\n{}{{\"type\":\"x\"}}\n", user_line("second")),
            0,
        );
        // First user text wins; this rollout has a long one.
        let p = rollout(
            &h,
            "c",
            "id-newest",
            "2026-07-04T13:41:00Z",
            &user_line(&long),
            0,
        );
        let info = CodexAgent.enrich(&proc(1000), &ctx(&h));
        assert_eq!(info.tool, "codex");
        assert_eq!(info.pid, 5);
        assert_eq!(info.tty.as_deref(), Some("ttys3"));
        assert_eq!(info.session_id.as_deref(), Some("id-newest"));
        assert_eq!(info.cwd.as_deref(), Some("/w/c"));
        assert_eq!(info.title.as_deref(), Some(&long[..80]));
        assert_eq!(
            info.resume_command.as_deref(),
            Some("codex resume id-newest")
        );
        assert_eq!(info.updated_at, Some(NOW_MS));
        // Title taken from the first user message of a plain rollout.
        fs::remove_file(p).unwrap();
        let info = CodexAgent.enrich(&proc(1000), &ctx(&h));
        assert_eq!(info.session_id.as_deref(), Some("id-new"));
        assert_eq!(info.title.as_deref(), Some("second"));
    }

    #[test]
    fn codex_ignores_rollouts_before_start() {
        let h = tmp("before");
        // Process started 100 s ago (13:42:40Z); rollout began earlier.
        rollout(&h, "a", "id-early", "2026-07-04T13:00:00Z", "", 0);
        // Stale by mtime (8 days), even though it starts after the process.
        rollout(&h, "b", "id-stale", "2026-07-04T13:44:00Z", "", 8);
        // Bad meta lines are skipped.
        let bad = h.join("sessions/2026/07/04/rollout-bad.jsonl");
        fs::write(&bad, "{\"type\":\"other\"}\n").unwrap();
        let bad2 = h.join("sessions/2026/07/04/rollout-bad2.jsonl");
        fs::write(&bad2, "").unwrap();
        let bad3 = h.join("sessions/2026/07/04/rollout-bad3.jsonl");
        fs::write(
            &bad3,
            "{\"type\":\"session_meta\",\"timestamp\":\"nope\",\"payload\":{\"id\":\"x\"}}\n",
        )
        .unwrap();
        let bad4 = h.join("sessions/2026/07/04/rollout-bad4.jsonl");
        fs::write(
            &bad4,
            "{\"type\":\"session_meta\",\"timestamp\":\"2026-07-04T13:44:00Z\",\"payload\":{}}\n",
        )
        .unwrap();
        // Not a rollout file.
        fs::write(h.join("sessions/2026/07/04/notes.jsonl"), "x").unwrap();
        let info = CodexAgent.enrich(&proc(100), &ctx(&h));
        assert_eq!(info.tool, "codex");
        assert_eq!(info.session_id, None);
        assert_eq!(info.resume_command, None);
        // Missing sessions dir.
        let empty = tmp("empty");
        assert_eq!(CodexAgent.enrich(&proc(100), &ctx(&empty)).session_id, None);
    }

    #[test]
    fn codex_title_absent_without_user_text() {
        let h = tmp("notitle");
        let extra = concat!(
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"bare\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_image\"}]}}\n",
        );
        rollout(&h, "a", "id-1", "2026-07-04T13:44:00Z", extra, 0);
        let info = CodexAgent.enrich(&proc(100), &ctx(&h));
        assert_eq!(info.session_id.as_deref(), Some("id-1"));
        assert_eq!(info.title, None);
    }

    #[test]
    fn codex_title_skips_injected_context() {
        let h = tmp("injected");
        let extra = format!(
            "{}{}{}",
            user_line("  <environment_context>\\n  <cwd>/w</cwd>"),
            user_line("# AGENTS.md instructions for /w"),
            user_line("the real prompt"),
        );
        rollout(&h, "a", "id-1", "2026-07-04T13:44:00Z", &extra, 0);
        let info = CodexAgent.enrich(&proc(100), &ctx(&h));
        assert_eq!(info.title.as_deref(), Some("the real prompt"));
        let only = format!(
            "{}{}",
            user_line("<user_instructions>x</user_instructions>"),
            user_line("<environment_context>y")
        );
        let h2 = tmp("injected-only");
        rollout(&h2, "a", "id-2", "2026-07-04T13:44:00Z", &only, 0);
        let info = CodexAgent.enrich(&proc(100), &ctx(&h2));
        assert_eq!(info.session_id.as_deref(), Some("id-2"));
        assert_eq!(info.title, None);
    }

    #[test]
    fn codex_matches_by_program() {
        assert!(CodexAgent.matches(&proc(1)));
        let mut p = proc(1);
        p.args = "vim".into();
        assert!(!CodexAgent.matches(&p));
        assert_eq!(CodexAgent.tool(), "codex");
    }
}
