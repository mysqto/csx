//! Generic agent detection for other AI tools.

use super::{base_info, Agent, AgentContext};
use crate::tabs::process::Process;
use crate::tabs::AgentInfo;

/// A tool recognized by program name only.
#[derive(Debug, Clone, Copy)]
pub struct NamedTool {
    /// Stable tool id.
    pub tool: &'static str,
    /// Program names that identify the tool.
    pub programs: &'static [&'static str],
}

macro_rules! tools {
    ($($n:literal),*) => { &[$(NamedTool { tool: $n, programs: &[$n] }),*] };
}

/// Tools detected by name, with tool id equal to the program name.
pub const GENERIC_TOOLS: &[NamedTool] = tools!(
    "gemini",
    "opencode",
    "copilot",
    "cursor-agent",
    "aider",
    "qwen",
    "goose",
    "amp",
    "crush",
    "droid"
);

impl Agent for NamedTool {
    fn tool(&self) -> &'static str {
        self.tool
    }

    fn matches(&self, p: &Process) -> bool {
        let prog = p.program();
        self.programs.contains(&prog.as_str())
    }

    fn enrich(&self, p: &Process, _ctx: &AgentContext) -> AgentInfo {
        base_info(self.tool, p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn proc(args: &str) -> Process {
        Process {
            pid: 7,
            ppid: 1,
            pgid: 7,
            tpgid: 7,
            tty: Some("ttys4".into()),
            elapsed_secs: 1,
            args: args.into(),
        }
    }

    #[test]
    fn generic_matches_node_script_name() {
        let gemini = &GENERIC_TOOLS[0];
        assert!(gemini.matches(&proc("node /opt/lib/gemini/dist/index.js --x")));
        assert!(gemini.matches(&proc("/usr/local/bin/gemini")));
        assert!(!gemini.matches(&proc("node /opt/other.js")));
        assert_eq!(gemini.tool(), "gemini");
    }

    #[test]
    fn generic_enrich_fills_tool_pid_tty_only() {
        let ctx = AgentContext {
            home: PathBuf::new(),
            claude_dirs: vec![],
            codex_home: PathBuf::new(),
            now_ms: 0,
        };
        let info = GENERIC_TOOLS[1].enrich(&proc("opencode"), &ctx);
        assert_eq!(
            info,
            AgentInfo {
                tool: "opencode".into(),
                pid: 7,
                tty: Some("ttys4".into()),
                ..AgentInfo::default()
            }
        );
    }
}
