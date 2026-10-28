//! AI-tool agent detection.

pub mod claude;
pub mod codex;
pub mod devin;
pub mod generic;

use std::path::{Path, PathBuf};

use crate::tabs::process::{Process, ProcessTable};
use crate::tabs::AgentInfo;

/// True for a UUID: 8-4-4-4-12 hex digits. Session ids are checked with this
/// before they go into a command line that a user may paste.
pub(crate) fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Quote `s` for a POSIX shell inside single quotes.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The tools whose sessions csx can resume from the index.
pub const RESUMABLE_TOOLS: &[&str] = &["claude-code", "codex", "devin"];

/// The command that resumes session `session_id` of `tool`, or `None` for a
/// tool csx cannot resume or an id that fails the tool's id check (UUIDs for
/// Claude Code and Codex, session names for Devin).
pub fn resume_command(tool: &str, session_id: &str) -> Option<String> {
    match tool {
        "claude-code" if is_uuid(session_id) => Some(format!("claude --resume {session_id}")),
        "codex" if is_uuid(session_id) => Some(format!("codex resume {session_id}")),
        "devin" if devin::is_session_name(session_id) => Some(format!("devin -r {session_id}")),
        _ => None,
    }
}

/// `cd '<cwd>' && [NAME='<value>' ...]<resume command>`: a line that resumes
/// the session from any folder. `None` when [`resume_command`] is `None`.
pub fn resume_line(
    tool: &str,
    session_id: &str,
    cwd: &str,
    env: &[(&str, String)],
) -> Option<String> {
    let command = resume_command(tool, session_id)?;
    let vars: String = env
        .iter()
        .map(|(name, value)| format!("{name}={} ", shell_quote(value)))
        .collect();
    Some(format!("cd {} && {vars}{command}", shell_quote(cwd)))
}

/// The variable that points `tool` at its config folder `config_dir`, when
/// that folder is not the tool's default under `home`: `CLAUDE_CONFIG_DIR`,
/// `CODEX_HOME`, or `XDG_DATA_HOME` (the folder above `devin/cli`) for Devin.
/// Empty for the default folder, no folder, an unknown tool, or a Devin folder
/// that does not end in `devin/cli`.
pub fn session_env(
    tool: &str,
    config_dir: Option<&str>,
    home: &Path,
) -> Vec<(&'static str, String)> {
    let Some(dir) = config_dir.filter(|d| !d.is_empty()).map(Path::new) else {
        return Vec::new();
    };
    let var = match tool {
        "claude-code" if dir != home.join(".claude") => Some(("CLAUDE_CONFIG_DIR", dir)),
        "codex" if dir != home.join(".codex") => Some(("CODEX_HOME", dir)),
        "devin" if dir != home.join(".local/share/devin/cli") && dir.ends_with("devin/cli") => dir
            .parent()
            .and_then(Path::parent)
            .map(|d| ("XDG_DATA_HOME", d)),
        _ => None,
    };
    var.map(|(name, d)| (name, d.to_string_lossy().into_owned()))
        .into_iter()
        .collect()
}

/// Where agents look for session data, and the current time.
#[derive(Debug, Clone)]
pub struct AgentContext {
    /// The user's home directory.
    pub home: PathBuf,
    /// Claude config directories, deduplicated, in the order given.
    pub claude_dirs: Vec<PathBuf>,
    /// Codex home directory.
    pub codex_home: PathBuf,
    /// Devin CLI data root (`~/.local/share/devin/cli` by default).
    pub devin_home: PathBuf,
    /// Current time in unix milliseconds.
    pub now_ms: i64,
}

/// An AI coding tool that can be recognized in the process table.
pub trait Agent {
    /// Stable tool id, for example `claude-code`.
    fn tool(&self) -> &'static str;
    /// True when `p` is a process of this tool.
    fn matches(&self, p: &Process) -> bool;
    /// Fill an [`AgentInfo`] for `p`. Never fails; missing data is `None`.
    fn enrich(&self, p: &Process, ctx: &AgentContext) -> AgentInfo;
}

/// The info every tool provides: tool id, pid and tty.
pub(crate) fn base_info(tool: &str, p: &Process) -> AgentInfo {
    AgentInfo {
        tool: tool.to_string(),
        pid: p.pid,
        tty: p.tty.clone(),
        ..AgentInfo::default()
    }
}

/// All known agents: Claude Code, Codex, Devin, then the generic tools.
pub fn registry() -> Vec<Box<dyn Agent>> {
    let mut v: Vec<Box<dyn Agent>> = vec![
        Box::new(claude::ClaudeAgent),
        Box::new(codex::CodexAgent),
        Box::new(devin::DevinAgent),
    ];
    for t in generic::GENERIC_TOOLS {
        v.push(Box::new(*t));
    }
    v
}

/// Lower is better: foreground first, then the lowest pid.
fn rank(p: &Process) -> (bool, u32) {
    (!p.is_foreground(), p.pid)
}

/// One [`AgentInfo`] per matching process that has a tty (a daemon such as
/// `codex app-server` has none and is not a session). When several processes of the same
/// tool share a tty, keep the foreground one, else the lowest pid.
pub fn detect(
    table: &ProcessTable,
    agents: &[Box<dyn Agent>],
    ctx: &AgentContext,
) -> Vec<AgentInfo> {
    let mut picked: Vec<(&dyn Agent, &Process)> = Vec::new();
    for p in &table.procs {
        let Some(agent) = agents.iter().find(|a| a.matches(p)) else {
            continue;
        };
        if p.tty.is_none() {
            continue;
        }
        let same = picked
            .iter()
            .position(|(a, q)| a.tool() == agent.tool() && q.tty == p.tty);
        match same {
            Some(i) if rank(p) < rank(picked[i].1) => picked[i] = (agent.as_ref(), p),
            Some(_) => {}
            None => picked.push((agent.as_ref(), p)),
        }
    }
    picked.into_iter().map(|(a, p)| a.enrich(p, ctx)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> AgentContext {
        AgentContext {
            home: PathBuf::from("/nonexistent"),
            claude_dirs: vec![PathBuf::from("/nonexistent/claude")],
            codex_home: PathBuf::from("/nonexistent/codex"),
            devin_home: PathBuf::from("/nonexistent/devin"),
            now_ms: 0,
        }
    }

    fn proc(pid: u32, pgid: u32, tpgid: u32, tty: Option<&str>, args: &str) -> Process {
        Process {
            pid,
            ppid: 1,
            pgid,
            tpgid,
            tty: tty.map(str::to_string),
            elapsed_secs: 10,
            args: args.into(),
        }
    }

    #[test]
    fn registry_order_and_unique_ids() {
        let r = registry();
        let ids: Vec<&str> = r.iter().map(|a| a.tool()).collect();
        assert_eq!(&ids[..3], &["claude-code", "codex", "devin"]);
        assert_eq!(ids.len(), 3 + generic::GENERIC_TOOLS.len());
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
    }

    #[test]
    fn detect_keeps_foreground_per_tty_and_skips_tty_less_processes() {
        let table = ProcessTable {
            procs: vec![
                proc(30, 30, 99, Some("ttys1"), "claude"),
                proc(20, 20, 99, Some("ttys1"), "claude"),
                proc(40, 99, 99, Some("ttys1"), "claude"),
                proc(50, 99, 99, Some("ttys1"), "claude"),
                proc(60, 60, 99, Some("ttys2"), "claude"),
                proc(70, 70, 99, Some("ttys2"), "codex"),
                proc(80, 80, 99, None, "gemini"),
                proc(81, 81, 99, None, "gemini"),
                proc(90, 90, 99, Some("ttys3"), "vim"),
            ],
        };
        let got = detect(&table, &registry(), &ctx());
        let key: Vec<(&str, u32)> = got.iter().map(|a| (a.tool.as_str(), a.pid)).collect();
        assert_eq!(
            key,
            vec![("claude-code", 40), ("claude-code", 60), ("codex", 70)]
        );
        assert_eq!(got[0].tty.as_deref(), Some("ttys1"));
    }

    const U: &str = "0f1e2d3c-4b5a-4968-8776-5a4b3c2d1e0f";

    #[test]
    fn resume_command_per_tool() {
        assert_eq!(
            resume_command("claude-code", U),
            Some(format!("claude --resume {U}"))
        );
        assert_eq!(
            resume_command("codex", U),
            Some(format!("codex resume {U}"))
        );
        assert_eq!(
            resume_command("devin", "swamp-movie").as_deref(),
            Some("devin -r swamp-movie")
        );
        assert_eq!(resume_command("devin", "Bad Id"), None);
        assert_eq!(resume_command("claude-code", "x;rm"), None);
        assert_eq!(resume_command("codex", "swamp-movie"), None);
        assert_eq!(resume_command("gemini", U), None);
        assert_eq!(RESUMABLE_TOOLS, ["claude-code", "codex", "devin"]);
    }

    #[test]
    fn resume_line_quotes_folder_and_env() {
        assert_eq!(
            resume_line(
                "claude-code",
                U,
                "/Users/me/it's here",
                &[("CLAUDE_CONFIG_DIR", "/Users/me/.claude-b".into())]
            ),
            Some(format!(
                "cd '/Users/me/it'\\''s here' && CLAUDE_CONFIG_DIR='/Users/me/.claude-b' claude --resume {U}"
            ))
        );
        assert_eq!(
            resume_line("devin", "swamp-movie", "/Users/me/work/acme", &[]).as_deref(),
            Some("cd '/Users/me/work/acme' && devin -r swamp-movie")
        );
        assert_eq!(resume_line("gemini", U, "/w", &[]), None);
    }

    #[test]
    fn session_env_defaults_and_overrides() {
        let h = Path::new("/h");
        for (tool, dir) in [
            ("claude-code", "/h/.claude"),
            ("claude-code", "/h/.claude/"),
            ("codex", "/h/.codex"),
            ("devin", "/h/.local/share/devin/cli"),
        ] {
            assert!(session_env(tool, Some(dir), h).is_empty(), "{tool} {dir}");
        }
        assert_eq!(
            session_env("claude-code", Some("/h/.claude-b"), h),
            [("CLAUDE_CONFIG_DIR", "/h/.claude-b".to_string())]
        );
        assert_eq!(
            session_env("codex", Some("/x/codex"), h),
            [("CODEX_HOME", "/x/codex".to_string())]
        );
        assert_eq!(
            session_env("devin", Some("/x/devin/cli"), h),
            [("XDG_DATA_HOME", "/x".to_string())]
        );
        // A Devin folder that is not `<data home>/devin/cli` has no variable.
        assert!(session_env("devin", Some("/x/other"), h).is_empty());
        assert!(session_env("codex", None, h).is_empty());
        assert!(session_env("codex", Some(""), h).is_empty());
        assert!(session_env("gemini", Some("/x"), h).is_empty());
    }

    #[test]
    fn detect_lowest_pid_when_none_foreground() {
        let table = ProcessTable {
            procs: vec![
                proc(30, 30, 99, Some("ttys1"), "claude"),
                proc(20, 20, 99, Some("ttys1"), "claude"),
            ],
        };
        let got = detect(&table, &registry(), &ctx());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].pid, 20);
    }
}
