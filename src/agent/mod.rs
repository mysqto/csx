//! AI-tool agent detection.

pub mod claude;
pub mod codex;
pub mod generic;

use std::path::PathBuf;

use crate::tabs::process::{Process, ProcessTable};
use crate::tabs::AgentInfo;

/// Where agents look for session data, and the current time.
#[derive(Debug, Clone)]
pub struct AgentContext {
    /// The user's home directory.
    pub home: PathBuf,
    /// Claude config directories, deduplicated, in the order given.
    pub claude_dirs: Vec<PathBuf>,
    /// Codex home directory.
    pub codex_home: PathBuf,
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

/// All known agents: Claude Code, Codex, then the generic tools.
pub fn registry() -> Vec<Box<dyn Agent>> {
    let mut v: Vec<Box<dyn Agent>> =
        vec![Box::new(claude::ClaudeAgent), Box::new(codex::CodexAgent)];
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
        assert_eq!(&ids[..2], &["claude-code", "codex"]);
        assert_eq!(ids.len(), 2 + generic::GENERIC_TOOLS.len());
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
