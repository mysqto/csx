//! Terminal tabs and the AI-tool agents running in them: the shared model, the
//! OS ports every backend consumes, and row filtering/sorting.

pub mod app;
pub mod ghostty;
pub mod iterm2;
pub mod kitty;
pub mod os_shim;
pub mod process;
pub mod render;
pub mod terminal_app;
#[cfg(test)]
pub(crate) mod test_support;
pub mod tmux;
pub mod wezterm;

use crate::error::Result;
use serde::Serialize;

/// Whether an agent is working, waiting for input, or not known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    /// The agent is working.
    Busy,
    /// The agent waits for input.
    Idle,
    /// The state could not be determined.
    #[default]
    Unknown,
}

/// An AI-tool agent process found on a tty.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct AgentInfo {
    /// Stable tool id, for example `claude-code`.
    pub tool: String,
    /// Process id of the agent.
    pub pid: u32,
    /// Controlling tty without `/dev/`, for example `ttys012`.
    pub tty: Option<String>,
    /// Working directory of the agent.
    pub cwd: Option<String>,
    /// Session id of the agent, when known.
    pub session_id: Option<String>,
    /// Session title, when known.
    pub title: Option<String>,
    /// Last user prompt, when known.
    pub last_prompt: Option<String>,
    /// Busy, idle or unknown.
    pub status: AgentStatus,
    /// Last activity time in unix milliseconds.
    pub updated_at: Option<i64>,
    /// Command that resumes the session.
    pub resume_command: Option<String>,
}

/// One terminal tab, optionally with an agent running in it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Tab {
    /// Terminal id: `ghostty`, `iterm2`, `terminal`, `kitty`, `wezterm` or `tmux`.
    pub terminal: String,
    /// Backend-issued reference used to focus the tab.
    #[serde(rename = "ref")]
    pub ref_: Option<String>,
    /// Tab title.
    pub title: String,
    /// Working directory of the tab.
    pub cwd: Option<String>,
    /// Controlling tty without `/dev/`, for example `ttys012`.
    pub tty: Option<String>,
    /// 1-based window order.
    pub window: u32,
    /// 1-based tab order in the window.
    pub index: u32,
    /// Agent running in the tab, if any.
    pub agent: Option<AgentInfo>,
}

/// A row is a [`Tab`]. An agent with no tab is a tab with terminal `""`, the
/// agent's tty and cwd copied in, and `ref_` set to `tty:<tty>` or `None`.
pub type Row = Tab;

/// The result of listing tabs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct TabsReport {
    /// Rows, sorted for display.
    pub rows: Vec<Row>,
    /// Non-fatal problems met while listing.
    pub warnings: Vec<String>,
}

/// Port: run a JXA (JavaScript for Automation) script.
pub trait OsaRunner {
    /// Run `script` and return its stdout.
    fn run_jxa(&self, script: &str) -> Result<String>;
}

/// Port: run an external command.
pub trait CommandRunner {
    /// Run `program` with `args` and return its stdout.
    fn run(&self, program: &str, args: &[&str]) -> Result<String>;
}

/// Port: write bytes to a tty device.
pub trait TtyWriter {
    /// Write `bytes` to the tty named `tty` (for example `ttys012`). Callers
    /// must check [`is_tty_name`] first.
    fn write(&self, tty: &str, bytes: &[u8]) -> Result<()>;
}

/// Port: sleep.
pub trait Sleeper {
    /// Sleep for `ms` milliseconds.
    fn sleep_ms(&self, ms: u64);
}

/// The set of OS ports handed to backends.
pub struct Ports<'a> {
    /// JXA runner.
    pub osa: &'a dyn OsaRunner,
    /// Command runner.
    pub cmd: &'a dyn CommandRunner,
    /// Tty writer.
    pub tty: &'a dyn TtyWriter,
    /// Sleeper.
    pub sleep: &'a dyn Sleeper,
}

/// A terminal application that can list and focus its tabs.
pub trait TerminalBackend {
    /// Stable terminal id.
    fn id(&self) -> &'static str;
    /// List tabs. `Ok(None)` when the app is not running.
    fn list(&self, ports: &Ports) -> Result<Option<Vec<Tab>>>;
    /// Focus a tab whose ref this backend issued.
    fn focus(&self, ports: &Ports, tab_ref: &str) -> Result<()>;
    /// Focus the tab that owns `tty`. `Ok(false)` when this app has no such tab.
    fn focus_tty(&self, ports: &Ports, tty: &str) -> Result<bool>;
}

/// True when every query word (compared case-insensitively) appears in the joined searchable
/// fields: title, cwd, terminal, agent tool, agent title, last prompt and
/// session id. An empty query matches every row.
pub fn matches_query(row: &Row, words: &[String]) -> bool {
    let mut parts: Vec<&str> = vec![&row.title, &row.terminal];
    parts.extend(row.cwd.as_deref());
    if let Some(a) = &row.agent {
        parts.push(&a.tool);
        parts.extend(a.title.as_deref());
        parts.extend(a.last_prompt.as_deref());
        parts.extend(a.session_id.as_deref());
    }
    let hay = parts.join(" ").to_lowercase();
    words.iter().all(|w| hay.contains(&w.to_lowercase()))
}

/// Turn a finished command into a result: stdout (lossy UTF-8) on success, or
/// an error naming `what` with the trimmed stderr on failure.
pub fn exit_result(what: &str, success: bool, stdout: &[u8], stderr: &[u8]) -> Result<String> {
    if success {
        Ok(String::from_utf8_lossy(stdout).into_owned())
    } else {
        Err(crate::error::Error::other(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(stderr).trim()
        )))
    }
}

/// True only for `tty` followed by one or more ASCII letters or digits, for
/// example `ttys012`. Guards writes to `/dev/<tty>`.
pub fn is_tty_name(s: &str) -> bool {
    s.strip_prefix("tty")
        .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Write `bytes` to the tty `tty` through the port, after checking the name
/// with [`is_tty_name`].
pub fn write_tty(ports: &Ports, tty: &str, bytes: &[u8]) -> Result<()> {
    if !is_tty_name(tty) {
        return Err(crate::error::Error::other(format!(
            "refusing to write to invalid tty name: {tty}"
        )));
    }
    ports.tty.write(tty, bytes)
}

/// Sort rows: agent rows first, newest `updated_at` first (`None` last); then
/// tabs by (terminal, window, index).
pub fn sort_rows(rows: &mut [Row]) {
    rows.sort_by(|a, b| match (&a.agent, &b.agent) {
        (Some(x), Some(y)) => y.updated_at.cmp(&x.updated_at),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => (&a.terminal, a.window, a.index).cmp(&(&b.terminal, b.window, b.index)),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(terminal: &str, window: u32, index: u32, title: &str) -> Tab {
        Tab {
            terminal: terminal.into(),
            title: title.into(),
            window,
            index,
            ..Tab::default()
        }
    }

    fn agent_row(tool: &str, updated_at: Option<i64>) -> Row {
        Tab {
            agent: Some(AgentInfo {
                tool: tool.into(),
                updated_at,
                ..AgentInfo::default()
            }),
            ..Tab::default()
        }
    }

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn matches_query_all_words_any_field() {
        let mut r = tab("ghostty", 1, 1, "Build Server");
        r.cwd = Some("/work/csx".into());
        r.agent = Some(AgentInfo {
            tool: "claude-code".into(),
            title: Some("Fix parser".into()),
            last_prompt: Some("run the tests".into()),
            session_id: Some("abc-123".into()),
            ..AgentInfo::default()
        });
        assert!(matches_query(&r, &words("ghostty csx")));
        assert!(matches_query(&r, &words("claude parser")));
        assert!(matches_query(&r, &words("tests abc-123")));
        assert!(matches_query(&r, &words("server")));
        assert!(matches_query(&r, &words("GHOSTTY Parser")));
        assert!(!matches_query(&r, &words("csx missing")));
        let plain = tab("tmux", 1, 1, "x");
        assert!(!matches_query(&plain, &words("claude")));
    }

    #[test]
    fn matches_query_empty_matches_all() {
        assert!(matches_query(&Tab::default(), &[]));
    }

    #[test]
    fn sort_rows_agents_newest_first_then_tabs() {
        let mut rows = vec![
            tab("tmux", 1, 1, "t1"),
            agent_row("old", Some(10)),
            tab("ghostty", 2, 1, "g21"),
            agent_row("none", None),
            tab("ghostty", 1, 2, "g12"),
            agent_row("new", Some(20)),
            tab("ghostty", 1, 1, "g11"),
        ];
        sort_rows(&mut rows);
        let order: Vec<String> = rows
            .iter()
            .map(|r| match &r.agent {
                Some(a) => a.tool.clone(),
                None => r.title.clone(),
            })
            .collect();
        assert_eq!(order, vec!["new", "old", "none", "g11", "g12", "g21", "t1"]);
    }

    #[test]
    fn exit_result_success_and_failure() {
        assert_eq!(exit_result("x", true, b"out\n", b"e").unwrap(), "out\n");
        let e = exit_result("ps", false, b"o", b"  boom \n").unwrap_err();
        assert_eq!(e.to_string(), "ps failed: boom");
    }

    #[test]
    fn is_tty_name_cases() {
        assert!(is_tty_name("ttys012"));
        assert!(is_tty_name("tty1"));
        for bad in ["../x", "tty", "ttys0/1", "", "pts1", "ttys 1", "../ttys1"] {
            assert!(!is_tty_name(bad), "{bad}");
        }
    }

    #[test]
    fn write_tty_checks_name_then_writes() {
        use std::cell::RefCell;
        struct W(RefCell<Vec<(String, Vec<u8>)>>);
        impl TtyWriter for W {
            fn write(&self, tty: &str, bytes: &[u8]) -> Result<()> {
                self.0.borrow_mut().push((tty.into(), bytes.to_vec()));
                Ok(())
            }
        }
        struct N;
        impl OsaRunner for N {
            fn run_jxa(&self, _: &str) -> Result<String> {
                Ok(String::new())
            }
        }
        impl CommandRunner for N {
            fn run(&self, _: &str, _: &[&str]) -> Result<String> {
                Ok(String::new())
            }
        }
        impl Sleeper for N {
            fn sleep_ms(&self, _: u64) {}
        }
        assert!(N.run_jxa("").unwrap().is_empty());
        assert!(N.run("", &[]).unwrap().is_empty());
        N.sleep_ms(0);
        let w = W(RefCell::new(vec![]));
        let ports = Ports {
            osa: &N,
            cmd: &N,
            tty: &w,
            sleep: &N,
        };
        write_tty(&ports, "ttys001", b"hi").unwrap();
        let e = write_tty(&ports, "../etc/passwd", b"x").unwrap_err();
        assert!(e.to_string().contains("invalid tty name"));
        assert_eq!(
            w.0.borrow().as_slice(),
            &[("ttys001".to_string(), b"hi".to_vec())]
        );
    }

    #[test]
    fn serializes_ref_and_lowercase_status() {
        let t = Tab {
            ref_: Some("w1".into()),
            agent: Some(AgentInfo {
                status: AgentStatus::Busy,
                ..AgentInfo::default()
            }),
            ..Tab::default()
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["ref"], "w1");
        assert_eq!(v["agent"]["status"], "busy");
        assert_eq!(AgentInfo::default().status, AgentStatus::Unknown);
        let r = TabsReport::default();
        assert!(r.rows.is_empty() && r.warnings.is_empty());
    }
}
