//! AI-tool agent detection.

pub mod claude;
pub mod codex;
pub mod devin;
pub mod generic;

use std::path::{Path, PathBuf};

use crate::model::Origin;
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

/// True when `s` can go into a line that csx types into a terminal, inside
/// single quotes. A control character would arrive as a keystroke (Ctrl-C,
/// return) and end the line early. A backslash ends a single-quoted string
/// early in fish, where `\'` is an escape. Quotes, `$`, backticks and spaces
/// are fine: single quotes keep them literal in sh, bash, zsh and fish.
pub fn is_safe_typed_path(s: &str) -> bool {
    !s.chars().any(|c| c.is_control() || c == '\\')
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
/// the session from any folder. `None` when [`resume_command`] is `None`, or
/// when `cwd` or a value fails [`is_safe_typed_path`].
pub fn resume_line(
    tool: &str,
    session_id: &str,
    cwd: &str,
    env: &[(&str, String)],
) -> Option<String> {
    let command = resume_command(tool, session_id)?;
    let safe = is_safe_typed_path(cwd) && env.iter().all(|(_, v)| is_safe_typed_path(v));
    if !safe {
        return None;
    }
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
    /// What the command line of `p` says about who started it, for example
    /// `Some(Origin::Automated)` for a print-mode flag. The default is `None`
    /// (unknown). [`detect`] checks the process ancestry first, for every tool.
    fn origin(&self, _p: &Process) -> Option<Origin> {
        None
    }
}

/// True when one of `markers` is a flag before the first word that is not a
/// flag, or is that first word (a subcommand such as `exec` or `run`). A `--`
/// ends the check. `ps` joins the command line with spaces, so a quoted
/// prompt such as `"why does grep -p fail"` arrives as separate words; only
/// its first word is checked. A flag after a flag value (`--model opus -p`)
/// is missed, which only leaves the agent shown.
pub(crate) fn has_marker(args: &[&str], markers: &[&str]) -> bool {
    for a in args {
        if *a == "--" {
            return false;
        }
        if markers.contains(a) {
            return true;
        }
        if !a.starts_with('-') {
            return false;
        }
    }
    false
}

/// [`Origin::Automated`] when `p` has one of `markers` (see [`has_marker`]).
pub(crate) fn automated_if_marker(p: &Process, markers: &[&str]) -> Option<Origin> {
    has_marker(&p.arguments(), markers).then_some(Origin::Automated)
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

/// Who started agent process `p` of tool `agent`. Ancestry first, for every
/// tool: an agent process with another agent process above it is a
/// [`Origin::Subagent`] job. The direct parent does not count when it is the
/// same tool under an interpreter, because that is a launcher such as
/// `node .../codex`. Then the tool's own [`Agent::origin`].
fn origin_of(
    table: &ProcessTable,
    agents: &[Box<dyn Agent>],
    agent: &dyn Agent,
    p: &Process,
) -> Option<Origin> {
    let ancestors = table.ancestors(p);
    for (i, q) in ancestors.iter().enumerate() {
        let Some(owner) = agents.iter().find(|a| a.matches(q)) else {
            continue;
        };
        let launcher = i == 0 && owner.tool() == agent.tool() && q.runs_under_interpreter();
        if !launcher {
            return Some(Origin::Subagent);
        }
    }
    agent.origin(p)
}

/// One [`AgentInfo`] per matching process that has a tty (a daemon such as
/// `codex app-server` has none and is not a session), with its origin: from
/// the process (ancestry, then [`Agent::origin`]), else from
/// [`Agent::enrich`]. Per tool and tty, csx keeps one shown agent and one
/// hidden one (see [`Origin::hidden_by_default`]): the foreground one, else
/// the lowest pid. So a hidden job never takes the place of the person's
/// session, and its session still counts as running. Shown agents come
/// first, so they get the tab when agents are attached to tabs.
pub fn detect(
    table: &ProcessTable,
    agents: &[Box<dyn Agent>],
    ctx: &AgentContext,
) -> Vec<AgentInfo> {
    let mut picked: Vec<(&dyn Agent, &Process, Option<Origin>)> = Vec::new();
    for p in &table.procs {
        let Some(agent) = agents.iter().find(|a| a.matches(p)) else {
            continue;
        };
        if p.tty.is_none() {
            continue;
        }
        let origin = origin_of(table, agents, agent.as_ref(), p);
        let hidden = Origin::hidden_by_default(origin);
        let same = picked.iter().position(|(a, q, o)| {
            a.tool() == agent.tool() && q.tty == p.tty && Origin::hidden_by_default(*o) == hidden
        });
        match same {
            Some(i) if rank(p) < rank(picked[i].1) => picked[i] = (agent.as_ref(), p, origin),
            Some(_) => {}
            None => picked.push((agent.as_ref(), p, origin)),
        }
    }
    picked.sort_by_key(|(_, _, o)| Origin::hidden_by_default(*o));
    picked
        .into_iter()
        .map(|(a, p, origin)| {
            // The process wins; else whatever the tool's session data says.
            let info = a.enrich(p, ctx);
            AgentInfo {
                origin: origin.or(info.origin),
                ..info
            }
        })
        .collect()
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

    fn child(pid: u32, ppid: u32, tty: &str, args: &str) -> Process {
        Process {
            ppid,
            ..proc(pid, pid, pid, Some(tty), args)
        }
    }

    fn origins(table: &ProcessTable) -> Vec<(String, u32, Option<Origin>)> {
        detect(table, &registry(), &ctx())
            .into_iter()
            .map(|a| (a.tool, a.pid, a.origin))
            .collect()
    }

    #[test]
    fn detect_marks_agents_started_by_agents_as_subagents() {
        let table = ProcessTable {
            procs: vec![
                child(1, 0, "ttys1", "/bin/zsh"),
                child(10, 1, "ttys1", "claude"),
                child(11, 10, "ttys1", "/bin/sh -c review"),
                child(12, 11, "ttys1", "python3 -m reviewer"),
                child(13, 12, "ttys1", "claude --output-format stream-json"),
                child(20, 1, "ttys2", "codex"),
                child(21, 20, "ttys2", "gemini"),
            ],
        };
        assert_eq!(
            origins(&table),
            vec![
                ("claude-code".into(), 10, None),
                ("codex".into(), 20, None),
                ("claude-code".into(), 13, Some(Origin::Subagent)),
                ("gemini".into(), 21, Some(Origin::Subagent)),
            ]
        );
    }

    #[test]
    fn detect_keeps_a_lone_hidden_agent_and_prefers_shown_ones() {
        // On ttys3 a print-mode claude that the person's claude started has
        // the lower pid. Both stay: the person's session first, so it gets
        // the tab, and the job after it, so its session still counts as
        // running. On ttys5 a hidden job alone on its tty stays too.
        let table = ProcessTable {
            procs: vec![
                child(5, 50, "ttys3", "claude -p review"),
                child(50, 1, "ttys3", "claude"),
                child(60, 1, "ttys4", "claude"),
                child(61, 60, "ttys5", "claude -p x"),
            ],
        };
        assert_eq!(
            origins(&table),
            vec![
                ("claude-code".into(), 50, None),
                ("claude-code".into(), 60, None),
                ("claude-code".into(), 5, Some(Origin::Subagent)),
                ("claude-code".into(), 61, Some(Origin::Subagent)),
            ]
        );
    }

    #[test]
    fn detect_skips_a_same_tool_launcher_parent() {
        let table = ProcessTable {
            procs: vec![
                child(30, 1, "ttys6", "node /opt/homebrew/bin/codex"),
                child(31, 30, "ttys6", "/opt/codex/codex-aarch64"),
                child(40, 1, "ttys7", "node /opt/bin/codex"),
                child(41, 40, "ttys7", "node /opt/bin/claude"),
                child(50, 1, "ttys8", "codex"),
                child(51, 50, "ttys8", "codex exec x"),
            ],
        };
        let r = registry();
        let codex = r.iter().find(|a| a.tool() == "codex").unwrap();
        let claude = r.iter().find(|a| a.tool() == "claude-code").unwrap();
        let of =
            |agent: &dyn Agent, pid: u32| origin_of(&table, &r, agent, table.get(pid).unwrap());
        // The codex that a node launcher starts is the person's session.
        assert_eq!(of(codex.as_ref(), 31), None);
        // Claude under a codex launcher: another tool, so a subagent.
        assert_eq!(of(claude.as_ref(), 41), Some(Origin::Subagent));
        // A codex that a native codex started is a job, not a launch.
        assert_eq!(of(codex.as_ref(), 51), Some(Origin::Subagent));
    }

    #[test]
    fn has_marker_checks_flags_then_the_first_word_only() {
        assert!(has_marker(&["-p", "x"], &["-p"]));
        assert!(has_marker(&["--verbose", "-p", "x"], &["-p"]));
        assert!(!has_marker(&["--", "-p"], &["-p"]));
        assert!(!has_marker(&["why", "does", "grep", "-p", "fail"], &["-p"]));
        assert!(has_marker(&["--full-auto", "exec", "x"], &["exec"]));
        assert!(!has_marker(&["resume", "exec"], &["exec"]));
        assert!(!has_marker(&["-v"], &["-p"]));
        assert!(!has_marker(&["x"], &[]));
    }

    #[test]
    fn tools_read_print_mode_from_the_command_line() {
        let r = registry();
        let o = |tool: &str, args: &str| {
            let a = r.iter().find(|a| a.tool() == tool).unwrap();
            a.origin(&proc(1, 1, 1, None, args))
        };
        assert_eq!(o("claude-code", "claude -p hi"), Some(Origin::Automated));
        assert_eq!(o("claude-code", "claude --print"), Some(Origin::Automated));
        assert_eq!(o("claude-code", "claude --resume x"), None);
        assert_eq!(o("claude-code", "claude -- -p"), None);
        assert_eq!(o("claude-code", "claude why does grep -p fail"), None);
        assert_eq!(o("devin", "devin -p -- hi"), Some(Origin::Automated));
        assert_eq!(o("devin", "devin"), None);
        assert_eq!(o("codex", "codex exec fix"), Some(Origin::Automated));
        assert_eq!(
            o("codex", "codex --full-auto e fix"),
            Some(Origin::Automated)
        );
        assert_eq!(o("codex", "codex resume abc"), None);
        assert_eq!(o("codex", "codex"), None);
        assert_eq!(o("gemini", "gemini -p hi"), None);
        let marked = generic::NamedTool {
            tool: "t",
            programs: &["t"],
            print_markers: &["run"],
        };
        assert_eq!(
            marked.origin(&proc(1, 1, 1, None, "t run x")),
            Some(Origin::Automated)
        );
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

    /// Paths that would break out of the typed line: a control character
    /// arrives as a keystroke (Ctrl-C, return), and a backslash ends a fish
    /// single-quoted string early.
    const UNSAFE: &[&str] = &[
        "/w/a\x03b",
        "/w/a\nrm -rf ~",
        "/w/a\rb",
        "/w/a\x1b[2Jb",
        "/w/trailing\\",
        "/w/mid\\dle",
    ];

    #[test]
    fn is_safe_typed_path_rejects_control_chars_and_backslashes() {
        for bad in UNSAFE {
            assert!(!is_safe_typed_path(bad), "{bad:?}");
        }
        for good in ["/Users/me/it's here", "/w/$HOME `x` \"q\"", "/w/café", ""] {
            assert!(is_safe_typed_path(good), "{good:?}");
        }
    }

    #[test]
    fn resume_line_refuses_unsafe_folders_and_env_values() {
        for bad in UNSAFE {
            assert_eq!(resume_line("codex", U, bad, &[]), None, "{bad:?}");
            let env = [("CODEX_HOME", bad.to_string())];
            assert_eq!(resume_line("codex", U, "/w", &env), None, "{bad:?}");
        }
        assert_eq!(
            resume_line("codex", U, "/w/$x `y`", &[("CODEX_HOME", "/c/it's".into())]),
            Some(format!(
                "cd '/w/$x `y`' && CODEX_HOME='/c/it'\\''s' codex resume {U}"
            ))
        );
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
